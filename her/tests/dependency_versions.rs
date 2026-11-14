//! Seam 1: pinned dependency versions through the real supervisor/CLI, disposable fake
//! dependency programs and a loopback fake npm registry. No real npm registry or DSH is contacted.
use support::install_root;
mod support;
#[path = "../src/process_identity.rs"]
mod process_identity;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SNOW_VERSION: &str = "1.14.22";
const DSH_VERSION: &str = "0.2.1-rc.2";
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";

fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    crate::support::ports::release_held(); // every child starts here: free the fixture's held ports first
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
    fn new(child: Child) -> Self {
        let identity = process_identity::identity_of(child.id()).unwrap().unwrap();
        Self { child, identity }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let outcome = process_identity::kill_verified(&self.identity);
            assert!(
                matches!(
                    outcome,
                    process_identity::KillOutcome::Killed | process_identity::KillOutcome::Gone
                ),
                "{outcome:?}; expected={:?}; observed={:?}",
                self.identity,
                process_identity::identity_of(self.identity.pid)
            );
            let _ = self.child.wait();
        }
    }
}

fn which(name: &str) -> PathBuf {
    let output = hidden(if cfg!(windows) { "where.exe" } else { "which" })
        .arg(name)
        .output()
        .unwrap();
    assert!(output.status.success(), "{name} fixture prerequisite");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim(),
    )
}

/// The npm CLI entry script of the local Node installation (never a registry).
fn npm_cli(node: &Path) -> PathBuf {
    let candidates = if cfg!(windows) {
        vec![node
            .parent()
            .unwrap()
            .join("node_modules/npm/bin/npm-cli.js")]
    } else {
        vec![
            std::fs::canonicalize(which("npm")).unwrap(),
            node.parent()
                .unwrap()
                .join("../lib/node_modules/npm/bin/npm-cli.js"),
        ]
    };
    candidates
        .into_iter()
        .find(|path| path.ends_with("bin/npm-cli.js") && path.is_file())
        .expect("npm-cli.js fixture prerequisite")
}

/// SnowLuma fake: a OneBot status endpoint. DSH fake: records how it was launched and answers RPC.
const FAKE: &str = r#"
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
const cfg=JSON.parse(fs.readFileSync(new URL('./fixture.json',import.meta.url)));
if(cfg.dsh && path.resolve(process.env.DSH_HOME||'')!==path.resolve(cfg.home)) throw Error('wrong DSH_HOME');
if(!cfg.dsh && process.env.SNOWLUMA_HOOK_AUTOLOAD!=='1') throw Error('missing SnowLuma hook');
fs.mkdirSync(cfg.record,{recursive:true});
fs.writeFileSync(path.join(cfg.record,process.pid+'.json'),JSON.stringify({pid:process.pid,cwd:process.cwd(),script:process.argv[1],args:process.argv.slice(2),home:process.env.DSH_HOME||null}));
const server=http.createServer((req,res)=>{
 let request='';req.on('data',chunk=>request+=chunk);
 req.on('end',()=>{
  const body=JSON.stringify(cfg.dsh ? {type:'server-response',rpcId:JSON.parse(request||'{}').rpcId,result:{ok:true,value:{}}} : {status:'ok',retcode:0,data:{online:true,good:true}});
  res.writeHead(200,{'Content-Length':Buffer.byteLength(body),'Connection':'close'});res.end(body);
 });
});
server.listen(cfg.port,'127.0.0.1');
setTimeout(()=>server.close(()=>process.exit(0)),300000).unref();
"#;

/// Serves one package version built from a local folder: packument plus a ustar/gzip tarball.
const REGISTRY: &str = r#"
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import crypto from 'node:crypto';
const cfg=JSON.parse(fs.readFileSync(process.argv[2]));
function header(name,size){
 const h=Buffer.alloc(512);
 h.write(name,0,100,'utf8');h.write('0000644\0',100);h.write('0000000\0',108);h.write('0000000\0',116);
 h.write(size.toString(8).padStart(11,'0')+'\0',124);h.write('00000000000\0',136);h.write('        ',148);h.write('0',156);
 h.write('ustar\0',257);h.write('00',263);
 let sum=0;for(const b of h) sum+=b;
 h.write(sum.toString(8).padStart(6,'0')+'\0 ',148);
 return h;
}
function walk(dir,rel=''){
 return fs.readdirSync(path.join(dir,rel),{withFileTypes:true}).flatMap(e=>e.isDirectory()?walk(dir,path.posix.join(rel,e.name)):[path.posix.join(rel,e.name)]);
}
const parts=[];
for(const file of walk(cfg.package)){
 const body=fs.readFileSync(path.join(cfg.package,file));
 parts.push(header('package/'+file,body.length),body,Buffer.alloc((512-body.length%512)%512));
}
parts.push(Buffer.alloc(1024));
const tarball=zlib.gzipSync(Buffer.concat(parts));
const manifest=JSON.parse(fs.readFileSync(path.join(cfg.package,'package.json')));
const server=http.createServer((req,res)=>{
 fs.appendFileSync(cfg.requests,req.method+' '+req.url+'\n');
 if(fs.existsSync(cfg.stall)) return;
 const missing=fs.existsSync(cfg.missing);
 const base='http://127.0.0.1:'+server.address().port;
 let body,type='application/json';
 if(!missing && decodeURIComponent(req.url)==='/'+manifest.name){
  body=JSON.stringify({name:manifest.name,'dist-tags':{latest:manifest.version},versions:{[manifest.version]:{...manifest,_id:manifest.name+'@'+manifest.version,
   dist:{tarball:base+'/tarball.tgz',shasum:crypto.createHash('sha1').update(tarball).digest('hex'),integrity:'sha512-'+crypto.createHash('sha512').update(tarball).digest('base64')}}}});
 } else if(!missing && req.url==='/tarball.tgz'){
  body=tarball;type='application/octet-stream';
 } else {res.writeHead(404,{'Content-Type':'application/json'});res.end('{"error":"not found"}');return;}
 res.writeHead(200,{'Content-Type':type,'Content-Length':Buffer.byteLength(body)});res.end(body);
});
server.listen(0,'127.0.0.1',()=>fs.writeFileSync(cfg.ready+'.tmp',String(server.address().port))||fs.renameSync(cfg.ready+'.tmp',cfg.ready));
"#;

