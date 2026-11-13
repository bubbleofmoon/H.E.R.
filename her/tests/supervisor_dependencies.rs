//! Seam 1: real supervisor/CLI and disposable fake dependency programs.
#[path = "../src/install_root.rs"]
mod install_root;
#[path = "../src/process_identity.rs"]
mod process_identity;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.env_remove("HER_RUNTIME_ROOT");
    command
}

struct Owned {
    child: Child,
    identity: process_identity::ProcessIdentity,
}
impl Owned {
    fn new(mut child: Child) -> Self {
        let inherited_image = process_identity::current_identity().unwrap().exe;
        let deadline = Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Some(found) = process_identity::identity_of(child.id()).unwrap() {
                // Linux can briefly expose the forked harness image after spawn returns.
                // Pin cleanup only after the native child has completed its exec transition.
                if found.exe != inherited_image {
                    break found;
                }
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture child exited before exec identity was recorded"
            );
            assert!(
                Instant::now() < deadline,
                "fixture child did not complete exec"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        Self { child, identity }
    }
    fn stop(&mut self) {
        let outcome = process_identity::kill_verified(&self.identity);
        assert!(
            matches!(
                outcome,
                process_identity::KillOutcome::Killed | process_identity::KillOutcome::Gone
            ),
            "{outcome:?}; expected={:?}; current={:?}",
            self.identity,
            process_identity::identity_of(self.child.id())
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "owned child did not exit: {:?}",
                self.identity
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

struct Fixture {
    root: PathBuf,
    supervisor: Option<Owned>,
    console: Option<Owned>,
    config: Value,
    ports: [u16; 3],
    image: PathBuf,
    cli: PathBuf,
    install_root: Option<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
                "her-deps-中文 it's {}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        for dir in [
            "config",
            "data/lifecycle",
            "resources",
            "snow",
            "dsh",
            "user/dsh",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let reservations: Vec<_> = (0..3)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports = std::array::from_fn(|i| reservations[i].local_addr().unwrap().port());
        let node = hidden(if cfg!(windows) { "where.exe" } else { "which" })
            .arg("node")
            .output()
            .unwrap();
        assert!(node.status.success(), "Node fixture prerequisite");
        let node = PathBuf::from(
            String::from_utf8(node.stdout)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .trim(),
        );
        std::fs::copy(
            &node,
            root.join(if cfg!(windows) {
                "snow/node.exe"
            } else {
                "snow/node"
            }),
        )
        .unwrap();
        std::fs::copy(
            &node,
            root.join(if cfg!(windows) {
                "dsh/node.exe"
            } else {
                "dsh/node"
            }),
        )
        .unwrap();
        let script = r#"
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
const cfg=JSON.parse(fs.readFileSync(new URL('./fixture.json',import.meta.url)));
if(cfg.dsh && path.resolve(process.env.DSH_HOME||'')!==path.resolve(cfg.home)) throw Error('wrong DSH_HOME');
if(!cfg.dsh && process.env.SNOWLUMA_HOOK_AUTOLOAD!=='1') throw Error('missing SnowLuma hook');
const server=http.createServer((req,res)=>{
 let request='';req.on('data',chunk=>request+=chunk);
 req.on('end',()=>{
  console.log(req.method+' '+req.url);
  if(req.url.includes('send_private_msg')) {
   if(fs.existsSync(new URL('./reject-alert',import.meta.url))) {
    fs.appendFileSync(new URL('./alert-refusals',import.meta.url),'refused\n');
    res.writeHead(401,{'Content-Length':0,'Connection':'close'});res.end();return;
   }
   fs.appendFileSync(new URL('./alerts',import.meta.url),request+'\n');
   if(fs.existsSync(new URL('./lose-alert-reply',import.meta.url))) {req.socket.destroy();return;}
  }
  const body=JSON.stringify(cfg.dsh ? {type:'server-response',rpcId:JSON.parse(request||'{}').rpcId,result:{ok:true,value:{}}} : {status:'ok',retcode:0,data:{online:true,good:true}});
  res.writeHead(200,{'Content-Length':Buffer.byteLength(body),'Connection':'close'});res.end(body);
 });
});
fs.writeFileSync(new URL('./booting',import.meta.url),String(process.pid));
console.log('BIRTH '+process.pid);
const attempts=new URL('./attempts',import.meta.url);
const first=!fs.existsSync(attempts);
fs.appendFileSync(attempts,process.pid+'\n');
if(cfg.failFirst && first) process.exit(17);
let hung=false;
const control=setInterval(()=>{
 if(fs.existsSync(new URL('./exit',import.meta.url))) process.exit(18);
 if(fs.existsSync(new URL('./hang',import.meta.url)) && !hung){
  hung=true;control.ref();server.close(()=>fs.writeFileSync(new URL('./hung',import.meta.url),'closed'));
 } else if(!fs.existsSync(new URL('./hang',import.meta.url)) && hung){
  hung=false;server.listen(cfg.port,'127.0.0.1');control.unref();
 }
},25);control.unref();
setTimeout(()=>server.listen(cfg.port,'127.0.0.1',()=>fs.appendFileSync(new URL('./starts',import.meta.url),process.pid+'\n')),cfg.delay||0);
setTimeout(()=>server.close(()=>process.exit(0)),300000).unref();
"#;
        for (i, name) in ["snow", "dsh"].iter().enumerate() {
            std::fs::write(root.join(name).join("index.mjs"), script).unwrap();
            std::fs::write(
                root.join(name).join("fixture.json"),
                json!({"dsh":i==1,"port":ports[i],"home":root.join("user/dsh")}).to_string(),
            )
            .unwrap();
        }
        let config = json!({"consoleToken":"fixture-console","update":{"policy":"off"},"consolePort":ports[2],"snowluma":{"accessToken":"fixture-snow"},
            "backends":{"mcpRustBridge":false},"rollout":{"rustGroups":[]},
            "dependencies":{
                "snowluma":{"program":root.join(if cfg!(windows) {"snow/node.exe"} else {"snow/node"}),"script":root.join("snow/index.mjs"),"workingDirectory":root.join("snow")},
                "dsh":{"program":root.join(if cfg!(windows) {"dsh/node.exe"} else {"dsh/node"}),"script":root.join("dsh/index.mjs"),"workingDirectory":root.join("dsh")}},
            "tasks":{"watchdog":{"enabled":false},"memory-digest":{"enabled":false},"memory-digest-check":{"enabled":false},"backup":{"enabled":false}}});
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        std::fs::write(root.join("dependencies.json"), json!({"onebot":format!("http://127.0.0.1:{}",ports[0]),"dsh":format!("http://127.0.0.1:{}",ports[1]),"model":"http://127.0.0.1:1"}).to_string()).unwrap();
        drop(reservations);
        let fixture = Self {
            root,
            supervisor: None,
            console: None,
            config,
            ports,
            image: PathBuf::from(env!("CARGO_BIN_EXE_her")),
            cli: PathBuf::from(env!("CARGO_BIN_EXE_her-cli")),
            install_root: None,
        };
        // A real host task (or Linux's unsupported native scheduler) can raise
        // an unrelated startup notice. Give every replacement the same matching
        // file task without filtering any sends out of the outage assertions.
        let task = fixture
            .command(&fixture.cli)
            .arg("install")
            .output()
            .unwrap();
        assert!(task.status.success(), "fixture scheduler: {task:?}");
        fixture
    }
    fn relay(&mut self, mode: &str) -> PathBuf {
        let folder = self.root.join("deps/relay/8.0.36");
        std::fs::create_dir_all(folder.join("auth")).unwrap();
        let program = folder.join(if cfg!(windows) { "relay.exe" } else { "relay" });
        std::fs::copy(
            self.config["dependencies"]["dsh"]["program"]
                .as_str()
                .unwrap(),
            &program,
        )
        .unwrap();
        std::fs::write(folder.join("index.mjs"), include_str!("fixtures/relay.mjs")).unwrap();
        std::fs::write(folder.join("client-key"), "synthetic-relay-secret-80").unwrap();
        std::fs::write(folder.join("auth/login.json"), "synthetic-private-login").unwrap();
        std::fs::write(folder.join("mode"), mode).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(
            folder.join("fixture.json"),
            json!({"port":port}).to_string(),
        )
        .unwrap();
        self.config["dependencies"]["directory"] = json!(self.root.join("deps"));
        self.config["dependencies"]["relay"] = json!({
            "program":program, "arguments":[folder.join("index.mjs")],
            "workingDirectory":folder, "version":"8.0.36",
            "loginDirectory":folder.join("auth"), "clientKeyFile":folder.join("client-key")
        });
        std::fs::write(
            self.root.join("config/config.json"),
            self.config.to_string(),
        )
        .unwrap();
        let path = self.root.join("dependencies.json");
        let mut endpoints: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        endpoints["relay"] = json!(format!("http://127.0.0.1:{port}"));
        std::fs::write(path, endpoints.to_string()).unwrap();
        folder
    }
    fn command(&self, image: &std::path::Path) -> Command {
        let mut command = hidden(image);
        command
            .env("HER_TASK_SCHEDULER_FILE", self.root.join("scheduler.xml"))
            .env(
                "HER_INSTALL_ROOT",
                self.install_root
                    .clone()
                    .unwrap_or_else(|| self.root.join("install-boundary")),
            );
        #[cfg(feature = "parity-clock")]
        if self.root.join("data/lifecycle/dependency-clock").is_file() {
            command.env("HER_TEST_DEPENDENCY_CLOCK", "1");
        }
        command
    }
    fn spawn_her(&self, args: &[&str]) -> Owned {
        Owned::new(
            self.command(&self.image)
                .args(args)
                .arg("--runtime-root")
                .arg(&self.root)
                .stdout(Stdio::from(
                    std::fs::File::create(self.root.join(format!("{}.out", args[0]))).unwrap(),
                ))
                .stderr(Stdio::from(
                    std::fs::File::create(self.root.join(format!("{}.err", args[0]))).unwrap(),
                ))
                .spawn()
                .unwrap(),
        )
    }
    fn start(&mut self) {
        if self.console.is_none() {
            self.console = Some(self.spawn_her(&[
                "--console-serve",
                "--console-port",
                &self.ports[2].to_string(),
            ]));
        }
        self.supervisor = Some(self.spawn_her(&["supervise"]));
    }
    fn healthy(&self) {
        let output = self
            .command(&self.cli)
            .args([
                "lifecycle",
                "assert-running",
                "--wait",
                "15s",
                "--runtime-root",
            ])
            .arg(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "health={} states={} supervisor={}",
            String::from_utf8_lossy(&output.stdout),
            self.states(),
            std::fs::read_to_string(self.root.join("supervise.err")).unwrap_or_default()
        );
        // HTTP readiness can precede the manager's post-spawn identity checkpoint.
        self.wait(|| {
            let states = self.states();
            ["snowluma", "dsh"].iter().all(|name| {
                states[name]["identity"].is_object()
                    && states[name]["healthySince"].is_number()
                    && matches!(states[name]["state"].as_str(), Some("started" | "adopted"))
            })
        });
    }
    fn states(&self) -> Value {
        let path = self.root.join("data/lifecycle/dependencies.json");
        for attempt in 0..20 {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    return serde_json::from_slice(&bytes)
                        .expect("dependency snapshot must be valid JSON")
                }
                Err(error) if attempt == 19 && error.kind() == std::io::ErrorKind::NotFound => {
                    return json!({})
                }
                Err(error) if attempt == 19 => panic!("dependency snapshot unavailable: {error}"),
                // Windows may temporarily refuse opening a leaf being replaced.
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        unreachable!()
    }
    fn dependency(&self, name: &str) -> Owned {
        let config = &self.config["dependencies"][name];
        let mut command = hidden(config["program"].as_str().unwrap());
        if let Some(arguments) = config["arguments"].as_array() {
            command.args(arguments.iter().map(|arg| arg.as_str().unwrap()));
        } else {
            command.arg(config["script"].as_str().unwrap());
        }
        command
            .current_dir(config["workingDirectory"].as_str().unwrap())
            .env("DSH_HOME", self.root.join("user/dsh"))
            .env("SNOWLUMA_HOOK_AUTOLOAD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Owned::new(command.spawn().unwrap())
    }
    fn wait(&self, predicate: impl Fn() -> bool) {
        self.wait_for(Duration::from_secs(15), predicate);
    }
    fn wait_for(&self, timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "fixture wait expired; states={} supervisor={}",
                self.states(),
                std::fs::read_to_string(self.root.join("supervise.err")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn fake_config(&self, folder: &str, field: &str, value: Value) {
        let path = self.root.join(folder).join("fixture.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config[field] = value;
        std::fs::write(path, config.to_string()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.supervisor.take();
        self.console.take();
        for name in ["snowluma", "relay", "dsh"] {
            if let Ok(identity) = serde_json::from_value::<process_identity::ProcessIdentity>(
                self.states()[name]["identity"].clone(),
            ) {
                if matches!(
                    process_identity::check_identity(&identity),
                    process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
                ) {
                    continue;
                }
                let outcome = process_identity::kill_verified(&identity);
                assert!(
                    matches!(
                        outcome,
                        process_identity::KillOutcome::Killed | process_identity::KillOutcome::Gone
                    ),
                    "cleanup {name}: {outcome:?}"
                );
                let deadline = Instant::now() + Duration::from_secs(10);
                while !matches!(
                    process_identity::check_identity(&identity),
                    process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
                ) {
                    assert!(Instant::now() < deadline, "pinned {name} did not exit");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(feature = "parity-clock")]
impl Fixture {
    fn clock(&self, now: i64) {
        // Atomic fixture clock publication, read only by feature-enabled children.
        let path = self.root.join("data/lifecycle/dependency-clock");
        std::fs::write(path.with_extension("tmp"), now.to_string()).unwrap();
        std::fs::rename(path.with_extension("tmp"), path).unwrap();
    }
    fn attempts(&self, folder: &str) -> usize {
        std::fs::read_to_string(self.root.join(folder).join("attempts"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn alerts(&self) -> usize {
        std::fs::read_to_string(self.root.join("snow/alerts"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn enable_alerts(&mut self) {
        self.config["ownerQQ"] = json!(42);
        std::fs::write(
            self.root.join("config/config.json"),
            self.config.to_string(),
        )
        .unwrap();
    }
    fn crash_dsh(&self) {
        let instance = serde_json::from_value::<process_identity::ProcessIdentity>(
            self.states()["dsh"]["identity"].clone(),
        )
        .unwrap();
        assert_eq!(
            process_identity::kill_verified(&instance),
            process_identity::KillOutcome::Killed
        );
        self.wait(|| self.states()["dsh"]["state"] == "backoff");
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn dead_dependency_obeys_5_30_60_second_backoff_across_supervisor_replacement() {
    let mut fixture = Fixture::new();
    let mut now = 1_800_000_000_000i64;
    fixture.clock(now);
    fixture.start();
    fixture.healthy();
    let sibling = fixture.states()["snowluma"]["identity"].clone();
    for (index, delay) in [5_000, 30_000, 60_000, 60_000].into_iter().enumerate() {
        let instance: process_identity::ProcessIdentity =
            serde_json::from_value(fixture.states()["dsh"]["identity"].clone()).unwrap();
        assert_eq!(
            process_identity::kill_verified(&instance),
            process_identity::KillOutcome::Killed
        );
        fixture.wait(|| fixture.states()["dsh"]["state"] == "backoff");
        assert_eq!(fixture.states()["dsh"]["restartAt"], now + delay);
        if index == 1 {
            fixture.supervisor.take();
            fixture.start();
            std::thread::sleep(Duration::from_millis(500));
        }
        fixture.clock(now + delay - 1);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(fixture.attempts("dsh"), index + 1, "must not restart early");
        now += delay;
        fixture.clock(now);
        fixture.wait(|| fixture.attempts("dsh") == index + 2);
        fixture.wait(|| fixture.states()["dsh"]["state"] == "started");
        fixture.healthy();
        assert_eq!(fixture.states()["snowluma"]["identity"], sibling);
    }
}

#[cfg(feature = "parity-clock")]
fn restored_backoff_after_clock_correction(correction: i64) {
    let mut fixture = Fixture::new();
    fixture.enable_alerts();
    let epoch = 1_800_000_000_000i64;
    fixture.clock(epoch);
    fixture.start();
    fixture.healthy();
    fixture.crash_dsh();
    fixture.clock(epoch + 5_000);
    fixture.wait(|| fixture.attempts("dsh") == 2);
    fixture.healthy();
    fixture.crash_dsh();
    fixture.supervisor.take();
    let now = epoch + 5_000 + correction;
    fixture.clock(now);
    fixture.start();
    fixture.wait(|| fixture.states()["snowluma"]["state"] == "adopted");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fixture.attempts("dsh"),
        2,
        "clock correction must not erase pending backoff"
    );
    assert_eq!(
        fixture.states()["dsh"]["restartAt"],
        now + 30_000,
        "restored wait must be bounded by the current backoff step"
    );
    fixture.clock(now + 29_999);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fixture.attempts("dsh"), 2);
    fixture.clock(now + 30_000);
    fixture.wait(|| fixture.attempts("dsh") == 3);
    fixture.healthy();
    fixture.crash_dsh();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fixture.states()["dsh"]["failures"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "future or expired failures cannot count toward a new outage"
    );
    assert_eq!(fixture.alerts(), 0);
}

#[cfg(feature = "parity-clock")]
#[test]
fn restored_backoff_survives_forward_clock_correction() {
    restored_backoff_after_clock_correction(3_600_000);
}

#[cfg(feature = "parity-clock")]
#[test]
fn restored_backoff_survives_backward_clock_correction() {
    restored_backoff_after_clock_correction(-3_600_000);
}

#[cfg(feature = "parity-clock")]
#[test]
fn restored_hang_timer_keeps_observed_refusal_duration_across_clock_corrections() {
    for correction in [3_600_000, -3_600_000] {
        let mut fixture = Fixture::new();
        let epoch = 1_800_000_000_000i64;
        fixture.clock(epoch);
        fixture.start();
        fixture.healthy();
        let identity = fixture.states()["dsh"]["identity"].clone();
        std::fs::write(fixture.root.join("dsh/hang"), "").unwrap();
        fixture.wait(|| fixture.states()["dsh"]["refusalSince"] == epoch);
        fixture.clock(epoch + 60_000);
        fixture.wait(|| fixture.states()["dsh"]["timerAt"] == epoch + 60_000);
        fixture.supervisor.take();
        let now = epoch + 60_000 + correction;
        fixture.clock(now);
        fixture.start();
        fixture.wait(|| {
            fixture.states()["dsh"]["state"] == "adopted"
                && fixture.states()["dsh"]["timerAt"] == now
        });
        fixture.clock(now + 59_999);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(fixture.states()["dsh"]["identity"], identity);
        assert_eq!(
            process_identity::check_identity(&serde_json::from_value(identity).unwrap()),
            process_identity::Liveness::Matches,
            "must not kill before two observed refusal minutes"
        );
        fixture.clock(now + 60_000);
        fixture.wait(|| fixture.states()["dsh"]["state"] == "backoff");
        assert_eq!(fixture.states()["dsh"]["restartAt"], now + 65_000);
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn restored_healthy_timer_keeps_observed_recovery_duration_across_clock_corrections() {
    for correction in [3_600_000, -3_600_000] {
        let mut fixture = Fixture::new();
        let epoch = 1_800_000_000_000i64;
        fixture.clock(epoch);
        fixture.start();
        fixture.healthy();
        fixture.crash_dsh();
        fixture.clock(epoch + 5_000);
        fixture.healthy();
        fixture.clock(epoch + 305_000);
        fixture.wait(|| fixture.states()["dsh"]["timerAt"] == epoch + 305_000);
        fixture.supervisor.take();
        let now = epoch + 305_000 + correction;
        fixture.clock(now);
        fixture.start();
        fixture.wait(|| {
            fixture.states()["dsh"]["state"] == "adopted"
                && fixture.states()["dsh"]["timerAt"] == now
        });
        assert_eq!(fixture.states()["dsh"]["restartStep"], 1);
        fixture.clock(now + 299_999);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            fixture.states()["dsh"]["restartStep"],
            1,
            "must not clear outage early"
        );
        fixture.clock(now + 300_000);
        fixture.wait(|| fixture.states()["dsh"]["restartStep"] == 0);
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn supervisor_startup_with_ready_gateway_does_not_send_an_outage_notice() {
    // Drop the fixture's supervisor before the separately guarded fake peers.
    let (_snow, _dsh);
    let mut fixture = Fixture::new();
    fixture.enable_alerts();
    fixture.clock(1_800_000_000_000);
    // Make startup notices deterministic instead of racing the gateway's boot.
    _snow = fixture.dependency("snowluma");
    _dsh = fixture.dependency("dsh");
    fixture.wait(|| fixture.root.join("snow/starts").is_file());
    fixture.wait(|| fixture.root.join("dsh/starts").is_file());
    fixture.start();
    fixture.healthy();
    fixture.wait(|| {
        std::fs::read(fixture.root.join("data/qq-login-episode.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|episode| episode["state"] == "logged-in")
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fixture.alerts(),
        0,
        "startup and login polling must leave outage notice counts untouched: {}",
        std::fs::read_to_string(fixture.root.join("snow/alerts")).unwrap_or_default()
    );
    let records = std::fs::read_to_string(fixture.root.join("data/run-record.jsonl")).unwrap();
    assert!(
        records
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|record| record["event"] == "scheduled-task-check" && record["outcome"] == "ok"),
        "the disposable scheduler must match: {records}"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn dependency_failure_alert_is_once_per_outage_even_after_supervisor_replacement() {
    let mut fixture = Fixture::new();
    fixture.enable_alerts();
    let mut now = 1_800_000_000_000i64;
    fixture.clock(now);
    fixture.start();
    fixture.healthy();
    for (index, delay) in [5_000, 30_000, 60_000, 60_000, 60_000]
        .into_iter()
        .enumerate()
    {
        fixture.crash_dsh();
        if index < 2 {
            assert_eq!(fixture.alerts(), 0);
        } else {
            fixture.wait(|| fixture.alerts() == 1);
        }
        if index == 2 {
            fixture.supervisor.take();
            fixture.start();
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(fixture.alerts(), 1);
        }
        now += delay;
        fixture.clock(now);
        fixture.wait(|| fixture.attempts("dsh") == index + 2);
        fixture.healthy();
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fixture.alerts(), 1);
    // A sustained recovery allows one notice for a subsequent outage.
    now += 600_000;
    fixture.clock(now);
    fixture.wait(|| fixture.states()["dsh"]["restartStep"] == 0);
    for (index, delay) in [5_000, 30_000, 60_000].into_iter().enumerate() {
        fixture.crash_dsh();
        if index < 2 {
            assert_eq!(fixture.alerts(), 1);
        } else {
            fixture.wait(|| fixture.alerts() == 2);
        }
        now += delay;
        fixture.clock(now);
        fixture.wait(|| fixture.attempts("dsh") == index + 7);
        fixture.healthy();
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn dependency_failure_alert_uses_a_sliding_ten_minute_window() {
    let mut fixture = Fixture::new();
    fixture.enable_alerts();
    let epoch = 1_800_000_000_000i64;
    fixture.clock(epoch);
    fixture.start();
    fixture.healthy();
    for (index, next) in [epoch + 5_000, epoch + 35_000].into_iter().enumerate() {
        fixture.crash_dsh();
        fixture.clock(next);
        fixture.wait(|| fixture.attempts("dsh") == index + 2);
        fixture.healthy();
    }
    fixture.clock(epoch + 610_000);
    fixture.crash_dsh();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fixture.alerts(),
        0,
        "expired failures must not trigger an outage alert; states={} alerts={}",
        fixture.states(),
        std::fs::read_to_string(fixture.root.join("snow/alerts")).unwrap_or_default()
    );
    assert_eq!(
        fixture.states()["dsh"]["failures"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn snowluma_crash_loop_delivers_one_pending_alert_after_recovery_and_replacement() {
    for replace in [false, true] {
        let mut fixture = Fixture::new();
        fixture.enable_alerts();
        let mut now = 1_800_000_000_000i64;
        fixture.clock(now);
        fixture.start();
        fixture.healthy();
        for (index, delay) in [5_000, 30_000, 60_000, 60_000].into_iter().enumerate() {
            let instance = serde_json::from_value::<process_identity::ProcessIdentity>(
                fixture.states()["snowluma"]["identity"].clone(),
            )
            .unwrap();
            assert_eq!(
                process_identity::kill_verified(&instance),
                process_identity::KillOutcome::Killed
            );
            fixture.wait(|| fixture.states()["snowluma"]["state"] == "backoff");
            if index == 2 {
                std::thread::sleep(Duration::from_millis(500));
                assert_eq!(fixture.alerts(), 0, "gateway is down");
                let health = fixture
                    .command(&fixture.cli)
                    .args(["lifecycle", "assert-running", "--runtime-root"])
                    .arg(&fixture.root)
                    .output()
                    .unwrap();
                let health: Value = serde_json::from_slice(&health.stdout).unwrap();
                assert_eq!(health["dependencyNotices"][0]["status"], "pending");
                assert!(
                    std::fs::read_to_string(fixture.root.join("data/run-record.jsonl"))
                        .unwrap()
                        .contains("pending administrator notice")
                );
                if replace {
                    fixture.supervisor.take();
                    fixture.start();
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            now += delay;
            fixture.clock(now);
            fixture.wait(|| fixture.attempts("snow") == index + 2);
            fixture.healthy();
            if index >= 2 {
                fixture.wait(|| fixture.alerts() == 1);
            }
        }
        fixture.supervisor.take();
        fixture.start();
        fixture.healthy();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(fixture.alerts(), 1, "replacement must preserve suppression");
    }
}

#[cfg(feature = "parity-clock")]
fn outage_notice_after_send_failure(unknown: bool) {
    let mut fixture = Fixture::new();
    fixture.enable_alerts();
    let mut now = 1_800_000_000_000i64;
    fixture.clock(now);
    fixture.start();
    fixture.healthy();
    let control = fixture.root.join(if unknown {
        "snow/lose-alert-reply"
    } else {
        "snow/reject-alert"
    });
    std::fs::write(&control, "").unwrap();
    for (index, delay) in [5_000, 30_000, 60_000].into_iter().enumerate() {
        fixture.crash_dsh();
        if index == 2 {
            break;
        }
        now += delay;
        fixture.clock(now);
        fixture.wait(|| fixture.attempts("dsh") == index + 2);
        fixture.healthy();
    }
    if unknown {
        fixture.wait(|| fixture.states()["dsh"]["outageNotice"]["status"] == "unknown");
        assert_eq!(
            fixture.alerts(),
            1,
            "request arrived but the reply was lost"
        );
    } else {
        fixture.wait(|| fixture.root.join("snow/alert-refusals").is_file());
        fixture.wait(|| fixture.states()["dsh"]["outageNotice"]["status"] == "pending");
        assert_eq!(fixture.alerts(), 0, "401 refused the send action");
    }
    std::fs::remove_file(control).unwrap();
    fixture.supervisor.take();
    // Make the replacement's first timer checkpoint distinguishable from the old one.
    now += 1;
    fixture.clock(now);
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["timerAt"] == now);
    if !unknown {
        fixture.wait(|| fixture.alerts() == 1);
        fixture.wait(|| fixture.states()["dsh"]["outageNotice"]["status"] == "sent");
    }
    now += 60_000;
    fixture.clock(now);
    fixture.healthy();
    fixture.supervisor.take();
    fixture.start();
    fixture.healthy();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fixture.alerts(),
        1,
        "replacement must never duplicate a delivered notice"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn outage_notice_retries_a_known_pre_send_refusal_after_replacement() {
    outage_notice_after_send_failure(false);
}

#[cfg(feature = "parity-clock")]
#[test]
fn outage_notice_never_retries_a_lost_reply_even_after_replacement() {
    outage_notice_after_send_failure(true);
}

#[cfg(feature = "parity-clock")]
#[test]
fn live_dependency_restarts_only_after_two_minutes_of_continuous_port_refusal() {
    let mut fixture = Fixture::new();
    let epoch = 1_800_000_000_000i64;
    fixture.clock(epoch);
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["healthySince"].is_number());
    let before = fixture.states();
    let old: process_identity::ProcessIdentity =
        serde_json::from_value(before["dsh"]["identity"].clone()).unwrap();
    let hang = fixture.root.join("dsh/hang");
    std::fs::write(&hang, "close owned fake listener").unwrap();
    fixture.wait(|| fixture.root.join("dsh/hung").exists());
    fixture.wait(|| fixture.states()["dsh"]["refusalSince"] == epoch);
    fixture.clock(epoch + 119_999);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        process_identity::check_identity(&old),
        process_identity::Liveness::Matches
    );
    assert_eq!(fixture.attempts("dsh"), 1);
    // A reachable endpoint resets the continuous-refusal interval.
    std::fs::remove_file(&hang).unwrap();
    fixture.wait(|| fixture.states()["dsh"]["refusalSince"].is_null());
    std::fs::write(&hang, "hang again").unwrap();
    fixture.wait(|| fixture.states()["dsh"]["refusalSince"] == epoch + 119_999);
    fixture.clock(epoch + 120_000);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        process_identity::check_identity(&old),
        process_identity::Liveness::Matches
    );
    fixture.clock(epoch + 239_998);
    // Replacement can restore only the refusal duration actually checkpointed.
    fixture.wait(|| fixture.states()["dsh"]["timerAt"] == epoch + 239_998);
    assert_eq!(
        process_identity::check_identity(&old),
        process_identity::Liveness::Matches
    );
    // The refusal interval survives supervisor replacement too.
    fixture.supervisor.take();
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    fixture.clock(epoch + 239_999);
    fixture.wait(|| fixture.states()["dsh"]["state"] == "backoff");
    fixture.wait(|| process_identity::check_identity(&old) == process_identity::Liveness::Gone);
    assert_eq!(fixture.states()["dsh"]["restartAt"], epoch + 244_999);
    assert_eq!(
        fixture.states()["snowluma"]["identity"],
        before["snowluma"]["identity"]
    );
    std::fs::remove_file(hang).unwrap();
    fixture.clock(epoch + 244_999);
    fixture.wait(|| fixture.attempts("dsh") == 2);
    fixture.healthy();
    assert_ne!(
        fixture.states()["dsh"]["identity"],
        before["dsh"]["identity"]
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn explicit_stop_preserves_dependencies_unless_with_dependencies_is_requested() {
    for adopted in [false, true] {
        for with_dependencies in [false, true] {
            let mut fixture = Fixture::new();
            fixture.clock(1_800_000_000_000);
            let _adopted =
                adopted.then(|| [fixture.dependency("snowluma"), fixture.dependency("dsh")]);
            if adopted {
                fixture.wait(|| {
                    fixture.root.join("snow/starts").exists()
                        && fixture.root.join("dsh/starts").exists()
                });
            }
            fixture.start();
            fixture.healthy();
            let before = fixture.states();
            let console = fixture.console.as_ref().unwrap().identity.clone();
            let mut command = fixture.command(&fixture.cli);
            command
                .args(["stop", "--confirm", "--runtime-root"])
                .arg(&fixture.root);
            if with_dependencies {
                command.arg("--with-dependencies");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "adopted={adopted} with_dependencies={with_dependencies}: {output:?}"
            );
            fixture.wait(|| {
                process_identity::check_identity(&console) == process_identity::Liveness::Gone
            });
            for name in ["snowluma", "dsh"] {
                let instance = serde_json::from_value::<process_identity::ProcessIdentity>(
                    before[name]["identity"].clone(),
                )
                .unwrap();
                let expected = if with_dependencies {
                    process_identity::Liveness::Gone
                } else {
                    process_identity::Liveness::Matches
                };
                fixture.wait(|| process_identity::check_identity(&instance) == expected);
            }
            fixture.clock(1_800_001_000_000);
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(fixture.attempts("snow"), 1);
            assert_eq!(fixture.attempts("dsh"), 1);
            if with_dependencies {
                let resume = fixture
                    .command(&fixture.cli)
                    .args(["lifecycle", "set", "running", "--runtime-root"])
                    .arg(&fixture.root)
                    .output()
                    .unwrap();
                assert!(resume.status.success(), "{resume:?}");
                fixture.console.take();
                fixture.supervisor.take();
                fixture.start();
                fixture.healthy();
                assert_eq!(fixture.attempts("snow"), 2);
                assert_eq!(fixture.attempts("dsh"), 2);
            }
        }
    }
}

#[cfg(all(windows, feature = "parity-clock"))]
#[test]
fn explicit_stop_and_hang_recovery_never_kill_uninspectable_dependencies() {
    let mut fixture = Fixture::new();
    let epoch = 1_800_000_000_000i64;
    fixture.clock(epoch);
    let snow = fixture.dependency("snowluma");
    let dsh = fixture.dependency("dsh");
    fixture.wait(|| {
        fixture.root.join("snow/starts").exists() && fixture.root.join("dsh/starts").exists()
    });
    fixture.start();
    fixture.healthy();
    std::fs::write(fixture.root.join("dsh/hang"), "close fake listener").unwrap();
    fixture.wait(|| fixture.states()["dsh"]["refusalSince"] == epoch);
    deny_full_inspection(&dsh);
    fixture.clock(epoch + 120_000);
    fixture.wait(|| fixture.states()["dsh"]["state"] == "refused");
    assert!(fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .contains("cannot inspect elevated or access-restricted dependency"));
    let output = fixture
        .command(&fixture.cli)
        .args(["stop", "--with-dependencies", "--confirm", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    for instance in [&snow, &dsh] {
        assert_eq!(
            process_identity::check_identity(&instance.identity),
            process_identity::Liveness::Matches
        );
    }
    assert_eq!(fixture.attempts("dsh"), 1);
}

#[cfg(feature = "parity-clock")]
#[test]
fn explicit_stop_refuses_a_foreign_port_before_stopping_any_dependency() {
    let mut fixture = Fixture::new();
    fixture.clock(1_800_000_000_000);
    fixture.start();
    fixture.healthy();
    let snow: process_identity::ProcessIdentity =
        serde_json::from_value(fixture.states()["snowluma"]["identity"].clone()).unwrap();
    fixture.crash_dsh();
    let foreign = std::net::TcpListener::bind(("127.0.0.1", fixture.ports[1])).unwrap();
    let output = fixture
        .command(&fixture.cli)
        .args(["stop", "--with-dependencies", "--confirm", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown process"),
        "{output:?}"
    );
    assert_eq!(
        process_identity::check_identity(&snow),
        process_identity::Liveness::Matches
    );
    assert!(std::net::TcpStream::connect(foreign.local_addr().unwrap()).is_ok());
}

#[test]
fn snowluma_failure_stops_dsh_startup_until_later_reconciliation() {
    for failure in ["refusal", "exit", "timeout"] {
        let mut fixture = Fixture::new();
        #[cfg(feature = "parity-clock")]
        fixture.clock(1_800_000_000_000);
        let foreign = if failure == "refusal" {
            Some(std::net::TcpListener::bind(("127.0.0.1", fixture.ports[0])).unwrap())
        } else {
            None
        };
        if failure == "exit" {
            fixture.fake_config("snow", "failFirst", json!(true));
        } else if failure == "timeout" {
            fixture.fake_config("snow", "delay", json!(120000));
        }
        fixture.start();
        #[cfg(feature = "parity-clock")]
        if failure == "timeout" {
            fixture.wait(|| fixture.root.join("snow/booting").exists());
            fixture.clock(1_800_000_090_000);
        }
        fixture.wait_for(Duration::from_secs(110), || {
            fixture.states()["snowluma"]["state"]
                == if failure == "exit" {
                    "backoff"
                } else {
                    "refused"
                }
        });
        let reason = fixture.states()["snowluma"]["reason"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            reason.contains(match failure {
                "refusal" => "taken by an unknown process",
                "exit" => "exited",
                _ => "did not listen within 90 seconds",
            }),
            "{failure}: {reason}"
        );
        // Observe another pass while refusal/backoff still forbids initial DSH startup.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !fixture.root.join("dsh/booting").exists(),
            "{failure}: DSH started after SnowLuma failed"
        );
        assert!(
            fixture.states()["dsh"].is_null(),
            "{failure}: {}",
            fixture.states()
        );
        drop(foreign);
    }
}

fn initial_failure_is_retried(name: &str, folder: &str) {
    let mut fixture = Fixture::new();
    #[cfg(feature = "parity-clock")]
    fixture.clock(1_800_000_000_000);
    fixture.fake_config(folder, "failFirst", json!(true));
    fixture.start();
    fixture.wait(|| fixture.states()[name]["state"] == "backoff");
    assert!(
        !fixture.root.join(folder).join("starts").exists(),
        "first launch must fail before readiness"
    );
    let failed = fixture.states()[name]["identity"].clone();
    #[cfg(feature = "parity-clock")]
    fixture.clock(1_800_000_005_000);
    fixture.wait_for(Duration::from_secs(80), || {
        fixture.states()[name]["state"] == "started" && fixture.states()[name]["identity"] != failed
    });
    fixture.healthy();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("attempts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn snowluma_retries_after_initial_startup_failure() {
    initial_failure_is_retried("snowluma", "snow");
}

#[test]
fn dsh_retries_after_initial_startup_failure() {
    initial_failure_is_retried("dsh", "dsh");
}

fn running_dependency_recovers(name: &str, folder: &str, sibling: &str) {
    let mut fixture = Fixture::new();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["identity"].is_object());
    let before = fixture.states();
    let failed: process_identity::ProcessIdentity =
        serde_json::from_value(before[name]["identity"].clone()).unwrap();
    let exit = fixture.root.join(folder).join("exit");
    std::fs::write(&exit, "exit owned fake").unwrap();
    fixture.wait(|| process_identity::check_identity(&failed) == process_identity::Liveness::Gone);
    std::fs::remove_file(exit).unwrap();
    fixture.wait_for(Duration::from_secs(80), || {
        let states = fixture.states();
        states[name]["state"] == "started"
            && states[name]["identity"].is_object()
            && states[name]["identity"] != before[name]["identity"]
    });
    fixture.healthy();
    assert_eq!(fixture.states()[name]["state"], "started");
    assert_eq!(
        fixture.states()[sibling]["identity"],
        before[sibling]["identity"],
        "healthy sibling must survive recovery"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("attempts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn snowluma_recovers_after_a_previously_running_dependency_exits() {
    running_dependency_recovers("snowluma", "snow", "dsh");
}

#[test]
fn dsh_recovers_after_a_previously_running_dependency_exits() {
    running_dependency_recovers("dsh", "dsh", "snowluma");
}

fn immediately_exiting_adoption_recovers(name: &str, folder: &str) {
    let mut fixture = Fixture::new();
    #[cfg(feature = "parity-clock")]
    fixture.clock(1_800_000_000_000);
    // Hold readiness so the seam can observe adoption and then request immediate exit.
    fixture.fake_config(folder, "delay", json!(10000));
    let mut adopted = fixture.dependency(name);
    fixture.wait(|| fixture.root.join(folder).join("booting").exists());
    fixture.start();
    fixture.wait(|| fixture.states()[name]["state"] == "adopted");
    assert_eq!(
        fixture.states()[name]["identity"]["pid"],
        adopted.identity.pid
    );
    assert_eq!(
        fixture.states()[name]["identity"]["start"],
        adopted.identity.start
    );
    std::fs::write(
        fixture.root.join(folder).join("exit"),
        "exit just after adoption",
    )
    .unwrap();
    fixture.wait(|| {
        process_identity::check_identity(&adopted.identity) == process_identity::Liveness::Gone
    });
    assert_eq!(adopted.child.wait().unwrap().code(), Some(18));
    fixture.wait(|| fixture.states()[name]["state"] == "backoff");
    assert!(fixture.states()[name]["reason"]
        .as_str()
        .unwrap()
        .contains("exited"));
    assert!(!fixture.root.join(folder).join("starts").exists());
    std::fs::remove_file(fixture.root.join(folder).join("exit")).unwrap();
    fixture.fake_config(folder, "delay", json!(0));
    #[cfg(feature = "parity-clock")]
    fixture.clock(1_800_000_005_000);
    fixture.wait_for(Duration::from_secs(80), || {
        fixture.states()[name]["state"] == "started"
    });
    fixture.healthy();
    assert_ne!(fixture.states()[name]["identity"], json!(adopted.identity));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("attempts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn snowluma_recovers_after_exit_immediately_after_adoption() {
    immediately_exiting_adoption_recovers("snowluma", "snow");
}

#[test]
fn dsh_recovers_after_exit_immediately_after_adoption() {
    immediately_exiting_adoption_recovers("dsh", "dsh");
}

fn listening_adoption_recovers(name: &str, folder: &str) {
    let mut fixture = Fixture::new();
    let mut adopted = fixture.dependency(name);
    fixture.wait(|| fixture.root.join(folder).join("starts").exists());
    fixture.start();
    fixture.wait(|| fixture.states()[name]["state"] == "adopted");
    assert_eq!(fixture.states()[name]["identity"], json!(adopted.identity));
    let exit = fixture.root.join(folder).join("exit");
    std::fs::write(&exit, "exit listening fake immediately after adoption").unwrap();
    fixture.wait(|| {
        process_identity::check_identity(&adopted.identity) == process_identity::Liveness::Gone
    });
    assert_eq!(adopted.child.wait().unwrap().code(), Some(18));
    std::fs::remove_file(exit).unwrap();
    fixture.wait_for(Duration::from_secs(80), || {
        fixture.states()[name]["state"] == "started"
    });
    fixture.healthy();
    assert_ne!(fixture.states()[name]["identity"], json!(adopted.identity));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("attempts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(folder).join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn snowluma_recovers_after_a_listening_dependency_exits_immediately_after_adoption() {
    listening_adoption_recovers("snowluma", "snow");
}

#[test]
fn dsh_recovers_after_a_listening_dependency_exits_immediately_after_adoption() {
    listening_adoption_recovers("dsh", "dsh");
}

#[test]
fn cold_start_launches_both_configured_dependencies_and_health_passes() {
    let mut fixture = Fixture::new();
    let closed = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "assert-running", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert_eq!(closed.status.code(), Some(1));
    let verdict: Value = serde_json::from_slice(&closed.stdout).unwrap();
    for name in ["snowluma", "dsh"] {
        assert_eq!(
            verdict["parts"][name]["verdict"], "fail",
            "closed selected dependency ports must be down: {verdict}"
        );
    }
    fixture.start();
    fixture.healthy();
    for name in ["snowluma", "dsh"] {
        assert_eq!(fixture.states()[name]["state"], "started");
    }
}

#[test]
fn adopts_existing_dependencies_and_readopts_them_after_supervisor_stop() {
    let mut fixture = Fixture::new();
    let snow = fixture.dependency("snowluma");
    let dsh = fixture.dependency("dsh");
    fixture.wait(|| {
        fixture.root.join("snow/starts").exists() && fixture.root.join("dsh/starts").exists()
    });
    fixture.start();
    fixture.healthy();
    fixture.wait(|| {
        fixture.states()["snowluma"]["state"] == "adopted"
            && fixture.states()["dsh"]["state"] == "adopted"
    });
    for (name, instance) in [("snowluma", &snow), ("dsh", &dsh)] {
        assert_eq!(
            fixture.states()[name]["identity"]["pid"],
            instance.identity.pid
        );
        assert_eq!(
            fixture.states()[name]["identity"]["start"],
            instance.identity.start
        );
    }
    fixture.supervisor.take();
    for instance in [&snow, &dsh] {
        assert_eq!(
            process_identity::check_identity(&instance.identity),
            process_identity::Liveness::Matches
        );
    }
    // Adoption must be established from the OS even without the previous report.
    std::fs::remove_file(fixture.root.join("data/lifecycle/dependencies.json")).unwrap();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["identity"]["pid"] == dsh.identity.pid);
    for (name, folder, instance) in [("snowluma", "snow", &snow), ("dsh", "dsh", &dsh)] {
        assert_eq!(fixture.states()[name]["state"], "adopted");
        assert_eq!(
            fixture.states()[name]["identity"]["start"],
            instance.identity.start
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(folder).join("starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[test]
fn adopts_matching_dependencies_that_are_still_starting_without_duplicates() {
    let mut fixture = Fixture::new();
    for folder in ["snow", "dsh"] {
        let path = fixture.root.join(folder).join("fixture.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["delay"] = json!(if folder == "snow" { 2000 } else { 6000 });
        std::fs::write(path, config.to_string()).unwrap();
    }
    let snow = fixture.dependency("snowluma");
    let dsh = fixture.dependency("dsh");
    fixture.wait(|| {
        fixture.root.join("snow/booting").exists() && fixture.root.join("dsh/booting").exists()
    });
    fixture.start();
    fixture.wait(|| fixture.states()["snowluma"]["identity"]["pid"].is_number());
    assert_eq!(
        fixture.states()["snowluma"]["identity"]["pid"],
        snow.identity.pid,
        "must wait for the matching booting process, never launch a duplicate"
    );
    fixture.healthy();
    assert_eq!(fixture.states()["dsh"]["identity"]["pid"], dsh.identity.pid);
    for folder in ["snow", "dsh"] {
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(folder).join("starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[test]
fn foreign_port_holders_are_alerted_and_left_running() {
    for (name, folder) in [("snowluma", "snow"), ("dsh", "dsh")] {
        let mut fixture = Fixture::new();
        fixture.config["ownerQQ"] = json!(42);
        std::fs::write(
            fixture.root.join("config/config.json"),
            fixture.config.to_string(),
        )
        .unwrap();
        let foreign_dir = fixture.root.join("foreign");
        std::fs::create_dir(&foreign_dir).unwrap();
        for file in ["index.mjs", "fixture.json"] {
            std::fs::copy(fixture.root.join(folder).join(file), foreign_dir.join(file)).unwrap();
        }
        let foreign = Owned::new(
            hidden(
                fixture.config["dependencies"][name]["program"]
                    .as_str()
                    .unwrap(),
            )
            .arg(foreign_dir.join("index.mjs"))
            .current_dir(&foreign_dir)
            .env("DSH_HOME", fixture.root.join("user/dsh"))
            .env("SNOWLUMA_HOOK_AUTOLOAD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        );
        fixture.wait(|| foreign_dir.join("starts").exists());
        fixture.start();
        fixture.wait(|| fixture.states()[name]["state"] == "refused");
        let reason = fixture.states()[name]["reason"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            reason.contains("port") && reason.contains("taken by an unknown process"),
            "{reason}"
        );
        assert_eq!(
            process_identity::check_identity(&foreign.identity),
            process_identity::Liveness::Matches
        );
        assert_eq!(
            std::fs::read_to_string(foreign_dir.join("starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(
            !fixture.root.join(folder).join("starts").exists(),
            "foreign holder must prevent a configured duplicate"
        );
        let alert = if name == "snowluma" {
            foreign_dir.join("alerts")
        } else {
            fixture.root.join("snow/alerts")
        };
        fixture.wait(|| {
            std::fs::read_to_string(&alert)
                .unwrap_or_default()
                .contains("taken by an unknown process")
        });
    }
}

#[cfg(windows)]
fn deny_full_inspection(dsh: &Owned) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn SetKernelObjectSecurity(
            handle: *mut c_void,
            information: u32,
            descriptor: *const c_void,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    // Model the native access boundary of an elevated process, using only our
    // fake: VM_READ is denied while limited-query liveness and termination work.
    let text: Vec<u16> = "D:(D;;0x0010;;;WD)(A;;GA;;;OW)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    unsafe {
        assert_ne!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut()
            ),
            0
        );
        let changed = SetKernelObjectSecurity(dsh.child.as_raw_handle(), 4, descriptor);
        LocalFree(descriptor);
        assert_ne!(
            changed,
            0,
            "set fake process DACL: {}",
            std::io::Error::last_os_error()
        );
    }
    assert!(
        process_identity::identify(dsh.identity.pid).is_err(),
        "native full inspection must actually be denied"
    );
}

#[cfg(windows)]
#[test]
fn denied_inspection_reports_an_elevated_or_access_restricted_dependency_and_leaves_it_alone() {
    let mut fixture = Fixture::new();
    let dsh = fixture.dependency("dsh");
    fixture.wait(|| fixture.root.join("dsh/starts").exists());
    deny_full_inspection(&dsh);
    assert_eq!(
        process_identity::check_identity(&dsh.identity),
        process_identity::Liveness::Matches
    );
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "refused");
    assert!(fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .contains("cannot inspect elevated or access-restricted dependency"));
    assert_eq!(
        process_identity::check_identity(&dsh.identity),
        process_identity::Liveness::Matches
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("dsh/starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[cfg(windows)]
#[test]
fn an_uninspectable_foreign_node_image_does_not_block_configured_dependency_startup() {
    let mut fixture = Fixture::new();
    let foreign_dir = fixture.root.join("foreign");
    std::fs::create_dir(&foreign_dir).unwrap();
    let program = foreign_dir.join("node.exe");
    std::fs::copy(
        fixture.config["dependencies"]["dsh"]["program"]
            .as_str()
            .unwrap(),
        &program,
    )
    .unwrap();
    let script = foreign_dir.join("unrelated.js");
    std::fs::write(&script, "setInterval(()=>{},1000);").unwrap();
    let foreign = Owned::new(
        hidden(&program)
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    deny_full_inspection(&foreign);
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "started");
    assert_eq!(
        process_identity::check_identity(&foreign.identity),
        process_identity::Liveness::Matches
    );
}

#[test]
fn dependencies_started_by_the_supervisor_survive_cli_stop_and_are_readopted() {
    let mut fixture = Fixture::new();
    fixture.start();
    fixture.healthy();
    let before = fixture.states();
    for args in [
        vec!["lifecycle", "set", "stopped", "--confirm", "--runtime-root"],
        vec![
            "lifecycle",
            "stop-supervisor",
            "--confirm",
            "--runtime-root",
        ],
    ] {
        let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
            .args(args)
            .arg(&fixture.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "selected CLI stop: {output:?}");
    }
    fixture.supervisor.take();
    fixture.console.take();
    for name in ["snowluma", "dsh"] {
        let instance = serde_json::from_value::<process_identity::ProcessIdentity>(
            before[name]["identity"].clone(),
        )
        .unwrap();
        assert_eq!(
            process_identity::check_identity(&instance),
            process_identity::Liveness::Matches
        );
    }
    let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "set", "running", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "selected CLI resume: {output:?}");
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    for name in ["snowluma", "dsh"] {
        assert_eq!(fixture.states()[name]["identity"], before[name]["identity"]);
    }
}

#[test]
fn start_all_helper_and_task_entry_are_retired() {
    let fixture = Fixture::new();
    let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["tasks", "list", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!list["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|task| task["id"] == "start-all"));
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for helper in [
        "start-all.ps1",
        "start-all.bat",
        "start-all-task.bat",
        "启动QQ机器人.bat",
    ] {
        assert!(
            !repo.join(helper).exists(),
            "retired startup helper remains: {helper}"
        );
    }
    #[cfg(windows)]
    {
        let output = hidden("powershell.exe")
            .args(["-NoProfile", "-File"])
            .arg(repo.join("scripts/test-ps-count.ps1"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "maintained PowerShell static verification: {output:?}"
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    }
}

struct Wrapper {
    process: Owned,
    children: std::sync::Arc<std::sync::Mutex<Vec<process_identity::ProcessIdentity>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Wrapper {
    fn new(fixture: &Fixture, delay: Option<u64>, entry: PathBuf, via_cmd: bool) -> Self {
        Self::with_program(
            fixture,
            delay,
            entry,
            via_cmd,
            fixture.config["dependencies"]["dsh"]["program"]
                .as_str()
                .unwrap(),
        )
    }
    fn with_program(
        fixture: &Fixture,
        delay: Option<u64>,
        entry: PathBuf,
        via_cmd: bool,
        program: &str,
    ) -> Self {
        use std::io::BufRead;
        let root = &fixture.root;
        let wrapper = root.join("npx-cli.js");
        std::fs::write(&wrapper,r#"
const fs=require('fs'),cp=require('child_process');
const cfg=JSON.parse(fs.readFileSync(process.argv[3]));
fs.writeFileSync(cfg.ready,String(process.pid));
if(cfg.delay!==null) setTimeout(()=>{
 const args=cfg.cmd ? ['/d','/s','/c','""'+cfg.batch+'""'] : [cfg.entry];
 const child=cp.spawn(cfg.cmd ? process.env.ComSpec : process.execPath,args,{cwd:cfg.cwd,env:process.env,windowsHide:true,windowsVerbatimArguments:cfg.cmd,stdio:['ignore','inherit','inherit']});
 console.log('CHILD '+child.pid);
 child.on('exit',()=>process.exit(0));
},cfg.delay);
setInterval(()=>{},1000);
"#).unwrap();
        let cfg = root.join("wrapper.json");
        let batch = root.join("launch-entry.cmd");
        if via_cmd && cfg!(windows) {
            std::fs::write(
                &batch,
                format!(
                    "@echo off\r\nchcp 65001 >nul\r\n\"{program}\" \"{}\"\r\n",
                    entry.display()
                ),
            )
            .unwrap();
        }
        std::fs::write(&cfg,json!({"delay":delay,"entry":entry,"cwd":root.join("dsh"),"cmd":via_cmd && cfg!(windows),"batch":batch,"ready":root.join("npx-ready")}).to_string()).unwrap();
        let mut process = Owned::new(
            hidden(program)
                .arg(&wrapper)
                .arg("@deepseek-ai/dsh@fixture")
                .arg(cfg)
                .env("DSH_HOME", root.join("user/dsh"))
                .env("SNOWLUMA_HOOK_AUTOLOAD", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let out = process.child.stdout.take().unwrap();
        let children = std::sync::Arc::new(std::sync::Mutex::new(Vec::<
            process_identity::ProcessIdentity,
        >::new()));
        let records = children.clone();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let Some(pid) = line
                    .strip_prefix("CHILD ")
                    .or_else(|| line.strip_prefix("BIRTH "))
                    .and_then(|s| s.parse().ok())
                else {
                    continue;
                };
                if let Ok(Some(instance)) = process_identity::identity_of(pid) {
                    let mut records = records.lock().unwrap();
                    if !records.iter().any(|p| p == &instance) {
                        records.push(instance);
                    }
                }
            }
        });
        fixture.wait(|| root.join("npx-ready").exists());
        Self {
            process,
            children,
            reader: Some(reader),
        }
    }
}
impl Drop for Wrapper {
    fn drop(&mut self) {
        if self.process.child.try_wait().unwrap().is_none() {
            self.process.stop();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for instance in self.children.lock().unwrap().iter() {
                if matches!(
                    process_identity::check_identity(instance),
                    process_identity::Liveness::Matches
                ) {
                    let outcome = process_identity::kill_verified(instance);
                    assert!(
                        matches!(
                            outcome,
                            process_identity::KillOutcome::Killed
                                | process_identity::KillOutcome::Gone
                        ),
                        "wrapper cleanup: {outcome:?}"
                    );
                }
            }
            if self.reader.as_ref().unwrap().is_finished() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "wrapper output still held by a fixture child"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.reader.take().unwrap().join().unwrap();
        for instance in self.children.lock().unwrap().iter() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(
                process_identity::check_identity(instance),
                process_identity::Liveness::Matches
            ) {
                assert!(Instant::now() < deadline, "wrapper child still alive");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn pending_npx_wrapper_resolves_through_cmd_before_dsh_is_adopted() {
    let mut fixture = Fixture::new();
    let wrapper = Wrapper::new(
        &fixture,
        Some(6000),
        fixture.root.join("dsh/index.mjs"),
        true,
    );
    fixture.start();
    fixture.wait(|| fixture.states()["snowluma"]["identity"].is_object());
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !fixture.root.join("dsh/booting").exists(),
        "unresolved npx wrapper must defer direct DSH launch"
    );
    fixture.healthy();
    fixture.wait(|| !wrapper.children.lock().unwrap().is_empty());
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    let state = fixture.states();
    assert_eq!(state["dsh"]["state"], "adopted");
    assert_ne!(
        state["dsh"]["identity"]["pid"], wrapper.process.identity.pid,
        "the wrapper itself is never adopted"
    );
    assert!(wrapper
        .children
        .lock()
        .unwrap()
        .iter()
        .any(|p| state["dsh"]["identity"]["pid"] == p.pid
            && state["dsh"]["identity"]["start"] == p.start));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("dsh/starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let identity = state["dsh"]["identity"].clone();
    fixture.supervisor.take();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    assert_eq!(fixture.states()["dsh"]["identity"], identity);
    assert_eq!(
        process_identity::check_identity(&wrapper.process.identity),
        process_identity::Liveness::Matches
    );
}

#[test]
fn matching_script_from_another_node_installation_is_never_adopted() {
    let mut fixture = Fixture::new();
    let foreign = Owned::new(
        hidden(
            fixture.config["dependencies"]["snowluma"]["program"]
                .as_str()
                .unwrap(),
        )
        .arg(fixture.root.join("dsh/index.mjs"))
        .env("DSH_HOME", fixture.root.join("user/dsh"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    fixture.wait(|| fixture.root.join("dsh/starts").exists());
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "refused");
    assert!(fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .contains("taken by an unknown process"));
    assert_eq!(
        process_identity::check_identity(&foreign.identity),
        process_identity::Liveness::Matches
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("dsh/starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn unresolved_npx_wrapper_using_another_node_installation_defers_and_refuses_launch() {
    let mut fixture = Fixture::new();
    let wrapper = Wrapper::with_program(
        &fixture,
        None,
        fixture.root.join("dsh/index.mjs"),
        false,
        fixture.config["dependencies"]["snowluma"]["program"]
            .as_str()
            .unwrap(),
    );
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "waiting");
    let waiting = Instant::now();
    let deadline = waiting + Duration::from_secs(40);
    while fixture.states()["dsh"]["state"] != "refused" {
        assert!(Instant::now() < deadline, "unbounded wrapper deferral");
        assert!(!fixture.root.join("dsh/booting").exists());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(waiting.elapsed() >= Duration::from_secs(29));
    assert!(fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .contains("did not resolve within 30 seconds"));
    assert_eq!(
        process_identity::check_identity(&wrapper.process.identity),
        process_identity::Liveness::Matches
    );
    assert!(wrapper.children.lock().unwrap().is_empty());
}

#[test]
fn unresolved_npx_wrapper_is_refused_after_30_seconds_without_launch_or_stop() {
    let mut fixture = Fixture::new();
    let wrapper = Wrapper::new(&fixture, None, fixture.root.join("dsh/index.mjs"), false);
    let started = Instant::now();
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "waiting");
    let deadline = Instant::now() + Duration::from_secs(40);
    while fixture.states()["dsh"]["state"] != "refused" {
        assert!(Instant::now() < deadline, "unbounded wrapper deferral");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert!(fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .contains("did not resolve within 30 seconds"));
    assert!(!fixture.root.join("dsh/booting").exists());
    assert_eq!(
        process_identity::check_identity(&wrapper.process.identity),
        process_identity::Liveness::Matches
    );
    assert!(wrapper.children.lock().unwrap().is_empty());
}

#[test]
fn resolved_foreign_npx_entries_are_preserved_and_refused_only_on_the_selected_port() {
    for selected_port in [false, true] {
        let mut fixture = Fixture::new();
        let foreign = fixture.root.join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::copy(
            fixture.root.join("dsh/index.mjs"),
            foreign.join("index.mjs"),
        )
        .unwrap();
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(fixture.root.join("dsh/fixture.json")).unwrap())
                .unwrap();
        if !selected_port {
            let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config["port"] = json!(reserve.local_addr().unwrap().port());
        }
        std::fs::write(foreign.join("fixture.json"), config.to_string()).unwrap();
        let wrapper = Wrapper::new(&fixture, Some(3000), foreign.join("index.mjs"), true);
        if selected_port {
            // Port-holder refusal requires a listener to exist before the launch
            // decision; resolving a foreign script does not predict its future port.
            fixture.wait(|| foreign.join("starts").exists());
        }
        fixture.start();
        fixture.wait(|| foreign.join("starts").exists());
        if selected_port {
            fixture.wait(|| fixture.states()["dsh"]["state"] == "refused");
            assert!(fixture.states()["dsh"]["reason"]
                .as_str()
                .unwrap()
                .contains("taken by an unknown process"));
            assert!(!fixture.root.join("dsh/booting").exists());
        } else {
            fixture.healthy();
            assert_eq!(fixture.states()["dsh"]["state"], "started");
        }
        fixture.wait(|| {
            wrapper.children.lock().unwrap().iter().any(|p| {
                p.exe
                    .to_lowercase()
                    .ends_with(if cfg!(windows) { "node.exe" } else { "node" })
            })
        });
        assert_eq!(
            process_identity::check_identity(&wrapper.process.identity),
            process_identity::Liveness::Matches
        );
        for child in wrapper.children.lock().unwrap().iter() {
            assert_eq!(
                process_identity::check_identity(child),
                process_identity::Liveness::Matches
            );
        }
        assert_eq!(
            std::fs::read_to_string(foreign.join("starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[cfg(windows)]
#[test]
fn native_dependency_actions_serialize_with_the_runtime_mutex() {
    let mut fixture = Fixture::new();
    let script = fixture.root.join("hold-mutex.ps1");
    std::fs::write(&script,r#"
param($Root)
$ErrorActionPreference='Stop'
$sha=[Security.Cryptography.SHA256]::Create()
try {$key=[BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes([IO.Path]::GetFullPath($Root).ToLowerInvariant()))).Replace('-','')}
finally {$sha.Dispose()}
$mutex=[Threading.Mutex]::new($false,('Global\QQBot-StartAll-'+$key))
$acquired=$mutex.WaitOne(0)
if(-not $acquired){throw 'fixture mutex already held'}
try {
 [IO.File]::WriteAllText((Join-Path $Root 'mutex-ready'),'ready')
 Start-Sleep -Seconds 4
} finally {$mutex.ReleaseMutex();$mutex.Dispose()}
"#).unwrap();
    let mut holder = Owned::new(
        hidden("powershell.exe")
            .args(["-NoProfile", "-File"])
            .arg(script)
            .arg(&fixture.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    fixture.wait(|| fixture.root.join("mutex-ready").exists());
    fixture.start();
    fixture.wait(|| {
        std::fs::read_to_string(fixture.root.join("supervise.err"))
            .unwrap_or_default()
            .contains("already running")
    });
    for folder in ["snow", "dsh"] {
        assert!(!fixture.root.join(folder).join("booting").exists());
    }
    fixture.supervisor.take();
    assert!(holder.child.wait().unwrap().success());
    fixture.start();
    fixture.healthy();
}

#[cfg(windows)]
#[test]
fn supervisor_and_dependencies_have_no_sampled_visible_windows_during_startup() {
    use std::ffi::c_void;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(*mut c_void, isize) -> i32>,
            data: isize,
        ) -> i32;
        fn IsWindowVisible(window: *mut c_void) -> i32;
        fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    }
    unsafe extern "system" fn collect(window: *mut c_void, data: isize) -> i32 {
        if IsWindowVisible(window) != 0 {
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            (*(data as *mut Vec<u32>)).push(pid);
        }
        1
    }
    struct Observer {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Observer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(std::collections::BTreeSet::<u32>::new()));
    let stopping = stop.clone();
    let observed = samples.clone();
    let observer = Observer {
        stop,
        thread: Some(std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut pids = Vec::<u32>::new();
                assert_ne!(
                    unsafe { EnumWindows(Some(collect), &mut pids as *mut _ as isize) },
                    0,
                    "native window enumeration"
                );
                observed.lock().unwrap().extend(pids);
                std::thread::sleep(Duration::from_millis(10));
            }
        })),
    };
    let mut fixture = Fixture::new();
    fixture.start();
    fixture.healthy();
    drop(observer);
    let states = fixture.states();
    let pids = [
        fixture.supervisor.as_ref().unwrap().identity.pid,
        fixture.console.as_ref().unwrap().identity.pid,
        states["snowluma"]["identity"]["pid"].as_u64().unwrap() as u32,
        states["dsh"]["identity"]["pid"].as_u64().unwrap() as u32,
    ];
    for pid in pids {
        assert!(
            !samples.lock().unwrap().contains(&pid),
            "fixture process {pid} had a visible window"
        );
    }
    eprintln!("window enumeration passed at 10 ms sampling; shorter windows can be missed");
}

#[test]
fn stopped_or_unknown_desired_state_never_starts_dependencies_from_either_image() {
    for image in [env!("CARGO_BIN_EXE_her"), env!("CARGO_BIN_EXE_her-cli")] {
        for state in ["stopped", "unknown"] {
            let mut fixture = Fixture::new();
            if state == "stopped" {
                let result = hidden(env!("CARGO_BIN_EXE_her-cli"))
                    .args(["lifecycle", "set", "stopped", "--confirm", "--runtime-root"])
                    .arg(&fixture.root)
                    .output()
                    .unwrap();
                assert!(result.status.success(), "{result:?}");
            } else {
                std::fs::write(
                    fixture.root.join("data/lifecycle/desired-state.json"),
                    "invalid fixture desired state",
                )
                .unwrap();
            }
            fixture.supervisor = Some(Owned::new(
                hidden(image)
                    .args(["supervise", "--runtime-root"])
                    .arg(&fixture.root)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            ));
            fixture.wait(|| {
                let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
                    .args(["lifecycle", "status", "--runtime-root"])
                    .arg(&fixture.root)
                    .output()
                    .unwrap();
                serde_json::from_slice::<Value>(&output.stdout)
                    .ok()
                    .is_some_and(|v| v["owners"]["supervisor"]["state"] == "held")
            });
            std::thread::sleep(Duration::from_millis(300));
            for folder in ["snow", "dsh"] {
                assert!(
                    !fixture.root.join(folder).join("booting").exists(),
                    "{image}: dependency started while {state}"
                );
            }
            assert!(fixture.states().as_object().unwrap().is_empty());
        }
    }
}

#[test]
fn an_installed_release_starts_dependencies_inside_a_throwaway_install_root() {
    let mut fixture = Fixture::new();
    let release = fixture.root.join("releases/fixture");
    std::fs::create_dir_all(&release).unwrap();
    fixture.image = release.join(if cfg!(windows) { "her.exe" } else { "her" });
    fixture.cli = release.join(if cfg!(windows) {
        "her-cli.exe"
    } else {
        "her-cli"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_her"), &fixture.image).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_her-cli"), &fixture.cli).unwrap();
    fixture.install_root = Some(fixture.root.clone());
    fixture.start();
    fixture.healthy();
    for name in ["snowluma", "dsh"] {
        assert_eq!(fixture.states()[name]["state"], "started");
    }
}

#[test]
fn a_development_image_refuses_dependency_paths_inside_a_protected_install_root() {
    for field in ["program", "script", "workingDirectory"] {
        let mut fixture = Fixture::new();
        let protected = fixture.root.join("protected-install");
        std::fs::create_dir(&protected).unwrap();
        let path = match field {
            "program" => {
                let path = protected.join(if cfg!(windows) { "node.exe" } else { "node" });
                std::fs::copy(
                    fixture.config["dependencies"]["snowluma"]["program"]
                        .as_str()
                        .unwrap(),
                    &path,
                )
                .unwrap();
                path
            }
            "script" => {
                for file in ["index.mjs", "fixture.json"] {
                    std::fs::copy(fixture.root.join("snow").join(file), protected.join(file))
                        .unwrap();
                }
                protected.join("index.mjs")
            }
            _ => protected.clone(),
        };
        fixture.config["dependencies"]["snowluma"][field] = json!(path);
        std::fs::write(
            fixture.root.join("config/config.json"),
            fixture.config.to_string(),
        )
        .unwrap();
        fixture.install_root = Some(protected.clone());
        fixture.supervisor = Some(fixture.spawn_her(&["supervise"]));
        fixture.wait(|| {
            std::fs::read_to_string(fixture.root.join("supervise.err"))
                .unwrap_or_default()
                .contains("production install root")
        });
        assert!(
            std::fs::read_to_string(fixture.root.join("supervise.err"))
                .unwrap_or_default()
                .contains("production install root"),
            "{field}: development image used a protected dependency path: {}",
            fixture.states()
        );
        for folder in [
            fixture.root.join("snow"),
            fixture.root.join("dsh"),
            protected,
        ] {
            assert!(
                !folder.join("booting").exists(),
                "{field}: dependency started across install boundary"
            );
        }
    }
}

#[test]
fn relay_readiness_gates_dsh_and_shared_health_without_exposing_secrets() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("503");
    fixture.start();
    fixture.wait(|| folder.join("probes").exists());
    assert!(
        !fixture.root.join("dsh/attempts").exists(),
        "DSH must wait for relay HTTP readiness"
    );
    std::fs::write(folder.join("mode"), "ready").unwrap();
    fixture.healthy();
    fixture.wait(|| fixture.states()["relay"]["ready"] == true);
    // A refused health probe recovers through the existing manager's adopted state.
    assert!(matches!(
        fixture.states()["relay"]["state"].as_str(),
        Some("started" | "adopted")
    ));
    assert_eq!(
        std::fs::read_to_string(folder.join("attempts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let output = fixture
        .command(&fixture.cli)
        .args(["lifecycle", "assert-running", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let verdict: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict["parts"]["relay"]["pinnedVersion"], "8.0.36");
    for mode in ["401", "429", "500", "timeout"] {
        std::fs::write(folder.join("mode"), mode).unwrap();
        let output = fixture
            .command(&fixture.cli)
            .args(["lifecycle", "assert-running", "--runtime-root"])
            .arg(&fixture.root)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let verdict: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(verdict["parts"]["relay"]["verdict"], "fail");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-relay-secret-80"));
    }
    assert!(
        !folder.join("unexpected-request").exists(),
        "health never calls a model"
    );
    fn check(path: &std::path::Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                check(&path);
            } else if let Ok(text) = std::fs::read_to_string(path) {
                assert!(
                    !text.contains("synthetic-relay-secret-80"),
                    "secret in diagnostics"
                );
                assert!(
                    !text.contains("synthetic-private-login"),
                    "login in diagnostics"
                );
            }
        }
    }
    check(&fixture.root.join("data"));
    check(&fixture.root.join("snow"));
    for name in ["supervise.out", "supervise.err"] {
        let text = std::fs::read_to_string(fixture.root.join(name)).unwrap();
        assert!(!text.contains("synthetic-relay-secret-80"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            &folder,
            &folder.join("auth"),
            &folder.join("auth/login.json"),
            &folder.join("client-key"),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }
}

#[test]
fn relay_is_adopted_without_duplicates_and_survives_ordinary_stop() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("ready");
    let relay = fixture.dependency("relay");
    fixture.wait(|| folder.join("listening").exists());
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["relay"]["state"] == "adopted");
    assert_eq!(fixture.states()["relay"]["identity"], json!(relay.identity));
    fixture.supervisor.take();
    std::fs::remove_file(fixture.root.join("data/lifecycle/dependencies.json")).unwrap();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["relay"]["state"] == "adopted");
    assert_eq!(fixture.states()["relay"]["identity"], json!(relay.identity));
    assert_eq!(
        std::fs::read_to_string(folder.join("attempts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let output = fixture
        .command(&fixture.cli)
        .args(["stop", "--confirm", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "ordinary stop failed");
    assert_eq!(
        process_identity::check_identity(&relay.identity),
        process_identity::Liveness::Matches
    );
    let output = fixture
        .command(&fixture.cli)
        .args(["stop", "--confirm", "--with-dependencies", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "explicit dependency stop failed");
    fixture.wait(|| {
        process_identity::check_identity(&relay.identity) == process_identity::Liveness::Gone
    });
    assert_eq!(fixture.states()["relay"]["state"], "stopped");
}

#[cfg(feature = "parity-clock")]
#[test]
fn relay_crashes_share_backoff_and_one_outage_notice_across_supervisor_replacement() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("ready");
    fixture.enable_alerts();
    let mut now = 1_800_000_000_000;
    fixture.clock(now);
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["relay"]["ready"] == true);
    let dsh = fixture.states()["dsh"]["identity"].clone();
    for (index, delay) in [5_000, 30_000, 60_000, 60_000].into_iter().enumerate() {
        let identity =
            serde_json::from_value(fixture.states()["relay"]["identity"].clone()).unwrap();
        assert_eq!(
            process_identity::kill_verified(&identity),
            process_identity::KillOutcome::Killed
        );
        fixture.wait(|| fixture.states()["relay"]["state"] == "backoff");
        assert_eq!(fixture.states()["relay"]["restartAt"], now + delay);
        if index == 1 {
            fixture.supervisor.take();
            fixture.start();
            fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
        }
        fixture.clock(now + delay - 1);
        fixture.wait(|| fixture.states()["relay"]["timerAt"] == now + delay - 1);
        assert_eq!(
            std::fs::read_to_string(folder.join("attempts"))
                .unwrap()
                .lines()
                .count(),
            index + 1
        );
        now += delay;
        fixture.clock(now);
        fixture.wait(|| {
            fixture.states()["relay"]["ready"] == true
                && fixture.states()["relay"]["state"] == "started"
        });
        fixture.healthy();
        assert_eq!(
            fixture.states()["dsh"]["identity"],
            dsh,
            "established DSH heals independently"
        );
    }
    fixture.wait(|| fixture.states()["relay"]["outageNotice"]["status"] == "sent");
    let alerts = std::fs::read_to_string(fixture.root.join("snow/alerts")).unwrap();
    assert_eq!(
        alerts
            .lines()
            .filter(|line| line.contains("relay failed 3 times"))
            .count(),
        1
    );
    assert!(!alerts.contains("synthetic-relay-secret-80"));
    assert!(!fixture
        .states()
        .to_string()
        .contains("synthetic-relay-secret-80"));
    now += 600_000;
    fixture.clock(now);
    fixture.wait(|| fixture.states()["relay"]["restartStep"] == 0);
}

#[test]
fn relay_with_foreign_arguments_is_never_adopted_or_stopped() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("ready");
    let cfg = &fixture.config["dependencies"]["relay"];
    let foreign = Owned::new(
        hidden(cfg["program"].as_str().unwrap())
            .args(
                cfg["arguments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|arg| arg.as_str().unwrap()),
            )
            .arg("--foreign-instance")
            .current_dir(&folder)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    fixture.wait(|| folder.join("listening").exists());
    fixture.start();
    fixture.wait(|| fixture.states()["relay"]["state"] == "refused");
    assert!(fixture.states()["relay"]["reason"]
        .as_str()
        .unwrap()
        .contains("unknown process"));
    assert_eq!(
        process_identity::check_identity(&foreign.identity),
        process_identity::Liveness::Matches
    );
    assert!(!fixture.root.join("dsh/attempts").exists());
    let output = fixture
        .command(&fixture.cli)
        .args(["stop", "--confirm", "--with-dependencies", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        process_identity::check_identity(&foreign.identity),
        process_identity::Liveness::Matches
    );
}

#[test]
fn relay_pin_and_private_paths_are_validated_before_launch() {
    for field in [
        "version",
        "program",
        "workingDirectory",
        "loginDirectory",
        "clientKeyFile",
    ] {
        let mut fixture = Fixture::new();
        let folder = fixture.relay("ready");
        fixture.config["dependencies"]["relay"][field] = if field == "version" {
            json!("^8.0.36")
        } else {
            json!(fixture.root.join("dsh"))
        };
        std::fs::write(
            fixture.root.join("config/config.json"),
            fixture.config.to_string(),
        )
        .unwrap();
        fixture.start();
        fixture.wait(|| fixture.states()["relay"]["state"] == "refused");
        assert!(
            !folder.join("attempts").exists(),
            "invalid relay must not launch"
        );
        assert!(!fixture.root.join("dsh/attempts").exists(), "DSH must wait");
        assert_eq!(fixture.states()["snowluma"]["state"], "started");
    }
}

#[test]
fn installed_relay_uses_default_deps_directory_and_reports_its_pin() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("ready");
    let install = fixture.root.clone();
    let runtime = install.join("runtime");
    fixture.fake_config("dsh", "home", json!(runtime.join("user/dsh")));
    std::fs::create_dir(&runtime).unwrap();
    for name in ["config", "data", "resources", "user", "dependencies.json"] {
        std::fs::rename(install.join(name), runtime.join(name)).unwrap();
    }
    fixture.root = runtime;
    fixture.config["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("directory");
    std::fs::write(
        fixture.root.join("config/config.json"),
        fixture.config.to_string(),
    )
    .unwrap();
    let release = install.join("releases/fixture");
    std::fs::create_dir_all(&release).unwrap();
    fixture.image = release.join(if cfg!(windows) { "her.exe" } else { "her" });
    fixture.cli = release.join(if cfg!(windows) {
        "her-cli.exe"
    } else {
        "her-cli"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_her"), &fixture.image).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_her-cli"), &fixture.cli).unwrap();
    fixture.install_root = Some(install.clone());
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["relay"]["ready"] == true);
    assert!(folder.join("attempts").exists());
    assert!(std::fs::canonicalize(std::path::Path::new(
        fixture.states()["relay"]["identity"]["exe"]
            .as_str()
            .unwrap()
    ))
    .unwrap()
    .starts_with(std::fs::canonicalize(install.join("deps")).unwrap()));
    drop(fixture);
    std::fs::remove_dir_all(install).unwrap();
}

#[test]
fn relay_refuses_configuration_arguments_outside_its_pinned_folder() {
    let mut fixture = Fixture::new();
    let folder = fixture.relay("ready");
    let outside = fixture.root.join("outside-relay.yaml");
    std::fs::write(&outside, "fixture: true\n").unwrap();
    fixture.config["dependencies"]["relay"]["arguments"]
        .as_array_mut()
        .unwrap()
        .extend([json!("-config"), json!(outside)]);
    std::fs::write(
        fixture.root.join("config/config.json"),
        fixture.config.to_string(),
    )
    .unwrap();
    fixture.start();
    fixture.wait(|| {
        fixture.states()["relay"]["state"] == "refused"
            || fixture.states()["relay"]["ready"] == true
    });
    assert_eq!(
        fixture.states()["relay"]["state"],
        "refused",
        "configuration outside the relay pin must be refused"
    );
    assert!(!folder.join("attempts").exists());
    assert!(!fixture.root.join("dsh/attempts").exists());
}

// Dependency upgrades use the real console/supervisor boundary and loopback metadata only.
fn dependency_api(
    f: &Fixture,
    method: &str,
    route: &str,
    body: Value,
    agent: bool,
) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", f.ports[2])).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap();
    let text = body.to_string();
    let port = f.ports[2];
    write!(stream, "{method} {route} HTTP/1.1\r\nHost: localhost:{port}\r\nx-console-token: fixture-console\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", if agent {"x-agent-token: rejected-agent\r\n"} else {""}, text.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, text) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(text).unwrap(),
    )
}

fn upgrade_feed(f: &Fixture) -> (Owned, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let script = f.root.join("feed.mjs");
    std::fs::write(&script, format!(r#"
import http from 'node:http'; import fs from 'node:fs';
http.createServer((req,res)=>{{
 fs.appendFileSync(new URL('./feed-requests',import.meta.url),req.url+'\n');
 // Fault modes target npm metadata; SnowLuma has its own independent release-list response.
 const mode=req.url.includes('@deepseek-ai') && fs.existsSync(new URL('./feed-mode',import.meta.url))?fs.readFileSync(new URL('./feed-mode',import.meta.url),'utf8'):'';
 if(mode==='timeout'){{fs.appendFileSync(new URL('./feed-stalled',import.meta.url),req.url+'\n');return;}}
 if(mode){{res.writeHead(Number(mode),{{'Content-Length':0,'Connection':'close'}});res.end();return;}}
 if(req.url==='/archive.zip'){{const data=fs.readFileSync(new URL('./snow.zip',import.meta.url));res.writeHead(200,{{'Content-Length':data.length,'Connection':'close'}});res.end(data);return;}}
 const file=(req.url.startsWith('/snow')||req.url.startsWith('/repos/SnowLuma/SnowLuma/releases'))?'snow-feed.json':'dsh-feed.json';
 const data=fs.readFileSync(new URL('./'+file,import.meta.url));
 res.writeHead(200,{{'Content-Length':data.length,'Connection':'close'}});res.end(data);
}}).listen({port},'127.0.0.1',()=>fs.writeFileSync(new URL('./feed-ready',import.meta.url),'ready'));
"#)).unwrap();
    std::fs::write(
        f.root.join("dsh-feed.json"),
        json!({"dist-tags":{"latest":"0.3.0"},"versions":{"0.3.0":{}}}).to_string(),
    )
    .unwrap();
    std::fs::write(f.root.join("snow-feed.json"), "[]").unwrap();
    let process = Owned::new(
        hidden(f.config["dependencies"]["dsh"]["program"].as_str().unwrap())
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    f.wait(|| f.root.join("feed-ready").exists());
    (process, format!("http://127.0.0.1:{port}/"))
}

#[test]
fn dependency_discovery_reports_sources_and_never_installs_under_any_policy() {
    for policy in ["off", "notify", "auto"] {
        let mut f = Fixture::new();
        let (_feed, url) = upgrade_feed(&f);
        f.config["update"]["policy"] = json!(policy);
        f.config["update"]["feedUrl"] = json!(format!("{url}snow"));
        // A real pinned dependency with an available upgrade must remain on its old process.
        f.config["dependencies"]["directory"] = json!(f.root.join("deps"));
        f.config["dependencies"]["dsh"]["version"] = json!("0.2.1-rc.2");
        f.config["dependencies"]["dsh"]
            .as_object_mut()
            .unwrap()
            .remove("workingDirectory");
        std::fs::write(
            f.root.join("dsh/package.json"),
            json!({"name":"@deepseek-ai/dsh","version":"0.2.1-rc.2","bin":{"dsh":"index.mjs"}})
                .to_string(),
        )
        .unwrap();
        f.config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{url}snow"));
        f.config["dependencies"]["dsh"]["registry"] = json!(url);
        std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
        f.start();
        f.healthy();
        let (code, result) = dependency_api(&f, "GET", "/api/dependencies", Value::Null, false);
        assert_eq!(code, 200, "{result}");
        assert!(result["dependencies"]["snowluma"]["sourceUnavailable"].is_null());
        assert_eq!(
            result["dependencies"]["snowluma"]["source"],
            format!("{url}snow")
        );
        assert!(result["dependencies"]["relay"]["sourceUnavailable"]
            .as_str()
            .is_some());
        let (code, checked) =
            dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
        assert_eq!(code, 200, "{checked}");
        assert_eq!(
            checked["dependencies"]["dsh"]["latest"],
            if policy == "off" {
                Value::Null
            } else {
                json!("0.3.0")
            }
        );
        let identity = f.states()["dsh"]["identity"].clone();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(f.states()["dsh"]["identity"], identity);
        assert!(!f.root.join("deps/dsh/0.3.0").exists());
        let (code, _) = dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"confirm":true,"name":"dsh","version":"0.3.0"}),
            true,
        );
        assert!(code == 401 || code == 403);
        let (code, _) = dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"confirm":true,"name":"dsh","version":"0.3.1"}),
            false,
        );
        assert_eq!(code, 409, "approval must bind the displayed version");
    }
}

fn snow_upgrade(f: &mut Fixture, url: &str, healthy: bool) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let destination = f.root.join("deps/snowluma/1.14.23");
    f.config["dependencies"]["directory"] = json!(f.root.join("deps"));
    f.config["dependencies"]["snowluma"]["version"] = json!("1.14.22");
    f.config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{url}snow"));
    f.config["dependencies"]["dsh"]["registry"] = json!(url);
    f.config["update"] = json!({"policy":"notify","feedUrl":format!("{url}snow")});
    for dir in ["config", "data/session"] {
        std::fs::create_dir_all(f.root.join("snow").join(dir)).unwrap();
    }
    std::fs::write(
        f.root.join("snow/config/account.json"),
        "synthetic-config-16",
    )
    .unwrap();
    std::fs::write(f.root.join("snow/data/session/login"), "synthetic-login-16").unwrap();
    let script = std::fs::read_to_string(f.root.join("snow/index.mjs")).unwrap();
    let script=script.replace("const server=http",r#"
if(fs.readFileSync(new URL('./config/account.json',import.meta.url),'utf8')!=='synthetic-config-16')throw Error('config not carried');
if(fs.readFileSync(new URL('./data/session/login',import.meta.url),'utf8')!=='synthetic-login-16')throw Error('login not carried');
if(process.platform!=='win32') execFileSync('./check-state.sh',[],{stdio:'ignore'});
fs.writeFileSync(new URL('./carried-verified',import.meta.url),'config/data/login verified by candidate');
const server=http"#).replace("online:true,good:true",&format!("online:{healthy},good:{healthy}"));
    let script = format!("import {{execFileSync}} from 'node:child_process';\n{script}");
    let file = std::fs::File::create(f.root.join("snow.zip")).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for (name, bytes) in [
        ("index.mjs", script.into_bytes()),
        (
            "fixture.json",
            std::fs::read(f.root.join("snow/fixture.json")).unwrap(),
        ),
        (
            "check-state.sh",
            b"#!/bin/sh\ntest \"$(cat data/session/login)\" = synthetic-login-16\n".to_vec(),
        ),
    ] {
        let options = zip::write::FileOptions::default()
            .unix_permissions(if name.ends_with(".sh") { 0o755 } else { 0o644 });
        zip.start_file(name, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(std::fs::read(f.root.join("snow.zip")).unwrap())
    );
    std::fs::write(f.root.join("snow-feed.json"),json!([{"tag_name":"v1.14.23","draft":false,"prerelease":false,"assets":[{"name":"SnowLuma-v1.14.23-win-x64.zip","browser_download_url":format!("{url}archive.zip"),"digest":format!("sha256:{digest}")}]}]).to_string()).unwrap();
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    destination
}

#[test]
fn snowluma_approved_upgrade_carries_config_data_and_login_and_keeps_previous_install() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    let destination = snow_upgrade(&mut f, &url, true);
    f.start();
    f.healthy();
    let previous = f.states()["snowluma"]["identity"].clone();
    let (code, checked) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{checked}");
    assert_eq!(checked["dependencies"]["snowluma"]["available"], "1.14.23");
    assert!(!destination.exists(), "discovery never stages a dependency");
    let (code, result) = dependency_api(
        &f,
        "POST",
        "/api/dependencies/approve",
        json!({"confirm":true,"name":"snowluma","version":"1.14.23"}),
        false,
    );
    assert_eq!(code, 200, "{result}");
    f.wait(|| {
        std::fs::read_to_string(f.root.join("config/config.json"))
            .unwrap()
            .contains("1.14.23")
            && destination.join("carried-verified").exists()
    });
    f.wait(|| {
        dependency_api(&f, "GET", "/api/dependencies", Value::Null, false).1["result"]["status"]
            == "succeeded"
    });
    f.healthy();
    assert_ne!(f.states()["snowluma"]["identity"], previous);
    assert!(matches!(
        process_identity::check_identity(&serde_json::from_value(previous).unwrap()),
        process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
    ));
    assert!(f.root.join("snow/index.mjs").exists());
    assert_eq!(
        std::fs::read_to_string(f.root.join("snow/data/session/login")).unwrap(),
        "synthetic-login-16"
    );
}

fn await_upgrade(f: &Fixture, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let latest = dependency_api(f, "GET", "/api/dependencies", Value::Null, false).1;
        if latest["result"]["status"] == expected {
            return latest;
        }
        assert!(
            Instant::now() < deadline,
            "upgrade result did not become {expected}: {latest}; states={}",
            f.states()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn approve_upgrade(f: &Fixture, name: &str, version: &str) {
    let (code, checked) = dependency_api(f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{checked}");
    assert_eq!(checked["dependencies"][name]["available"], version);
    let (code, result) = dependency_api(
        f,
        "POST",
        "/api/dependencies/approve",
        json!({"confirm":true,"name":name,"version":version}),
        false,
    );
    assert_eq!(code, 200, "{result}");
}

#[test]
fn snowluma_upgrade_health_failure_restores_pin_process_and_alerts_administrator() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    let destination = snow_upgrade(&mut f, &url, false);
    f.config["ownerQQ"] = json!(42);
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    f.start();
    f.healthy();
    approve_upgrade(&f, "snowluma", "1.14.23");
    let result = await_upgrade(&f, "rolled-back");
    assert_eq!(result["result"]["rollback"], "succeeded");
    f.healthy();
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(
        cfg["dependencies"]["snowluma"],
        f.config["dependencies"]["snowluma"]
    );
    assert!(
        destination.join("carried-verified").exists(),
        "candidate itself must verify carried state even on failure"
    );
    assert!(destination.join("index.mjs").exists());
    f.wait(|| {
        std::fs::read_to_string(f.root.join("snow/alerts"))
            .unwrap_or_default()
            .contains("依赖升级失败")
    });
}

fn dsh_upgrade(f: &mut Fixture, url: &str, healthy: bool) -> PathBuf {
    let destination = f.root.join("deps/dsh/0.3.0");
    f.config["dependencies"]["directory"] = json!(f.root.join("deps"));
    f.config["dependencies"]["dsh"]["version"] = json!("0.2.1-rc.2");
    f.config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{url}snow"));
    f.config["dependencies"]["dsh"]["registry"] = json!(url);
    f.config["dependencies"]["dsh"]["npm"] = json!(f.root.join("fake-npm.mjs"));
    f.config["dependencies"]["dsh"]
        .as_object_mut()
        .unwrap()
        .remove("workingDirectory");
    f.config["update"] = json!({"policy":"auto","feedUrl":format!("{url}snow")});
    f.config["ownerQQ"] = json!(42);
    std::fs::write(
        f.root.join("dsh/package.json"),
        json!({"name":"@deepseek-ai/dsh","version":"0.2.1-rc.2","bin":{"dsh":"index.mjs"}})
            .to_string(),
    )
    .unwrap();
    let script = std::fs::read_to_string(f.root.join("dsh/index.mjs")).unwrap();
    let script = if healthy {
        script
    } else {
        script.replace(
            "const body=JSON.stringify",
            "if(cfg.dsh){res.writeHead(401,{'Content-Length':0,'Connection':'close'});res.end();return;} const body=JSON.stringify",
        )
    };
    std::fs::write(f.root.join("candidate-dsh.mjs"), script).unwrap();
    std::fs::write(f.root.join("fake-npm.mjs"),r#"
import fs from 'node:fs'; import path from 'node:path';
const prefix=process.argv[process.argv.indexOf('--prefix')+1];
const requested=process.argv.find(a=>a.startsWith('@deepseek-ai/dsh@'));
if(!requested || !prefix)process.exit(2);
const folder=path.join(prefix,'node_modules/@deepseek-ai/dsh');fs.mkdirSync(folder,{recursive:true});
fs.writeFileSync(path.join(folder,'package.json'),JSON.stringify({name:'@deepseek-ai/dsh',version:requested.slice('@deepseek-ai/dsh@'.length),bin:{dsh:'index.mjs'}}));
fs.copyFileSync(new URL('./candidate-dsh.mjs',import.meta.url),path.join(folder,'index.mjs'));
fs.copyFileSync(new URL('./dsh/fixture.json',import.meta.url),path.join(folder,'fixture.json'));
fs.appendFileSync(new URL('./npm-installs',import.meta.url),requested+'\n');
"#).unwrap();
    std::fs::write(
        f.root.join("user/dsh/login-state"),
        "synthetic-retained-dsh-session",
    )
    .unwrap();
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    destination
}
#[test]
fn dsh_upgrade_needs_approval_even_under_auto_and_retains_the_old_install() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    let destination = dsh_upgrade(&mut f, &url, true);
    f.start();
    f.healthy();
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!f.root.join("npm-installs").exists());
    assert!(!destination.exists());
    let previous = f.states()["dsh"]["identity"].clone();
    approve_upgrade(&f, "dsh", "0.3.0");
    await_upgrade(&f, "succeeded");
    f.healthy();
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(cfg["dependencies"]["dsh"]["version"], "0.3.0");
    assert!(cfg["dependencies"]["dsh"]["script"].is_null());
    assert!(f.root.join("dsh/index.mjs").exists());
    assert!(destination
        .join("node_modules/@deepseek-ai/dsh/index.mjs")
        .exists());
    assert_ne!(f.states()["dsh"]["identity"], previous);
    assert_eq!(
        std::fs::read_to_string(f.root.join("user/dsh/login-state")).unwrap(),
        "synthetic-retained-dsh-session"
    );
}
#[test]
fn dsh_upgrade_health_failure_restores_pin_process_and_alerts_administrator() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    let destination = dsh_upgrade(&mut f, &url, false);
    f.start();
    f.healthy();
    approve_upgrade(&f, "dsh", "0.3.0");
    let result = await_upgrade(&f, "rolled-back");
    assert_eq!(result["result"]["rollback"], "succeeded");
    f.healthy();
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(cfg["dependencies"]["dsh"], f.config["dependencies"]["dsh"]);
    assert!(destination
        .join("node_modules/@deepseek-ai/dsh/index.mjs")
        .exists());
    assert!(f.root.join("dsh/index.mjs").exists());
    f.wait(|| {
        std::fs::read_to_string(f.root.join("snow/alerts"))
            .unwrap_or_default()
            .contains("依赖升级失败")
    });
}

#[test]
fn dependency_metadata_failures_never_leave_an_approvable_candidate() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    // Only the console is needed. No supervisor races the explicit refreshes.
    f.config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{url}snow"));
    f.config["dependencies"]["dsh"]["registry"] = json!(url);
    f.config["dependencies"]["dsh"]["version"] = json!("0.2.1-rc.2");
    f.config["update"] = json!({"policy":"notify","feedUrl":format!("{url}snow")});
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    f.console = Some(f.spawn_her(&["--console-serve", "--console-port", &f.ports[2].to_string()]));
    f.wait(|| std::net::TcpStream::connect(("127.0.0.1", f.ports[2])).is_ok());
    for failure in ["429", "500", "empty", "malformed", "timeout"] {
        let _ = std::fs::remove_file(f.root.join("feed-mode"));
        std::fs::write(
            f.root.join("dsh-feed.json"),
            json!({"dist-tags":{"latest":"0.3.0"},"versions":{"0.3.0":{}}}).to_string(),
        )
        .unwrap();
        let (code, available) =
            dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
        assert_eq!(code, 200, "{available}");
        assert_eq!(available["dependencies"]["dsh"]["available"], "0.3.0");
        match failure {
            "empty" => std::fs::write(f.root.join("dsh-feed.json"), "").unwrap(),
            "malformed" => std::fs::write(
                f.root.join("dsh-feed.json"),
                "printed an error with HTTP 200",
            )
            .unwrap(),
            mode => std::fs::write(f.root.join("feed-mode"), mode).unwrap(),
        }
        let (code, result) =
            dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
        assert_eq!(code, 200, "{failure}: {result}");
        assert!(
            result["dependencies"]["dsh"]["available"].is_null(),
            "{failure}: {result}"
        );
        assert!(
            result["dependencies"]["dsh"]["error"].is_string(),
            "{failure}: {result}"
        );
        assert_eq!(
            dependency_api(
                &f,
                "POST",
                "/api/dependencies/approve",
                json!({"confirm":true,"name":"dsh","version":"0.3.0"}),
                false
            )
            .0,
            409
        );
        assert!(!f.root.join("deps/dsh/0.3.0").exists());
    }
}

#[test]
fn dependency_approval_requires_confirmation_fresh_configuration_and_a_free_install_gate() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    let destination = snow_upgrade(&mut f, &url, true);
    f.start();
    f.healthy();
    assert_eq!(
        dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false).0,
        200
    );
    assert_eq!(
        dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"name":"snowluma","version":"1.14.23"}),
            false
        )
        .0,
        400
    );
    std::fs::create_dir_all(f.root.join("data/locks")).unwrap();
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join("data/locks/update-restore.lock"))
        .unwrap();
    gate.lock().unwrap();
    assert_eq!(
        dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"confirm":true,"name":"snowluma","version":"1.14.23"}),
            false
        )
        .0,
        409
    );
    drop(gate);
    // Stop discovery before changing the pin, keeping the displayed proposal stale.
    f.supervisor.take();
    f.config["dependencies"]["snowluma"]["version"] = json!("1.14.21");
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    let (code, status) = dependency_api(&f, "GET", "/api/dependencies", Value::Null, false);
    assert_eq!(code, 200);
    assert!(status["dependencies"]["snowluma"]["available"].is_null());
    assert_eq!(
        dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"confirm":true,"name":"snowluma","version":"1.14.23"}),
            false
        )
        .0,
        409,
        "{status}"
    );
    assert!(!destination.exists());
}

#[cfg(feature = "parity-clock")]
#[test]
fn stalled_dependency_discovery_does_not_delay_dependency_recovery() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    f.clock(1_000_000);
    f.start();
    f.healthy();
    std::fs::write(f.root.join("feed-mode"), "timeout").unwrap();
    f.config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{url}snow"));
    f.config["dependencies"]["dsh"]["registry"] = json!(url);
    f.config["update"] = json!({"policy":"notify","feedUrl":format!("{url}snow")});
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    f.wait(|| {
        // Native Windows curl can uppercase percent escapes. Wait for the fake's
        // timeout branch so recovery starts only after metadata I/O has stalled.
        std::fs::read_to_string(f.root.join("feed-stalled"))
            .unwrap_or_default()
            .lines()
            .any(|path| matches!(path, "/@deepseek-ai%2fdsh" | "/@deepseek-ai%2Fdsh"))
    });
    let instance: process_identity::ProcessIdentity =
        serde_json::from_value(f.states()["dsh"]["identity"].clone()).unwrap();
    assert_eq!(
        process_identity::kill_verified(&instance),
        process_identity::KillOutcome::Killed
    );
    f.wait_for(Duration::from_secs(3), || {
        f.states()["dsh"]["state"] == "backoff"
    });
    f.clock(1_005_001);
    f.healthy();
    assert_ne!(f.states()["dsh"]["identity"], json!(instance));
}

#[test]
fn approved_upgrade_finishes_while_an_independent_metadata_refresh_is_stalled() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    dsh_upgrade(&mut f, &url, true);
    f.start();
    f.healthy();
    let (code, checked) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{checked}");
    assert_eq!(checked["dependencies"]["dsh"]["available"], "0.3.0");
    let before = std::fs::read_to_string(f.root.join("feed-requests"))
        .unwrap()
        .lines()
        .count();
    std::fs::write(f.root.join("feed-mode"), "timeout").unwrap();
    let finished = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let refresh = scope.spawn(|| {
            let result = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            result
        });
        f.wait(|| {
            std::fs::read_to_string(f.root.join("feed-requests"))
                .unwrap()
                .lines()
                .count()
                > before
        });
        let (code, result) = dependency_api(
            &f,
            "POST",
            "/api/dependencies/approve",
            json!({"confirm":true,"name":"dsh","version":"0.3.0"}),
            false,
        );
        assert_eq!(code, 200, "{result}");
        await_upgrade(&f, "succeeded");
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "upgrade result publication waited for the stalled metadata transfer"
        );
        assert_eq!(
            refresh.join().unwrap().0,
            503,
            "the old-pin discovery result must be discarded after the upgrade"
        );
    });
    assert_eq!(
        dependency_api(&f, "GET", "/api/dependencies", Value::Null, false).1["result"]["status"],
        "succeeded"
    );
    f.healthy();
}

#[test]
fn dependency_discovery_reports_newest_snowluma_without_requiring_old_release_assets() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    snow_upgrade(&mut f, &url, true);
    let path = f.root.join("snow-feed.json");
    let mut releases: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    releases.as_array_mut().unwrap().extend([
        json!({"tag_name":"v1.14.21","draft":false,"prerelease":false,"assets":[]}),
        json!({"tag_name":"v9.0.0","draft":true,"prerelease":false,"assets":[]}),
        json!({"tag_name":"v8.0.0","draft":false,"prerelease":true,"assets":[]}),
    ]);
    std::fs::write(&path, releases.to_string()).unwrap();
    f.start();
    f.healthy();
    let (code, result) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{result}");
    assert_eq!(
        result["dependencies"]["snowluma"]["available"], "1.14.23",
        "{result}"
    );
    releases
        .as_array_mut()
        .unwrap()
        .push(json!({"tag_name":"v1.14.24","draft":false,"prerelease":false,"assets":[]}));
    std::fs::write(path, releases.to_string()).unwrap();
    let (code, result) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{result}");
    assert_eq!(
        result["dependencies"]["snowluma"]["latest"], "1.14.24",
        "{result}"
    );
    assert!(result["dependencies"]["snowluma"]["available"].is_null());
    assert!(result["dependencies"]["snowluma"]["error"].is_string());
    assert!(!f.root.join("deps/snowluma/1.14.23").exists());
    assert!(!f.root.join("deps/snowluma/1.14.24").exists());
}

#[test]
fn dependency_discovery_keeps_existing_http_registry_configuration_without_network_when_off() {
    let mut f = Fixture::new();
    f.config["dependencies"]["dsh"]["registry"] =
        json!("http://registry.example.invalid/npm/?view=versions");
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    // Status only, with policy off: the reserved .invalid host must never be contacted.
    f.console = Some(f.spawn_her(&["--console-serve", "--console-port", &f.ports[2].to_string()]));
    f.wait(|| std::net::TcpStream::connect(("127.0.0.1", f.ports[2])).is_ok());
    let (code, result) = dependency_api(&f, "GET", "/api/dependencies", Value::Null, false);
    assert_eq!(code, 200, "{result}");
    assert_eq!(
        result["dependencies"]["dsh"]["source"],
        "http://registry.example.invalid/npm/@deepseek-ai%2fdsh?view=versions"
    );
    assert_eq!(
        dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false).0,
        200
    );
    assert!(!f.root.join("deps").exists());
}

#[cfg(feature = "parity-clock")]
#[test]
fn interrupted_dependency_upgrade_recovers_pin_process_and_failure_notice() {
    for (point, healthy) in [
        ("pin-changed", false),
        ("candidate-started", false),
        ("candidate-started", true),
    ] {
        let mut f = Fixture::new();
        let (_feed, url) = upgrade_feed(&f);
        dsh_upgrade(&mut f, &url, healthy);
        f.start();
        f.healthy();
        let control = f.root.join("data/lifecycle/dependency-upgrade-pause");
        std::fs::write(&control, point).unwrap();
        approve_upgrade(&f, "dsh", "0.3.0");
        f.wait(|| {
            std::fs::read_to_string(f.root.join("data/lifecycle/dependency-upgrade-reached"))
                .ok()
                .as_deref()
                == Some(point)
        });
        let candidate = f.states()["dsh"]["identity"].clone();
        f.supervisor.take(); // Verified PID/start/image kill, leaving dependencies alive.
        std::fs::remove_file(&control).unwrap();
        let config_path = f.root.join("config/config.json");
        let mut cfg: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        cfg["update"]["policy"] = json!("off");
        std::fs::write(&config_path, cfg.to_string()).unwrap();
        f.start();
        let result = await_upgrade(&f, if healthy { "succeeded" } else { "rolled-back" });
        assert_eq!(
            result["result"]["rollback"],
            if healthy { "not-needed" } else { "succeeded" },
            "{point}: {result}"
        );
        f.healthy();
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap())
                .unwrap();
        if healthy {
            assert_eq!(cfg["dependencies"]["dsh"]["version"], "0.3.0");
            assert_eq!(f.states()["dsh"]["identity"], candidate);
            continue;
        }
        assert_eq!(cfg["dependencies"]["dsh"], f.config["dependencies"]["dsh"]);
        if point == "candidate-started" {
            assert!(matches!(
                process_identity::check_identity(&serde_json::from_value(candidate).unwrap()),
                process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
            ));
        }
        f.wait(|| {
            std::fs::read_to_string(f.root.join("snow/alerts"))
                .unwrap_or_default()
                .contains("依赖升级失败")
        });
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn dependency_upgrade_rollback_obeys_changed_desired_revision_during_candidate_startup() {
    for desired in ["running", "stopped", "maintenance"] {
        let mut f = Fixture::new();
        let (_feed, url) = upgrade_feed(&f);
        let destination = dsh_upgrade(&mut f, &url, true);
        f.fake_config("dsh", "delay", json!(1000));
        f.start();
        f.healthy();
        let old: process_identity::ProcessIdentity =
            serde_json::from_value(f.states()["dsh"]["identity"].clone()).unwrap();
        let control = f.root.join("data/lifecycle/dependency-upgrade-pause");
        std::fs::write(&control, "candidate-started").unwrap();
        approve_upgrade(&f, "dsh", "0.3.0");
        f.wait(|| {
            std::fs::read_to_string(f.root.join("data/lifecycle/dependency-upgrade-reached"))
                .ok()
                .as_deref()
                == Some("candidate-started")
        });
        f.wait(|| {
            destination
                .join("node_modules/@deepseek-ai/dsh/booting")
                .exists()
        });
        let candidate: process_identity::ProcessIdentity =
            serde_json::from_value(f.states()["dsh"]["identity"].clone()).unwrap();
        let output = f
            .command(&f.cli)
            .args(["lifecycle", "set", desired, "--confirm", "--runtime-root"])
            .arg(&f.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{desired}: {output:?}");
        std::fs::remove_file(control).unwrap();
        let result_path = f.root.join("data/lifecycle/dependency-upgrades.json");
        f.wait(|| {
            let result: Value =
                serde_json::from_slice(&std::fs::read(&result_path).unwrap()).unwrap();
            result["result"]["status"] == "rolled-back"
        });
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap())
                .unwrap();
        assert_eq!(
            cfg["dependencies"]["dsh"], f.config["dependencies"]["dsh"],
            "{desired}"
        );
        assert!(
            matches!(
                process_identity::check_identity(&candidate),
                process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
            ),
            "candidate survived rollback under {desired}"
        );
        assert!(matches!(
            process_identity::check_identity(&old),
            process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
        ));
        if desired == "running" {
            f.healthy();
        } else {
            let attempts = std::fs::read_to_string(f.root.join("dsh/attempts")).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(
                std::fs::read_to_string(f.root.join("dsh/attempts")).unwrap(),
                attempts,
                "rollback restarted old DSH while {desired}"
            );
            assert_eq!(attempts.lines().count(), 1);
        }
        f.wait(|| {
            std::fs::read_to_string(f.root.join("snow/alerts"))
                .unwrap_or_default()
                .contains("依赖升级失败")
        });
    }
}

#[test]
fn snowluma_default_source_full_windows_asset_and_github_digest_are_enforced() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    // Policy off: inspect the built-in URL without ever contacting GitHub.
    f.console = Some(f.spawn_her(&["--console-serve", "--console-port", &f.ports[2].to_string()]));
    f.wait(|| std::net::TcpStream::connect(("127.0.0.1", f.ports[2])).is_ok());
    let (code, status) = dependency_api(&f, "GET", "/api/dependencies", Value::Null, false);
    assert_eq!(code, 200, "{status}");
    assert_eq!(
        status["dependencies"]["snowluma"]["source"],
        "https://api.github.com/repos/SnowLuma/SnowLuma/releases"
    );
    assert!(status["dependencies"]["snowluma"]["sourceUnavailable"].is_null());
    assert!(status["dependencies"]["relay"]["sourceUnavailable"].is_string());
    let destination = snow_upgrade(&mut f, &url, true);
    f.config["dependencies"]["snowluma"]["feedUrl"] =
        json!(format!("{url}repos/SnowLuma/SnowLuma/releases"));
    f.config["ownerQQ"] = json!(42);
    std::fs::write(f.root.join("config/config.json"), f.config.to_string()).unwrap();
    let feed_path = f.root.join("snow-feed.json");
    let mut releases: Value = serde_json::from_slice(&std::fs::read(&feed_path).unwrap()).unwrap();
    releases[0]["assets"][0]["name"] = json!("SnowLuma-v1.14.23-win-x64.zip");
    let full = releases[0]["assets"][0].clone();
    let mut lite = full.clone();
    lite["name"] = json!("SnowLuma-v1.14.23-win-x64-lite.zip");
    let mut linux = full.clone();
    linux["name"] = json!("SnowLuma-v1.14.23-linux-x64.zip");
    releases[0]["assets"] = json!([lite, linux, full]);
    std::fs::write(&feed_path, releases.to_string()).unwrap();
    f.start();
    f.healthy();
    let previous = f.states()["snowluma"]["identity"].clone();
    let (code, checked) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
    assert_eq!(code, 200, "{checked}");
    assert_eq!(
        checked["dependencies"]["snowluma"]["source"],
        format!("{url}repos/SnowLuma/SnowLuma/releases")
    );
    assert_eq!(
        checked["dependencies"]["snowluma"]["available"], "1.14.23",
        "{checked}"
    );
    // A lite-only release or a full asset without GitHub's digest is never approvable.
    for assets in [json!([lite]), {
        let mut no_digest = full.clone();
        no_digest.as_object_mut().unwrap().remove("digest");
        json!([no_digest])
    }] {
        releases[0]["assets"] = assets;
        std::fs::write(&feed_path, releases.to_string()).unwrap();
        let (_, checked) = dependency_api(&f, "POST", "/api/dependencies/check", json!({}), false);
        assert_eq!(checked["dependencies"]["snowluma"]["latest"], "1.14.23");
        assert!(
            checked["dependencies"]["snowluma"]["available"].is_null(),
            "{checked}"
        );
        assert!(checked["dependencies"]["snowluma"]["error"].is_string());
        assert_eq!(
            dependency_api(
                &f,
                "POST",
                "/api/dependencies/approve",
                json!({"confirm":true,"name":"snowluma","version":"1.14.23"}),
                false
            )
            .0,
            409
        );
    }
    let mut wrong_digest = full;
    wrong_digest["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    releases[0]["assets"] = json!([lite, wrong_digest]);
    std::fs::write(&feed_path, releases.to_string()).unwrap();
    approve_upgrade(&f, "snowluma", "1.14.23");
    let failed = await_upgrade(&f, "failed");
    assert!(
        failed["result"]["reason"]
            .as_str()
            .unwrap()
            .contains("checksum"),
        "{failed}"
    );
    assert_eq!(failed["result"]["rollback"], "not-needed");
    assert_eq!(f.states()["snowluma"]["identity"], previous);
    assert!(!destination.exists());
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(
        cfg["dependencies"]["snowluma"],
        f.config["dependencies"]["snowluma"]
    );
    f.wait(|| {
        std::fs::read_to_string(f.root.join("snow/alerts"))
            .unwrap_or_default()
            .contains("依赖升级失败")
    });
}

#[cfg(feature = "parity-clock")]
#[test]
fn unfinished_candidate_retirement_keeps_its_pin_across_restart_until_authority_recovers() {
    let mut f = Fixture::new();
    let (_feed, url) = upgrade_feed(&f);
    dsh_upgrade(&mut f, &url, true);
    f.start();
    f.healthy();
    let control = f.root.join("data/lifecycle/dependency-upgrade-pause");
    std::fs::write(&control, "candidate-started").unwrap();
    approve_upgrade(&f, "dsh", "0.3.0");
    f.wait(|| {
        std::fs::read_to_string(f.root.join("data/lifecycle/dependency-upgrade-reached"))
            .ok()
            .as_deref()
            == Some("candidate-started")
    });
    let candidate: process_identity::ProcessIdentity =
        serde_json::from_value(f.states()["dsh"]["identity"].clone()).unwrap();
    std::fs::write(
        f.root.join("data/lifecycle/desired-state.json"),
        "{unreadable",
    )
    .unwrap();
    std::fs::remove_file(control).unwrap();
    let journal_path = f.root.join("data/lifecycle/dependency-upgrades.json");
    f.wait(|| {
        let journal: Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        journal["transaction"]["phase"] == "retiring-candidate"
            && journal["result"]["rollback"] == "failed"
    });
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(
        cfg["dependencies"]["dsh"]["version"], "0.3.0",
        "unproven retirement must preserve the candidate pin"
    );
    assert_eq!(
        process_identity::check_identity(&candidate),
        process_identity::Liveness::Matches
    );
    f.supervisor.take();
    f.start();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        process_identity::check_identity(&candidate),
        process_identity::Liveness::Matches
    );
    let journal: Value = serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(journal["transaction"]["phase"], "retiring-candidate");
    let output = f
        .command(&f.cli)
        .args([
            "lifecycle",
            "set",
            "maintenance",
            "--confirm",
            "--runtime-root",
        ])
        .arg(&f.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let result = await_upgrade(&f, "rolled-back");
    assert_eq!(result["result"]["rollback"], "succeeded");
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(cfg["dependencies"]["dsh"], f.config["dependencies"]["dsh"]);
    assert!(matches!(
        process_identity::check_identity(&candidate),
        process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
    ));
    assert_eq!(
        std::fs::read_to_string(f.root.join("dsh/attempts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    f.wait(|| {
        std::fs::read_to_string(f.root.join("snow/alerts"))
            .unwrap_or_default()
            .contains("依赖升级失败")
    });
}