struct Fixture {
    root: PathBuf,
    node: PathBuf,
    ports: [u16; 3],
    config: Value,
    registry: Option<(Owned, String)>,
    supervisor: Option<Owned>,
    console: Option<Owned>,
    owned: Vec<Owned>,
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
            "her-pins-中文 it's {}-{}",
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
            "node",
            "deps",
            "user/dsh",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let reservations: Vec<_> = (0..3).map(|_| crate::support::ports::reserve()).collect();
        let ports = std::array::from_fn(|i| reservations[i].port());
        let system_node = which("node");
        let npm = npm_cli(&system_node);
        let exe = if cfg!(windows) { "node.exe" } else { "node" };
        std::fs::copy(&system_node, root.join("snow").join(exe)).unwrap();
        std::fs::copy(&system_node, root.join("node").join(exe)).unwrap();
        std::fs::write(root.join("snow/index.mjs"), FAKE).unwrap();
        std::fs::write(
            root.join("snow/fixture.json"),
            json!({"dsh":false,"port":ports[0],"record":root.join("records/snow")}).to_string(),
        )
        .unwrap();
        // The DSH package source the fake registry serves (and tests may prebuild from).
        let package = root.join("package-src");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        std::fs::write(
            package.join("package.json"),
            json!({"name":DSH_PACKAGE,"version":DSH_VERSION,"type":"module","bin":{"dsh":"bin/dsh.mjs"}}).to_string(),
        )
        .unwrap();
        std::fs::write(package.join("bin/dsh.mjs"), FAKE).unwrap();
        std::fs::write(
            package.join("bin/fixture.json"),
            json!({"dsh":true,"port":ports[1],"home":root.join("user/dsh"),"record":root.join("records/dsh")}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("npmrc"), "").unwrap();
        let config = json!({"consoleToken":"fixture-console","consolePort":ports[2],"snowluma":{"accessToken":"fixture-snow"},
            "backends":{"mcpRustBridge":false},"rollout":{"rustGroups":[]},
            "dependencies":{
                "directory":root.join("deps"),
                "snowluma":{"version":SNOW_VERSION,"program":root.join("snow").join(exe),"script":root.join("snow/index.mjs"),"workingDirectory":root.join("snow")},
                "dsh":{"version":DSH_VERSION,"program":root.join("node").join(exe),"npm":npm}},
            "tasks":{"watchdog":{"enabled":false},"memory-digest":{"enabled":false},"memory-digest-check":{"enabled":false},"backup":{"enabled":false}}});
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        std::fs::write(root.join("dependencies.json"), json!({"onebot":format!("http://127.0.0.1:{}",ports[0]),"dsh":format!("http://127.0.0.1:{}",ports[1]),"model":"http://127.0.0.1:1"}).to_string()).unwrap();
        let fixture = Self {
            root,
            node: system_node,
            ports,
            config,
            registry: None,
            supervisor: None,
            console: None,
            owned: Vec::new(),
            env: Vec::new(),
        };
        fixture.save();
        reservations.into_iter().for_each(crate::support::ports::hold); // freed by the first hidden() spawn
        fixture
    }

    fn save(&self) {
        std::fs::write(
            self.root.join("config/config.json"),
            self.config.to_string(),
        )
        .unwrap();
    }

    fn exe(&self) -> &'static str {
        if cfg!(windows) {
            "node.exe"
        } else {
            "node"
        }
    }

    fn versioned(&self) -> PathBuf {
        self.root.join("deps/dsh").join(DSH_VERSION)
    }

    /// Starts the loopback fake registry and selects it in config.
    fn start_registry(&mut self) -> String {
        let cfg = self.root.join("registry.json");
        std::fs::write(
            &cfg,
            json!({"package":self.root.join("package-src"),"requests":self.root.join("registry-requests"),
                "ready":self.root.join("registry-ready"),"missing":self.root.join("registry-missing"),"stall":self.root.join("registry-stall")})
            .to_string(),
        )
        .unwrap();
        std::fs::write(self.root.join("registry.mjs"), REGISTRY).unwrap();
        let child = hidden(&self.node)
            .arg(self.root.join("registry.mjs"))
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let owned = Owned::new(child);
        self.wait(|| self.root.join("registry-ready").exists());
        let port = std::fs::read_to_string(self.root.join("registry-ready")).unwrap();
        let url = format!("http://127.0.0.1:{}/", port.trim());
        self.registry = Some((owned, url.clone()));
        self.config["dependencies"]["dsh"]["registry"] = json!(url);
        self.save();
        url
    }

    fn registry_requests(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("registry-requests"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn spawn_her(&self, args: &[&str]) -> Owned {
        Owned::new(
            hidden(env!("CARGO_BIN_EXE_her"))
                .args(args)
                .arg("--runtime-root")
                .arg(&self.root)
                // npm reads no personal npmrc; the registry is the configured loopback fake.
                .env("npm_config_userconfig", self.root.join("npmrc"))
                .env("npm_config_update_notifier", "false")
                .envs(self.env.iter().cloned())
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

    fn health(&self, wait: &str) -> (i32, Value) {
        let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
            .args([
                "lifecycle",
                "assert-running",
                "--wait",
                wait,
                "--runtime-root",
            ])
            .arg(&self.root)
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
        )
    }

    fn healthy(&self) -> Value {
        let (code, verdict) = self.health("30s");
        assert_eq!(
            code,
            0,
            "health={verdict} states={} supervisor={}",
            self.states(),
            self.supervisor_err()
        );
        verdict
    }

    fn supervisor_err(&self) -> String {
        std::fs::read_to_string(self.root.join("supervise.err")).unwrap_or_default()
    }

    fn states(&self) -> Value {
        std::fs::read(self.root.join("data/lifecycle/dependencies.json"))
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or(json!({}))
    }

    /// Launch records written by fake DSH processes.
    fn dsh_records(&self) -> Vec<Value> {
        std::fs::read_dir(self.root.join("records/dsh"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|b| serde_json::from_slice(&b).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn wait(&self, predicate: impl Fn() -> bool) {
        self.wait_for(Duration::from_secs(60), predicate);
    }

    fn wait_for(&self, timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "fixture wait expired; states={} supervisor={}",
                self.states(),
                self.supervisor_err()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// A prebuilt fake package laid out as npm installs it.
    fn prebuilt(&self, prefix: &Path, version: &str) -> PathBuf {
        let package = prefix.join("node_modules/@deepseek-ai/dsh");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        for file in ["bin/dsh.mjs", "bin/fixture.json"] {
            std::fs::copy(self.root.join("package-src").join(file), package.join(file)).unwrap();
        }
        std::fs::write(
            package.join("package.json"),
            json!({"name":DSH_PACKAGE,"version":version,"type":"module","bin":{"dsh":"bin/dsh.mjs"}}).to_string(),
        )
        .unwrap();
        package.join("bin/dsh.mjs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.supervisor.take();
        self.console.take();
        self.owned.clear();
        self.registry.take();
        for name in ["snowluma", "dsh"] {
            let Ok(identity) = serde_json::from_value::<process_identity::ProcessIdentity>(
                self.states()[name]["identity"].clone(),
            ) else {
                continue;
            };
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
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn health_check_reports_the_pinned_versions_from_config() {
    let mut fixture = Fixture::new();
    let (code, verdict) = fixture.health("0");
    assert_eq!(code, 1, "{verdict}");
    assert_eq!(
        verdict["parts"]["snowluma"]["pinnedVersion"], SNOW_VERSION,
        "{verdict}"
    );
    assert_eq!(
        verdict["parts"]["dsh"]["pinnedVersion"], DSH_VERSION,
        "{verdict}"
    );

    // A range or tag is not a pin: the part fails and names the problem.
    fixture.config["dependencies"]["dsh"]["version"] = json!("^0.2.1");
    fixture.config["dependencies"]["snowluma"]
        .as_object_mut()
        .unwrap()
        .remove("version");
    fixture.save();
    let (code, verdict) = fixture.health("0");
    assert_eq!(code, 1, "{verdict}");
    assert_eq!(verdict["parts"]["dsh"]["verdict"], "fail", "{verdict}");
    assert!(
        verdict["parts"]["dsh"]["reason"]
            .as_str()
            .unwrap()
            .contains("pinned DSH version"),
        "{verdict}"
    );
    assert!(
        verdict["parts"]["snowluma"]["pinnedVersion"].is_null(),
        "{verdict}"
    );
}

fn same_path(left: &str, right: &Path) -> bool {
    std::fs::canonicalize(left).ok() == std::fs::canonicalize(right).ok()
}

#[test]
fn supervisor_installs_pinned_dsh_into_its_versioned_folder_and_runs_it_inside_the_runtime() {
    let mut fixture = Fixture::new();
    fixture.start_registry();
    fixture.start();
    let verdict = fixture.healthy();
    assert_eq!(verdict["parts"]["snowluma"]["pinnedVersion"], SNOW_VERSION);
    assert_eq!(verdict["parts"]["dsh"]["pinnedVersion"], DSH_VERSION);

    // npm fetched exactly the pinned version from the configured (fake) registry.
    let requests = fixture.registry_requests();
    assert!(
        requests.iter().any(|r| r == "GET /@deepseek-ai%2fdsh")
            && requests.iter().any(|r| r == "GET /tarball.tgz"),
        "{requests:?}"
    );
    let package: Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .versioned()
                .join("node_modules/@deepseek-ai/dsh/package.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(package["version"], DSH_VERSION);

    // The running DSH comes from the versioned folder, with home and working folder in the runtime.
    fixture.wait(|| fixture.states()["dsh"]["state"] == "started");
    let pid = fixture.states()["dsh"]["identity"]["pid"].as_u64().unwrap();
    let records = fixture.dsh_records();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["pid"].as_u64(), Some(pid));
    let script = record["script"].as_str().unwrap();
    assert!(
        same_path(
            script,
            &fixture
                .versioned()
                .join("node_modules/@deepseek-ai/dsh/bin/dsh.mjs")
        ),
        "{record}"
    );
    assert!(!script.contains("_npx"), "{record}");
    assert_eq!(record["args"], json!(["web", "--no-open"]));
    assert!(
        same_path(
            record["home"].as_str().unwrap(),
            &fixture.root.join("user/dsh")
        ),
        "{record}"
    );
    assert!(
        same_path(
            record["cwd"].as_str().unwrap(),
            &fixture.root.join("dsh/work")
        ),
        "{record}"
    );

    // No staging leftovers beside the versioned folder.
    let entries: Vec<_> = std::fs::read_dir(fixture.root.join("deps/dsh"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            !matches!(
                name.as_str(),
                ".npm-cache" | ".npm-userconfig" | ".npm-globalconfig"
            )
        })
        .collect();
    assert_eq!(entries, vec![DSH_VERSION.to_string()]);
}

#[test]
fn an_existing_versioned_install_is_used_in_place_without_contacting_the_registry() {
    let mut fixture = Fixture::new();
    let script = fixture.prebuilt(&fixture.versioned(), DSH_VERSION);
    fixture.start_registry();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "started");
    let first = fixture.states()["dsh"]["identity"].clone();
    let records = fixture.dsh_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(
        same_path(records[0]["script"].as_str().unwrap(), &script),
        "{records:?}"
    );
    assert!(same_path(
        records[0]["cwd"].as_str().unwrap(),
        &fixture.root.join("dsh/work")
    ));

    // A supervisor restart adopts the running pinned DSH instead of reinstalling or relaunching it.
    fixture.supervisor.take();
    std::fs::remove_file(fixture.root.join("data/lifecycle/dependencies.json")).unwrap();
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    assert_eq!(fixture.states()["dsh"]["identity"], first);
    assert_eq!(fixture.dsh_records().len(), 1);
    assert!(
        fixture.registry_requests().is_empty(),
        "{:?}",
        fixture.registry_requests()
    );
}

#[test]
fn a_configured_dsh_install_at_the_pinned_version_is_adopted_in_place() {
    let mut fixture = Fixture::new();
    let script = fixture.prebuilt(&fixture.root.join("existing"), DSH_VERSION);
    fixture.config["dependencies"]["dsh"]["script"] = json!(script);
    fixture.save();
    let running = Owned::new(
        hidden(fixture.root.join("node").join(fixture.exe()))
            .arg(&script)
            .args(["web", "--no-open"])
            .current_dir(fixture.root.join("existing"))
            .env("DSH_HOME", fixture.root.join("user/dsh"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = running.identity.pid;
    fixture.owned.push(running);
    fixture.wait(|| fixture.dsh_records().len() == 1);
    fixture.start();
    fixture.healthy();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    assert_eq!(
        fixture.states()["dsh"]["identity"]["pid"].as_u64(),
        Some(pid as u64)
    );
    assert_eq!(fixture.dsh_records().len(), 1, "no second DSH may start");
    assert!(
        !fixture.root.join("deps/dsh").exists(),
        "an install used in place needs no managed install"
    );
    // SnowLuma's configured install is used in place too.
    assert_eq!(fixture.states()["snowluma"]["state"], "started");
}

#[test]
fn dsh_installs_at_another_version_or_in_an_npx_cache_are_refused_without_starting_dsh() {
    let cases = [
        ("version", "not the pinned"),
        ("npx", "npx cache"),
        ("folder", "does not hold"),
        ("working", "workingDirectory"),
        ("npm", "npm for"),
    ];
    for (case, reason) in cases {
        let mut fixture = Fixture::new();
        match case {
            "version" => {
                let script = fixture.prebuilt(&fixture.root.join("existing"), "0.2.1-rc.5");
                fixture.config["dependencies"]["dsh"]["script"] = json!(script);
            }
            "npx" => {
                let script =
                    fixture.prebuilt(&fixture.root.join("npm-cache/_npx/0123abcd"), DSH_VERSION);
                fixture.config["dependencies"]["dsh"]["script"] = json!(script);
            }
            "working" => {
                fixture.config["dependencies"]["dsh"]["workingDirectory"] = json!(fixture.root)
            }
            "npm" => {
                fixture.config["dependencies"]["dsh"]["npm"] =
                    json!(fixture.root.join("missing-npm"))
            }
            _ => {
                fixture.prebuilt(&fixture.versioned(), "0.2.1-rc.5");
            }
        }
        fixture.save();
        fixture.start_registry();
        fixture.start();
        fixture.wait(|| fixture.supervisor_err().contains(reason));
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(
            fixture.states()["snowluma"]["state"],
            "started",
            "{case}: SnowLuma must start"
        );
        assert_eq!(fixture.states()["dsh"]["state"], "refused", "{case}");
        assert!(fixture.dsh_records().is_empty(), "{case}: DSH started");
        assert!(fixture.registry_requests().is_empty(), "{case}: npm ran");
        if case == "folder" {
            let package: Value = serde_json::from_slice(
                &std::fs::read(
                    fixture
                        .versioned()
                        .join("node_modules/@deepseek-ai/dsh/package.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                package["version"], "0.2.1-rc.5",
                "mismatched folder is left alone"
            );
            assert_eq!(fixture.states()["dsh"]["state"], "refused");
        }
    }
}

#[test]
fn a_failed_npm_install_leaves_no_versioned_folder_and_is_reported() {
    let mut fixture = Fixture::new();
    fixture.start_registry();
    std::fs::write(fixture.root.join("registry-missing"), "").unwrap();
    fixture.start();
    fixture.wait_for(Duration::from_secs(120), || {
        fixture.states()["dsh"]["state"] == "refused"
    });
    let reason = fixture.states()["dsh"]["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("npm could not install DSH 0.2.1-rc.2"),
        "{reason}"
    );
    assert!(
        fixture
            .registry_requests()
            .iter()
            .any(|r| r == "GET /@deepseek-ai%2fdsh"),
        "{:?}",
        fixture.registry_requests()
    );
    let leftovers: Vec<_> = std::fs::read_dir(fixture.root.join("deps/dsh"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            !matches!(
                name.as_str(),
                ".npm-cache" | ".npm-userconfig" | ".npm-globalconfig"
            )
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(fixture.dsh_records().is_empty());
    assert!(fixture
        .root
        .join("data/logs/dependency-dsh-install.err.log")
        .is_file());
    // SnowLuma comes first and is unaffected.
    assert_eq!(fixture.states()["snowluma"]["state"], "started");
}

fn link_directory(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    assert!(hidden("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status
        .success());
}

fn assert_pinned_entry(managed: bool, case: &str) {
    let mut fixture = Fixture::new();
    let prefix = if managed {
        fixture.versioned()
    } else {
        fixture.root.join("existing")
    };
    let script = fixture.prebuilt(&prefix, DSH_VERSION);
    let package = script.parent().unwrap().parent().unwrap();
    match case {
        "cache" => {
            let cached = fixture.prebuilt(&fixture.root.join("cache/_npx/id"), DSH_VERSION);
            std::fs::remove_dir_all(package).unwrap();
            link_directory(cached.parent().unwrap().parent().unwrap(), package);
        }
        "other-version" => {
            let other = fixture.prebuilt(&fixture.root.join("other"), "0.2.1-rc.5");
            std::fs::remove_dir_all(script.parent().unwrap()).unwrap();
            link_directory(other.parent().unwrap(), script.parent().unwrap());
        }
        _ => {
            let nested = package.join("node_modules/other");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::copy(&script, nested.join("entry.mjs")).unwrap();
            std::fs::copy(
                script.parent().unwrap().join("fixture.json"),
                nested.join("fixture.json"),
            )
            .unwrap();
            std::fs::write(
                nested.join("package.json"),
                r#"{"name":"other","version":"1.0.0","type":"module"}"#,
            )
            .unwrap();
            if managed {
                std::fs::write(package.join("package.json"), json!({"name":DSH_PACKAGE,"version":DSH_VERSION,"bin":"node_modules/other/entry.mjs"}).to_string()).unwrap();
            } else {
                fixture.config["dependencies"]["dsh"]["script"] = json!(nested.join("entry.mjs"));
            }
        }
    }
    if !managed && case != "nested" {
        fixture.config["dependencies"]["dsh"]["script"] = json!(script);
    }
    fixture.save();
    fixture.start_registry();
    fixture.start();
    fixture.wait(|| {
        fixture.states()["dsh"]["state"] == "refused" || !fixture.dsh_records().is_empty()
    });
    assert_eq!(
        fixture.states()["dsh"]["state"],
        "refused",
        "managed={managed} case={case}"
    );
    assert!(
        fixture.dsh_records().is_empty(),
        "managed={managed} case={case}"
    );
    assert_eq!(fixture.states()["snowluma"]["state"], "started");
    assert!(fixture.registry_requests().is_empty());
}
macro_rules! pinned_entry_test {
    ($name:ident, $managed:expr, $case:expr) => {
        #[test]
        fn $name() {
            assert_pinned_entry($managed, $case);
        }
    };
}
pinned_entry_test!(pinned_entry_configured_cache, false, "cache");
pinned_entry_test!(pinned_entry_managed_cache, true, "cache");
pinned_entry_test!(
    pinned_entry_configured_other_version,
    false,
    "other-version"
);
pinned_entry_test!(pinned_entry_managed_other_version, true, "other-version");
pinned_entry_test!(pinned_entry_configured_nested, false, "nested");
pinned_entry_test!(pinned_entry_managed_nested, true, "nested");

fn assert_guarded_destination(case: &str) {
    let mut fixture = Fixture::new();
    let protected = fixture.root.join("synthetic-protected-install");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("sentinel"), "unchanged").unwrap();
    let destination = match case {
        "dsh" => fixture.root.join("deps/dsh"),
        "version" => fixture.versioned(),
        "staging" => fixture.root.join("deps/dsh/.install-stale"),
        "userconfig" => fixture.root.join("deps/dsh/.npm-userconfig"),
        "globalconfig" => fixture.root.join("deps/dsh/.npm-globalconfig"),
        _ => fixture.root.join("deps/dsh/.npm-cache"),
    };
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    link_directory(&protected, &destination);
    fixture.env.push((
        "HER_INSTALL_ROOT".into(),
        protected.to_str().unwrap().into(),
    ));
    fixture.start_registry();
    fixture.start();
    fixture.wait(|| {
        fixture.states()["dsh"]["state"] == "refused" || !fixture.dsh_records().is_empty()
    });
    assert_eq!(fixture.states()["dsh"]["state"], "refused", "{case}");
    assert!(
        fixture.states()["dsh"]["reason"]
            .as_str()
            .unwrap()
            .contains("production install root"),
        "{}",
        fixture.states()
    );
    assert!(
        fixture.registry_requests().is_empty(),
        "npm must not start before guards"
    );
    assert_eq!(
        std::fs::read_dir(&protected).unwrap().count(),
        1,
        "{case}: protected destination changed"
    );
    assert_eq!(
        std::fs::read_to_string(protected.join("sentinel")).unwrap(),
        "unchanged"
    );
    assert_eq!(fixture.states()["snowluma"]["state"], "started");
}
#[test]
fn guarded_destination_dsh() {
    assert_guarded_destination("dsh");
}
#[test]
fn guarded_destination_version() {
    assert_guarded_destination("version");
}
#[test]
fn guarded_destination_cache() {
    assert_guarded_destination("cache");
}

#[test]
fn managed_npm_ignores_hostile_ambient_registry_and_config_files() {
    let mut fixture = Fixture::new();
    let package = fixture.root.join("package-src/package.json");
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(&package).unwrap()).unwrap();
    manifest["scripts"] = json!({"install":"node install.cjs"});
    std::fs::write(package, manifest.to_string()).unwrap();
    std::fs::write(
        fixture.root.join("package-src/install.cjs"),
        r#"
require('node:fs').writeFileSync('install-env.json',JSON.stringify({
 user:process.env.npm_config_userconfig,global:process.env.npm_config_globalconfig,
 ambient:process.env.npm_config_her_ambient||null}));
"#,
    )
    .unwrap();
    let registry = fixture.start_registry();
    let hostile = format!("{registry}hostile/");
    std::fs::write(
        fixture.root.join("npmrc"),
        format!("@deepseek-ai:registry={hostile}\n"),
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("global-npmrc"),
        format!("@deepseek-ai:registry={hostile}\n"),
    )
    .unwrap();
    fixture.env.extend([
        ("npm_config_@deepseek-ai:registry".into(), hostile.clone()),
        ("NPM_CONFIG_REGISTRY".into(), hostile),
        (
            "NPM_CONFIG_GLOBALCONFIG".into(),
            fixture.root.join("global-npmrc").to_str().unwrap().into(),
        ),
        ("npm_config_her_ambient".into(), "must-not-inherit".into()),
    ]);
    fixture.start();
    fixture.wait(|| {
        fixture.states()["dsh"]["state"] == "refused" || !fixture.dsh_records().is_empty()
    });
    assert_ne!(
        fixture.states()["dsh"]["state"],
        "refused",
        "{}",
        fixture.states()
    );
    fixture.healthy();
    let env: Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .versioned()
                .join("node_modules/@deepseek-ai/dsh/install-env.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(env["ambient"].is_null(), "{env}");
    for key in ["user", "global"] {
        let file = Path::new(env[key].as_str().unwrap());
        assert!(file.starts_with(fixture.root.join("deps")), "{env}");
        assert_eq!(std::fs::read(file).unwrap(), b"", "{env}");
    }
    assert_ne!(
        env["user"], env["global"],
        "npm rejects loading one file twice"
    );
    assert!(fixture
        .registry_requests()
        .iter()
        .all(|r| !r.contains("hostile")));
}

struct Captured(Vec<process_identity::ProcessIdentity>);
impl Drop for Captured {
    fn drop(&mut self) {
        for identity in &self.0 {
            if process_identity::check_identity(identity) == process_identity::Liveness::Matches {
                let _ = process_identity::kill_verified(identity);
            }
        }
    }
}

fn staging_folders(fixture: &Fixture) -> Vec<PathBuf> {
    std::fs::read_dir(fixture.root.join("deps/dsh"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".install-"))
        .map(|e| e.path())
        .collect()
}

#[test]
fn stopping_during_npm_install_reaps_its_group_cleans_staging_and_does_not_alert() {
    let mut fixture = Fixture::new();
    fixture.start_registry();
    std::fs::write(fixture.root.join("registry-stall"), "").unwrap();
    let npm = fixture.config["dependencies"]["dsh"]["npm"].clone();
    let pids = fixture.root.join("npm-pids.json");
    let wrapper = r#"
import fs from 'node:fs';
import {spawn} from 'node:child_process';
import {createRequire} from 'node:module';
const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore',windowsHide:true});
child.unref();
fs.writeFileSync(__PIDS__,JSON.stringify([process.pid,child.pid]));
createRequire(import.meta.url)(__NPM__);
"#.replace("__NPM__", &npm.to_string()).replace("__PIDS__", &json!(pids).to_string());
    let entry = fixture.root.join("npm-wrapper.mjs");
    std::fs::write(&entry, wrapper).unwrap();
    fixture.config["dependencies"]["dsh"]["npm"] = json!(entry);
    fixture.save();
    fixture.start();
    fixture.wait(|| pids.exists() && !fixture.registry_requests().is_empty());
    let ids: Vec<u32> = serde_json::from_slice(&std::fs::read(pids).unwrap()).unwrap();
    let captured = Captured(
        ids.iter()
            .map(|pid| process_identity::identity_of(*pid).unwrap().unwrap())
            .collect(),
    );
    #[cfg(unix)]
    assert_eq!(unsafe { libc::getpgid(ids[0] as i32) }, ids[0] as i32);
    let stages = staging_folders(&fixture);
    assert_eq!(stages.len(), 1);
    fixture.wait(|| stages[0].join(".installer.json").is_file());
    let owner: process_identity::ProcessIdentity = serde_json::from_slice(
        &std::fs::read(stages[0].join(".installer.json"))
            .expect("staging must record the owning supervisor"),
    )
    .unwrap();
    assert!(process_identity::same_identity(
        &owner,
        &fixture.supervisor.as_ref().unwrap().identity
    ));
    let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "set", "stopped", "--confirm", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    fixture.wait(|| {
        captured
            .0
            .iter()
            .all(|p| process_identity::check_identity(p) == process_identity::Liveness::Gone)
            && staging_folders(&fixture).is_empty()
    });
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(end) = stat.rfind(')') else { continue };
        let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
        assert!(
            !(fields.get(2).and_then(|s| s.parse::<u32>().ok()) == Some(ids[0])
                && fields[0] != "Z"),
            "npm process group still running: {stat}"
        );
    }
    assert!(fixture.dsh_records().is_empty());
    assert_ne!(
        fixture.states()["dsh"]["state"],
        "refused",
        "{}",
        fixture.states()
    );
    assert!(!fixture
        .supervisor_err()
        .contains("desired state stopped npm"));
}

fn set_desired(fixture: &Fixture, state: &str) {
    let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "set", state, "--confirm", "--runtime-root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn an_unpinned_legacy_npx_install_still_runs_with_one_warning() {
    let mut fixture = Fixture::new();
    let prefix = fixture.root.join("npm-cache/_npx/id");
    let script = fixture.prebuilt(&prefix, DSH_VERSION);
    fixture.config["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("dsh");
    fixture.config["tasks"] = json!({"nodePath":fixture.root.join("node").join(fixture.exe()),
        "start-all":{"dshScript":script,"dshWorkingDirectory":prefix}});
    fixture.save();
    fixture.start();
    fixture.wait(|| {
        fixture.states()["dsh"]["state"] == "refused" || !fixture.dsh_records().is_empty()
    });
    assert_ne!(
        fixture.states()["dsh"]["state"],
        "refused",
        "{}",
        fixture.states()
    );
    fixture.healthy();
    assert_eq!(fixture.supervisor_err().matches("unpinned DSH").count(), 1);
    set_desired(&fixture, "maintenance");
    std::thread::sleep(Duration::from_secs(2));
    set_desired(&fixture, "running");
    fixture.wait(|| fixture.states()["dsh"]["state"] == "adopted");
    assert_eq!(fixture.supervisor_err().matches("unpinned DSH").count(), 1);
    assert_eq!(fixture.dsh_records().len(), 1);
}

#[test]
fn pins_reject_numeric_prerelease_identifiers_with_leading_zeros() {
    let mut fixture = Fixture::new();
    for version in ["1.2.3-01", "1.2.3-rc.01"] {
        fixture.config["dependencies"]["dsh"]["version"] = json!(version);
        fixture.save();
        let (_, health) = fixture.health("0");
        assert!(
            health["parts"]["dsh"]["pinnedVersion"].is_null(),
            "{health}"
        );
        assert!(
            health["parts"]["dsh"]["reason"]
                .as_str()
                .unwrap()
                .contains("pinned DSH version"),
            "{health}"
        );
    }
    for version in ["1.2.3-0", "1.2.3-01alpha", DSH_VERSION] {
        fixture.config["dependencies"]["dsh"]["version"] = json!(version);
        fixture.save();
        let (_, health) = fixture.health("0");
        assert_eq!(health["parts"]["dsh"]["pinnedVersion"], version);
    }
}

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn failed_installs_back_off_across_passes_and_restarts_without_holding_the_launch_gate() {
    let mut fixture = Fixture::new();
    fixture.start_registry();
    std::fs::write(fixture.root.join("registry-missing"), "").unwrap();
    let before = wall_ms();
    fixture.start();
    fixture.wait(|| fixture.states()["dsh"]["state"] == "refused");
    let retry = fixture.states()["dsh"]["installRetry"].clone();
    assert_eq!(retry["failures"], 1, "{}", fixture.states());
    assert_eq!(retry["version"], DSH_VERSION);
    let after = retry["after"].as_i64().unwrap();
    assert!(
        after >= before + 600_000 && after <= wall_ms() + 600_000,
        "{retry}"
    );
    let requests = fixture.registry_requests().len();
    fixture.supervisor.take();
    fixture.start();
    fixture.wait(|| fixture.states()["snowluma"]["state"] == "adopted");
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fixture.root.join("data/lifecycle/dependencies.lock"))
            .unwrap();
        fixture.wait_for(
            Duration::from_secs(5),
            || unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        );
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        fixture.registry_requests().len(),
        requests,
        "backoff must survive a supervisor restart"
    );
    assert_eq!(fixture.states()["dsh"]["installRetry"], retry);

    // Expire only the fixture's persisted deadline between processes. Real npm failures still
    // drive each next attempt; no production clock or ten-minute wall wait is needed.
    for (failures, seconds) in [(2, 1200), (3, 2400), (4, 3600), (5, 3600)] {
        fixture.supervisor.take();
        let mut states = fixture.states();
        states["dsh"]["installRetry"]["after"] = json!(0);
        std::fs::write(
            fixture.root.join("data/lifecycle/dependencies.json"),
            states.to_string(),
        )
        .unwrap();
        let before = wall_ms();
        fixture.start();
        fixture.wait(|| {
            fixture.states()["dsh"]["state"] == "refused"
                && fixture.states()["dsh"]["installRetry"]["failures"] == failures
        });
        let retry = fixture.states()["dsh"]["installRetry"].clone();
        let after = retry["after"].as_i64().unwrap();
        assert!(
            after >= before + seconds * 1000 && after <= wall_ms() + seconds * 1000,
            "{retry}"
        );
    }
    assert!(fixture.dsh_records().is_empty());
}

#[test]
fn guarded_destination_staging() {
    assert_guarded_destination("staging");
}
#[test]
fn guarded_destination_userconfig() {
    assert_guarded_destination("userconfig");
}
#[test]
fn guarded_destination_globalconfig() {
    assert_guarded_destination("globalconfig");
}

#[test]
fn stale_staging_is_swept_only_when_the_recorded_owner_is_gone() {
    let mut fixture = Fixture::new();
    fixture.prebuilt(&fixture.versioned(), DSH_VERSION);
    fixture.start_registry();
    let live = fixture.registry.as_ref().unwrap().0.identity.clone();
    let child = Owned::new(
        hidden(&fixture.node)
            .args(["-e", "setInterval(()=>{},1000)"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let dead = child.identity.clone();
    drop(child);
    assert_eq!(
        process_identity::check_identity(&dead),
        process_identity::Liveness::Gone
    );
    let parent = fixture.root.join("deps/dsh");
    for name in ["dead", "live", "unknown", "reused"] {
        let folder = parent.join(format!(".install-{name}"));
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("sentinel"), name).unwrap();
        let owner = match name {
            "dead" => Some(json!(dead)),
            "live" => Some(json!(live)),
            "reused" => {
                let mut value = json!(live);
                value["start"] = json!(live.start.saturating_add(1));
                Some(value)
            }
            _ => None,
        };
        if let Some(owner) = owner {
            std::fs::write(folder.join(".installer.json"), owner.to_string()).unwrap();
        }
    }
    fixture.start();
    fixture.healthy();
    assert!(
        !parent.join(".install-dead").exists(),
        "dead owner's staging was not swept"
    );
    for name in ["live", "unknown", "reused"] {
        assert_eq!(
            std::fs::read_to_string(parent.join(format!(".install-{name}/sentinel"))).unwrap(),
            name
        );
    }
    assert!(fixture.registry_requests().is_empty());
}

#[cfg(unix)]
#[test]
fn another_runtime_sweep_preserves_staging_after_npm_exit_until_publication() {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let mut fixture = Fixture::new();
    let mut trial = Fixture::new();
    let ready = fixture.root.join("npm-ready.json");
    let release = fixture.root.join("npm-exit");
    let npm = fixture.root.join("npm-fixture.mjs");
    std::fs::write(
        &npm,
        r#"
import fs from 'node:fs';
import path from 'node:path';
const prefix=process.argv[process.argv.indexOf('--prefix')+1];
fs.cpSync(__SOURCE__,path.join(prefix,'node_modules/@deepseek-ai/dsh'),{recursive:true});
fs.writeFileSync(__READY__,JSON.stringify({pid:process.pid}));
setInterval(()=>{if(fs.existsSync(__RELEASE__)) process.exit(0);},10);
"#
        .replace(
            "__SOURCE__",
            &json!(fixture.root.join("package-src")).to_string(),
        )
        .replace("__READY__", &json!(ready).to_string())
        .replace("__RELEASE__", &json!(release).to_string()),
    )
    .unwrap();
    fixture.config["dependencies"]["dsh"]["npm"] = json!(npm);
    fixture.config["dependencies"]["dsh"]["registry"] = json!("http://127.0.0.1:1/");
    fixture.save();
    fixture.start();
    fixture.wait(|| ready.exists());
    let pid = serde_json::from_slice::<Value>(&std::fs::read(&ready).unwrap()).unwrap()["pid"]
        .as_u64()
        .unwrap() as u32;
    let installer = process_identity::identity_of(pid).unwrap().unwrap();
    let stages = staging_folders(&fixture);
    assert_eq!(stages.len(), 1);
    let stage = &stages[0];
    let manifest = stage.join("node_modules/@deepseek-ai/dsh/package.json");
    let contents = std::fs::read(&manifest).unwrap();
    std::fs::remove_file(&manifest).unwrap();
    let fifo = std::ffi::CString::new(manifest.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    std::fs::write(release, "").unwrap();

    // Opening a nonblocking FIFO writer succeeds only once the supervisor is reading the
    // manifest. That happens after run_npm has reaped npm and before staging is published.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut writer = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&manifest)
        {
            Ok(writer) => break writer,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    Instant::now() < deadline,
                    "supervisor did not validate staging"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("manifest FIFO: {error}"),
        }
    };
    assert_eq!(
        process_identity::check_identity(&installer),
        process_identity::Liveness::Gone
    );
    assert!(!fixture.versioned().exists());

    // A trial has its own launch gate and uses a different already installed pin. Reaching
    // healthy proves its reconciliation (including the shared-directory sweep) completed.
    let trial_version = "0.2.1-rc.5";
    trial.config["dependencies"]["directory"] = fixture.config["dependencies"]["directory"].clone();
    trial.config["dependencies"]["dsh"]["version"] = json!(trial_version);
    trial.prebuilt(
        &fixture.root.join("deps/dsh").join(trial_version),
        trial_version,
    );
    trial.save();
    trial.start();
    trial.healthy();
    assert!(
        stage.is_dir(),
        "another runtime swept staging while its supervisor was publishing"
    );

    // Restore a regular manifest for later package checks, then unblock the pending read.
    let replacement = manifest.with_extension("ready");
    std::fs::write(&replacement, &contents).unwrap();
    std::fs::rename(replacement, &manifest).unwrap();
    writer.write_all(&contents).unwrap();
    drop(writer);
    fixture.healthy();
    assert!(fixture.versioned().is_dir());
    assert!(!stage.exists());
    assert!(!fixture.versioned().join(".installer.json").exists());
    assert_eq!(fixture.dsh_records().len(), 1);
}
