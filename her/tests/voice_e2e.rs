//! Ticket 21: QQ voice on the Rust backend, observed through the public console HTTP surface of an
//! isolated runtime. Every dependency is a started fake: the OneBot gateway is a loopback HTTP
//! server, ffprobe/ffmpeg are fake executables (generated wrappers around this test binary, so Unix and Windows). No real QQ,
//! model, DSH or TTS service is contacted.
use support::install_root;
mod support;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE: &str = "voice-console-token-abcdefghij";
const AGENT: &str = "voice-agent-token-group-111";
const SELF_ID: i64 = 401;

// ----- fake executables -------------------------------------------------------------------------------

/// Both fakes are one behaviour in this test binary. `write_fake_tools` generates a tiny wrapper per
/// tool (a `.cmd` on Windows, a `sh` script elsewhere) that re-invokes this binary on the ignored
/// `fake_tool_entry` test with `FAKE_TOOL_MODE` set and the tool's own argv after `--`. The wrapper is
/// only a launcher, so `FFMPEG_PATH`/`FFPROBE_PATH` still name a real executable file that is spawned
/// directly, exactly like the real tools.
const FAKE_TOOL_MODE: &str = "FAKE_TOOL_MODE";

/// Not a test: the entry point of the fake `ffprobe`/`ffmpeg`. It does nothing unless a wrapper set
/// `FAKE_TOOL_MODE` (so `cargo test -- --ignored` stays harmless) and then exits with the tool's status.
#[test]
#[ignore = "entry point of the generated fake ffmpeg/ffprobe wrappers, not a test"]
fn fake_tool_entry() {
    let Ok(mode) = std::env::var(FAKE_TOOL_MODE) else { return };
    let args: Vec<String> = std::env::args().skip_while(|a| a != "--").skip(1).collect();
    let log = std::env::var("FAKE_TOOL_LOG").expect("FAKE_TOOL_LOG");
    let mut log_file = fs::OpenOptions::new().create(true).append(true).open(format!("{log}.{mode}")).expect("tool log");
    writeln!(log_file, "{}", args.join(" ")).unwrap();
    let last = args.last().cloned().unwrap_or_default();
    let value_after = |flag: &str| args.windows(2).filter(|w| w[0] == flag).map(|w| w[1].clone()).last().unwrap_or_default();
    let code = match mode.as_str() {
        "ffprobe" => {
            let duration = if last.contains("broken") {
                None
            } else if let Some(duration) = Path::new(&last).file_stem().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix("duration-")) {
                Some(duration)
            } else if last.contains("long") {
                Some("400")
            } else {
                Some("12.5")
            };
            match duration {
                None => 1,
                Some(dur) => {
                    let json = format!(
                        r#"{{"format":{{"duration":"{dur}","format_name":"mp3"}},"streams":[{{"codec_type":"audio","codec_name":"mp3","sample_rate":"24000","channels":1}}]}}"#
                    );
                    let json = if last.contains("missing-duration") {
                        let mut value: Value = serde_json::from_str(&json).unwrap();
                        value["format"].as_object_mut().unwrap().remove("duration");
                        value.to_string()
                    } else {
                        json
                    };
                    fs::write(value_after("-o"), json).expect("write probe output");
                    if last.contains("probe-failed") { 7 } else { 0 }
                }
            }
        }
        "ffmpeg" => {
            let input = value_after("-i");
            if input.contains("nofmpeg") {
                3
            } else {
                match fs::copy(&input, &last) {
                    Ok(_) => {
                        fs::OpenOptions::new().append(true).open(&last).and_then(|mut f| f.write_all(b"X")).expect("append marker");
                        0
                    }
                    Err(_) => 1,
                }
            }
        }
        other => panic!("unknown fake tool mode {other}"),
    };
    drop(log_file);
    std::process::exit(code);
}

/// Writes the wrapper for one fake tool and returns the path to put in `FFMPEG_PATH`/`FFPROBE_PATH`.
fn write_fake_tool(dir: &Path, tool: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let exe = exe.to_string_lossy();
    let entry = "--ignored --exact fake_tool_entry";
    if cfg!(windows) {
        let path = dir.join(format!("{tool}.cmd"));
        let body = format!(
            "@echo off\r\nset {FAKE_TOOL_MODE}={tool}\r\n\"{}\" {entry} -- %*\r\nexit /b %ERRORLEVEL%\r\n",
            exe.replace('%', "%%")
        );
        fs::write(&path, body).expect("write fake tool wrapper");
        path
    } else {
        let path = dir.join(tool);
        let body = format!("#!/bin/sh\n{FAKE_TOOL_MODE}={tool} exec '{}' {entry} -- \"$@\"\n", exe.replace('\'', r"'\''"));
        fs::write(&path, body).expect("write fake tool wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }
}

// ----- fixture root -----------------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    tools: PathBuf,
    ffprobe: PathBuf,
    ffmpeg: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Options {
    voice: Value,
    tools: Value,
    mode: &'static str,
    owner: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            voice: json!({ "enabled": true, "dir": "audio" }),
            tools: json!({ "sendVoice": true }),
            mode: "reserved2",
            owner: 9001,
        }
    }
}

fn fixture(label: &str, onebot_url: &str, options: Options) -> Fixture {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".scratch")
        .join("voice21 fixtures")
        .join(format!("{label}-{}-{stamp}-{n}", std::process::id()));
    let root = crate::install_root::development_root(root);
    for dir in ["config", "data", "resources/audio", "tools"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let config = json!({
        "consoleToken": CONSOLE,
        "ownerQQ": options.owner,
        "allow": { "groups": [111], "private": ["222"] },
        "snowluma": { "httpUrl": "http://127.0.0.1:1", "accessToken": "onebot-token" },
        "socialV2": { "tools": options.tools, "voice": options.voice, "send": { "maxSendPerMinute": 20, "maxSendPerHour": 100, "firstGapEnabled": false } }
    });
    fs::write(root.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let state = json!({ "paused": false, "conversations": {
        "group:111": { "agentToken": AGENT, "sendTimes": [], "recentMessages": [], "wakeConfig": { "noActionCount": 3 } },
        "private:222": { "agentToken": "voice-agent-token-private-222", "sendTimes": [], "recentMessages": [] }
    } });
    fs::write(root.join("data/social-v2.json"), serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    fs::write(root.join("data/mode.json"), json!({ "mode": options.mode }).to_string()).unwrap();
    crate::support::write_dependencies(&root, "http://127.0.0.1:1", "http://127.0.0.1:2", onebot_url, None);
    let tools = root.join("tools");
    let ffprobe = write_fake_tool(&tools, "ffprobe");
    let ffmpeg = write_fake_tool(&tools, "ffmpeg");
    Fixture { root, tools, ffprobe, ffmpeg }
}

impl Fixture {
    fn audio(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join("resources/audio").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn outbox(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("data/send-outbox.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn state(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.root.join("data/social-v2.json")).unwrap()).unwrap()
    }
    fn tool_log(&self, which: &str) -> Vec<String> {
        fs::read_to_string(self.tools.join(format!("log.{which}")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

// ----- console process --------------------------------------------------------------------------------

struct Console {
    child: Child,
    port: u16,
}

impl Console {
    fn start(fx: &Fixture) -> Self {
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(20), |port| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
            command
                .args(["--console-serve", "--runtime-root"])
                .arg(&fx.root)
                .args(["--console-port", &port.to_string()])
                .env_remove("HER_RUNTIME_ROOT")
                .env("FFPROBE_PATH", &fx.ffprobe)
                .env("FFMPEG_PATH", &fx.ffmpeg)
                .env("FAKE_TOOL_LOG", fx.tools.join("log"))
                .stdout(Stdio::from(fs::File::create(fx.root.join("console.out")).unwrap()))
                .stderr(Stdio::from(fs::File::create(fx.root.join("console.err")).unwrap()));
            crate::support::hide_window(&mut command);
            command.spawn().expect("start console")
        });
        let mut console = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(console.child.try_wait().unwrap().is_none(), "console exited early");
            if let Ok((200, _)) = request(port, "GET", "/api/health", None, None, &Value::Null) {
                return console;
            }
            assert!(Instant::now() < deadline, "console health timeout");
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn operator(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        request(self.port, method, path, Some(CONSOLE), None, &body).expect("operator request")
    }
    fn agent(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        request(self.port, method, path, Some(CONSOLE), Some(AGENT), &body).expect("agent request")
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    console: Option<&str>,
    agent: Option<&str>,
    body: &Value,
) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(90))).map_err(|e| e.to_string())?;
    let body = if body.is_null() { String::new() } else { body.to_string() };
    let mut headers = String::new();
    if let Some(v) = console {
        headers.push_str(&format!("X-Console-Token: {v}\r\n"));
    }
    if let Some(v) = agent {
        headers.push_str(&format!("X-Agent-Token: {v}\r\n"));
    }
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no separator")?;
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("bad status")?;
    Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

// ----- fake OneBot ------------------------------------------------------------------------------------

#[derive(Clone)]
enum Send {
    /// Deliver and answer with the next message id.
    Ok,
    /// OneBot refusal that proves nothing was delivered (retcode 1404).
    Refuse,
    /// Read the request, close without answering.
    Lost,
    /// Deliver (visible in history) but close without answering.
    LostButDelivered,
    /// Deliver and answer after this many milliseconds (the gateway lock is not held meanwhile).
    Slow(u64),
}

#[derive(Default)]
struct Script {
    sends: VecDeque<Send>,
    /// Segments `get_msg` returns for a sent id (None: message not found).
    readback_has_record: bool,
    history: Vec<Value>,
    requests: Vec<(String, Value)>,
    /// Arrival of every send request and the reply of slow ones, in order: `request:voice|text`, `reply:voice|text`.
    events: Vec<String>,
    next_id: i64,
}

struct FakeOneBot {
    url: String,
    script: Arc<Mutex<Script>>,
}

impl FakeOneBot {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let script = Arc::new(Mutex::new(Script { next_id: 5000, readback_has_record: true, ..Script::default() }));
        let shared = script.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                thread::spawn(move || handle(stream, shared));
            }
        });
        Self { url, script }
    }
    fn script(&self, sends: Vec<Send>) {
        self.script.lock().unwrap().sends = sends.into();
    }
    fn events(&self) -> Vec<String> {
        self.script.lock().unwrap().events.clone()
    }
    fn requests(&self, action: &str) -> Vec<Value> {
        self.script.lock().unwrap().requests.iter().filter(|(a, _)| a == action).map(|(_, b)| b.clone()).collect()
    }
}

fn handle(stream: TcpStream, script: Arc<Mutex<Script>>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let action = line.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut stream = reader.into_inner();
    let reply = |stream: &mut TcpStream, status: u16, value: Value| {
        let text = value.to_string();
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
    };
    let mut s = script.lock().unwrap();
    s.requests.push((action.clone(), body.clone()));
    let send_kind = if body.to_string().contains("\"record\"") { "voice" } else { "text" };
    if action.starts_with("send_") {
        s.events.push(format!("request:{send_kind}"));
    }
    match action.as_str() {
        "get_login_info" => reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"user_id":SELF_ID,"nickname":"bot"}})),
        "send_group_msg" | "send_private_msg" => {
            let step = s.sends.pop_front().unwrap_or(Send::Ok);
            if let Send::Slow(ms) = step {
                drop(s);
                thread::sleep(Duration::from_millis(ms));
                let mut s = script.lock().unwrap();
                s.next_id += 1;
                let id = s.next_id;
                s.events.push(format!("reply:{send_kind}"));
                reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"message_id":id}}));
                return;
            }
            match step {
                Send::Ok => {
                    s.next_id += 1;
                    let id = s.next_id;
                    reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"message_id":id}}));
                }
                Send::Refuse => reply(&mut stream, 200, json!({"status":"failed","retcode":1404,"wording":"not allowed","data":null})),
                Send::Lost => {}
                Send::Slow(_) => unreachable!(),
                Send::LostButDelivered => {
                    s.next_id += 1;
                    let id = s.next_id;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                    s.history.push(json!({"message_id": id, "time": now + 1, "sender": {"user_id": SELF_ID},
                        "message": [{"type":"record","data":{"file":"x.silk"}}]}));
                }
            }
        }
        "get_msg" => {
            if s.readback_has_record {
                reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"message":[{"type":"record","data":{"file":"/secret/path.mp3","url":"/secret/path.mp3","duration":12}}]}}));
            } else {
                reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"message":[{"type":"text","data":{"text":"x"}}]}}));
            }
        }
        "fetch_ptt_text" => reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"text":"转写"}})),
        "get_group_msg_history" | "get_friend_msg_history" => {
            let messages = s.history.clone();
            reply(&mut stream, 200, json!({"status":"ok","retcode":0,"data":{"messages":messages}}));
        }
        _ => reply(&mut stream, 200, json!({"status":"failed","retcode":1404,"data":null})),
    }
}

fn bytes(n: usize) -> Vec<u8> {
    vec![7u8; n]
}

fn list_names(value: &Value) -> Vec<String> {
    value["voices"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap().to_string()).collect()
}

// ----- AC1: formats, parameters, library, invocation --------------------------------------------------

#[test]
fn library_listing_follows_baseline_formats_hidden_files_and_probe() {
    let bot = FakeOneBot::start();
    let fx = fixture("list", &bot.url, Options::default());
    fx.audio("b.mp3", &bytes(2048));
    fx.audio("子目录/a.WAV", &bytes(100));
    fx.audio("c.silk", &silk_packets(500, false));
    fx.audio("d.spx", &bytes(10));
    fx.audio("e.wma", &bytes(10));
    fx.audio(".hidden.mp3", &bytes(10));
    fx.audio(".dot/inside.mp3", &bytes(10));
    fx.audio("notes.txt", &bytes(10));
    fx.audio("l1/l2/l3/l4/l5/deep.mp3", &bytes(10));
    fx.audio("l1/l2/l3/l4/ok4.mp3", &bytes(10));
    fx.audio("too-long.mp3", &bytes(10));
    fx.audio("broken.mp3", &bytes(10));
    let console = Console::start(&fx);
    let (status, list) = console.operator("GET", "/api/voice/list", Value::Null);
    assert_eq!(status, 200, "{list}");
    // JS: 13 extensions, hidden entries skipped, depth <= 4, localeCompare order (Latin before Han here is
    // not the case: Han sorts first), so the Han directory comes first.
    assert_eq!(
        list_names(&list),
        ["子目录/a.WAV", "b.mp3", "broken.mp3", "c.silk", "d.spx", "e.wma", "l1/l2/l3/l4/ok4.mp3", "too-long.mp3"]
    );
    let by_name = |name: &str| list["voices"].as_array().unwrap().iter().find(|v| v["name"] == name).unwrap().clone();
    let b = by_name("b.mp3");
    assert_eq!(b["ok"], true);
    assert_eq!(b["bytes"], 2048);
    assert_eq!(b["seconds"], 12.5);
    assert_eq!(b["detail"], "13秒 · 2.0 KB · mp3 · 24.0kHz · 单声道");
    // SILK reports a conservative packet duration bound without ffprobe.
    let silk = by_name("c.silk");
    assert_eq!(silk["ok"], true);
    assert_eq!(silk["seconds"], 50);
    assert_eq!(silk["detail"], "50秒 · 2.5 KB · silk");
    // A duration above maxSeconds (60) is listed as unsendable with the baseline wording.
    let long = by_name("too-long.mp3");
    assert_eq!(long["ok"], false);
    assert_eq!(long["error"], "时长 6分40秒 超过上限 1分00秒");
    // A failed ffprobe makes the file unsendable, with its cause visible.
    let broken = by_name("broken.mp3");
    assert_eq!(broken["ok"], false);
    assert_eq!(broken["seconds"], Value::Null);
    assert!(broken["error"].as_str().unwrap().contains("ffprobe 执行失败"));
    assert_eq!(list["limits"], json!({"maxSeconds": 60, "maxBytes": 20971520, "maxPerMinute": 2, "maxPerHour": 10}));
    // probe=0 lists names only.
    let (_, quick) = console.operator("GET", "/api/voice/list?probe=0", Value::Null);
    assert!(quick["voices"][0].get("ok").is_none());
}

#[test]
fn ai_channel_listing_is_gated_by_voice_switch_and_tool_flag_with_baseline_defaults() {
    let bot = FakeOneBot::start();
    // No voice section and no sendVoice flag: the JS config merge defaults both to off.
    let fx = fixture("defaults", &bot.url, Options { voice: Value::Null, tools: json!({}), ..Options::default() });
    fx.audio("a.mp3", &bytes(10));
    let console = Console::start(&fx);
    let (status, body) = console.agent("GET", "/api/socialV2/voices?key=group%3A111", Value::Null);
    assert_eq!((status, body["error"].as_str()), (403, Some("语音功能已关闭")), "{body}");
    // The operator panel still lists the library (voice.enabled only closes the AI channel).
    let (status, body) = console.operator("GET", "/api/voice/list", Value::Null);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 1);
}

#[test]
fn ai_send_route_checks_follow_the_baseline_order_and_messages() {
    let bot = FakeOneBot::start();
    let fx = fixture("ai-gates", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    // A rooted reference must not fall through to a same-named library file.
    // Node's path.win32.isAbsolute accepts /etc/passwd without a drive letter.
    fx.audio("etc/passwd", &bytes(64));
    let console = Console::start(&fx);
    let post = |body: Value| console.agent("POST", "/api/socialV2/send-voice", body);
    let key = "group:111";
    let (s, b) = post(json!({"voice": "a.mp3"}));
    assert_eq!((s, b["error"].as_str()), (400, Some("key 不能为空（格式 group:群号 或 private:QQ号）")));
    let (s, b) = request(console.port, "POST", "/api/socialV2/send-voice", Some(CONSOLE), None, &json!({"key": key, "voice": "a.mp3"})).unwrap();
    assert_eq!((s, b["error"].as_str()), (403, Some("reserved2 模式发送必须携带 agent token")), "{b}");
    let (s, b) = request(console.port, "POST", "/api/socialV2/send-voice", Some(CONSOLE), Some("wrong"), &json!({"key": key, "voice": "a.mp3"})).unwrap();
    assert_eq!(s, 403, "{b}");
    for (voice, expect) in [
        ("", "缺少音频：给语音库里的文件名（可用 qq_list_voices 查看）"),
        ("http://example.invalid/a.mp3", "语音只接受语音库里的本地文件；不接受 base64://、data: 或 http(s):// 来源"),
        ("base64://AAAA", "语音只接受语音库里的本地文件；不接受 base64://、data: 或 http(s):// 来源"),
        ("../outside.mp3", "语音只能从语音库取文件：路径里出现了「..」或绝对路径，已拒绝（防止读到库外的本机文件）"),
        ("/etc/passwd", "语音只能从语音库取文件（语音库目录：audio）；即使是绝对路径也必须位于语音库之内（socialV2.voice.allowAbsolutePath 已不再放宽 AI 通道）。请把音频放进语音库，或用操作者通道（CLI / 控制台）发送库外文件。"),
        #[cfg(windows)]
        (r"\etc\passwd", "语音只能从语音库取文件（语音库目录：audio）；即使是绝对路径也必须位于语音库之内（socialV2.voice.allowAbsolutePath 已不再放宽 AI 通道）。请把音频放进语音库，或用操作者通道（CLI / 控制台）发送库外文件。"),
        ("nothing", "语音库里找不到「nothing」。语音库里现有：a.mp3"),
    ] {
        let (s, b) = post(json!({"key": key, "voice": voice}));
        assert_eq!((s, b["error"].as_str()), (400, Some(expect)), "voice {voice:?}: {b}");
    }
    assert!(bot.requests("send_group_msg").is_empty(), "a refused reference never reaches the gateway");
}

#[test]
fn ai_send_posts_a_record_segment_verifies_by_readback_and_records_it_in_the_conversation() {
    let bot = FakeOneBot::start();
    let fx = fixture("ai-send", &bot.url, Options::default());
    let file = fx.audio("sub/a.mp3", &bytes(64));
    // An earlier observation left pre-sleep progress behind; her send starts a new one (as text sends do).
    let mut seeded = fx.state();
    for (field, value) in [("preSleepWaitSatisfiedAt", 111), ("preSleepWaitObservedAt", 222), ("preSleepWaitAccumMs", 333)] {
        seeded["conversations"]["group:111"][field] = json!(value);
    }
    fs::write(fx.root.join("data/social-v2.json"), serde_json::to_vec_pretty(&seeded).unwrap()).unwrap();
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "sub/a", "label": "说了句晚安", "verify": true, "token": AGENT}));
    assert_eq!(s, 200, "{b}");
    let sends = bot.requests("send_group_msg");
    assert_eq!(sends.len(), 1);
    let sent_file = sends[0]["message"][0]["data"]["file"].as_str().unwrap();
    assert!(Path::new(sent_file).is_absolute(), "the gateway receives an absolute local file path");
    assert_eq!(fs::canonicalize(sent_file).unwrap(), fs::canonicalize(&file).unwrap(),
        "the exact library file is sent, including on Windows with a plain gateway path");
    assert_eq!(sends[0], json!({"group_id": 111, "message": [{"type": "record", "data": {"file": sent_file}}]}));
    assert_eq!(b["ok"], true);
    assert_eq!(b["key"], "group:111");
    assert_eq!(b["messageId"], 5001);
    assert_eq!(b["file"], "a.mp3");
    assert_eq!(b["detail"], "13秒 · 64 B · mp3 · 24.0kHz · 单声道");
    assert_eq!(b["audioProcessing"], "原始音量");
    assert_eq!(b["verified"], true, "get_msg returned a record segment");
    assert_eq!(b["readback"], Value::Null);
    assert_eq!(b["warnings"], json!([]));
    assert!(!b.to_string().contains("secret"), "gateway file paths never reach the model: {b}");
    assert_eq!(bot.requests("get_msg"), vec![json!({"message_id": 5001})]);
    // The conversation record sees what was said (bridge.js recordVoiceSentV2).
    let state = fx.state();
    let conv = &state["conversations"]["group:111"];
    let last = conv["recentMessages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["sender"], "我");
    assert_eq!(last["text"], "说了句晚安");
    assert_eq!(last["messageId"], "5001");
    assert_eq!(last["isSelf"], true);
    assert!(conv["lastAiReplyAt"].as_i64().unwrap() > 0);
    // A delivered voice message is her send: it starts her engagement window (review fix 4).
    assert_eq!(conv["engagedAt"], conv["lastAiReplyAt"], "{conv}");
    for field in ["preSleepWaitSatisfiedAt", "preSleepWaitObservedAt", "preSleepWaitAccumMs"] {
        assert_eq!(conv[field], 0, "a delivered voice message resets {field} like a text send: {conv}");
    }
    assert_eq!(conv["wakeConfig"]["noActionCount"], 0);
}

#[test]
fn ai_send_without_label_records_the_audio_summary_and_readback_asks_for_transcription_only_on_request() {
    let bot = FakeOneBot::start();
    let fx = fixture("ai-readback", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "readback": true, "verify": false}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["verified"], false, "verify=false skips the get_msg readback");
    assert_eq!(b["readback"], "转写");
    assert!(bot.requests("get_msg").is_empty());
    assert_eq!(bot.requests("fetch_ptt_text"), vec![json!({"message_id": "5001"})]);
    let state = fx.state();
    let text = state["conversations"]["group:111"]["recentMessages"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(text, "[语音 13秒 · 64 B · mp3 · 24.0kHz · 单声道]");
}

#[test]
fn ai_dry_run_validates_without_sending_and_over_limit_audio_is_refused() {
    let bot = FakeOneBot::start();
    let fx = fixture("ai-dry", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    fx.audio("too-long.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "dryRun": true}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["dryRun"], true);
    assert_eq!(b["detail"], "13秒 · 64 B · mp3 · 24.0kHz · 单声道");
    assert!(bot.requests("send_group_msg").is_empty());
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "too-long.mp3"}));
    assert_eq!((s, b["error"].as_str()), (500, Some("音频不可发送：时长 6分40秒 超过上限 1分00秒")), "{b}");
    assert!(bot.requests("send_group_msg").is_empty());
}

// Ticket 56: exercise both public send channels with a real probe process and a fake gateway.
fn silk_packets(packets: usize, qq_header: bool) -> Vec<u8> {
    let mut data = Vec::new();
    if qq_header {
        data.push(0x02);
    }
    data.extend_from_slice(b"#!SILK_V3");
    for _ in 0..packets {
        data.extend_from_slice(&3_i16.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3]);
    }
    data
}

fn assert_audio_refused(console: &Console, fx: &Fixture, bot: &FakeOneBot, name: &str, cause: &str) {
    for dry_run in [false, true] {
        let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({
            "key": "group:111", "voice": name, "volume": 2, "dryRun": dry_run
        }));
        assert_eq!(status, if dry_run { 400 } else { 500 }, "{name}: {body}");
        assert!(body["error"].as_str().unwrap().contains(cause), "{name}: {body}");
        for route in ["/api/voice/send", "/api/send/voice"] {
            let (status, body) = console.operator("POST", route, json!({
                "target": "private:222", "file": name, "volume": 2, "dryRun": dry_run
            }));
            assert_eq!(status, 400, "{name}: {body}");
            assert!(body["error"].as_str().unwrap().contains(cause), "{name}: {body}");
        }
        assert!(bot.requests("send_group_msg").is_empty(), "{name}");
        assert!(bot.requests("send_private_msg").is_empty(), "{name}");
        assert!(fx.outbox().is_empty(), "{name}: refusal precedes any send intent");
        assert!(fx.tool_log("ffmpeg").is_empty(), "{name}: refusal precedes processing");
    }
}

fn silk_multiframe_packets(packets: usize, qq_header: bool) -> Vec<u8> {
    // One 60 ms packet (three frames), encoded from 1440 silent samples at 24 kHz
    // by the SILK v3 SDK. Generator and SDK TOC verification: ticket 56 fix2 evidence.
    const PAYLOAD: &[u8] = &[
        0xe9, 0xb7, 0x03, 0x81, 0x44, 0xfe, 0x03, 0x74, 0x2c, 0xfd,
        0x03, 0x02, 0x9a, 0x91, 0xdc, 0x48, 0xe8, 0xfb, 0x28, 0xfd,
        0x22, 0x0d, 0xbc, 0xc1, 0x5c, 0xac, 0xcf, 0xff, 0x26, 0x17,
    ];
    let mut data = silk_packets(0, qq_header);
    for _ in 0..packets {
        data.extend_from_slice(&(PAYLOAD.len() as i16).to_le_bytes());
        data.extend_from_slice(PAYLOAD);
    }
    data
}

fn assert_silk_packet_upper_bound(configured: Option<Value>, qq_header: bool) {
    let bot = FakeOneBot::start();
    let mut options = Options::default();
    if let Some(value) = configured {
        options.voice["maxSeconds"] = value;
    }
    let fx = fixture("silk-packet-bound", &bot.url, options);
    let extension = if qq_header { "slk" } else { "silk" };
    let long = format!("multi-frame.{extension}");
    // 1500 x 60 ms = 90 s, previously misreported as 30 s. Its safe bound is 150 s.
    fx.audio(&long, &silk_multiframe_packets(1500, qq_header));
    let console = Console::start(&fx);
    let cause = "SILK 文件时长无法确认，请改用 mp3/wav/ogg 等常见格式";
    assert_audio_refused(&console, &fx, &bot, &long, cause);

    // One DTX packet must push the upper bound from exactly 60 s to 60.1 s.
    let dtx_over = format!("dtx-over.{extension}");
    let mut audio = silk_multiframe_packets(600, qq_header);
    audio.extend_from_slice(&0_i16.to_le_bytes());
    fx.audio(&dtx_over, &audio);
    assert_audio_refused(&console, &fx, &bot, &dtx_over, cause);

    // 599 audio packets plus one DTX packet have a safe upper bound of 60 s.
    let short = format!("short-with-dtx.{extension}");
    let mut audio = silk_multiframe_packets(599, qq_header);
    audio.extend_from_slice(&0_i16.to_le_bytes());
    fx.audio(&short, &audio);
    let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({
        "key": "group:111", "voice": short, "verify": false
    }));
    assert_eq!(status, 200, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("1分00秒"), "{body}");
    let (status, body) = console.operator("POST", "/api/voice/send", json!({
        "target": "private:222", "file": short, "verify": false
    }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(bot.requests("send_group_msg").len(), 1);
    assert_eq!(bot.requests("send_private_msg").len(), 1);
    assert_eq!(fx.outbox().iter().filter(|r| r["k"] == "intent").count(), 2);
    assert!(fx.tool_log("ffprobe").is_empty(), "SILK uses a local packet upper bound");
}

#[test]
fn default_silk_packet_upper_bound_refuses_multiframe_and_dtx_over_limit() {
    assert_silk_packet_upper_bound(None, false);
}

#[test]
fn legacy_clamped_silk_packet_upper_bound_refuses_multiframe_and_dtx_over_limit() {
    for value in [json!(300), json!("300")] {
        assert_silk_packet_upper_bound(Some(value), true);
    }
}

#[test]
fn silk_packet_upper_bound_is_checked_before_sending() {
    for (extension, qq_header) in [("silk", false), ("slk", true)] {
        let bot = FakeOneBot::start();
        let mut options = Options::default();
        if qq_header {
            options.voice["maxSeconds"] = json!(30);
        }
        let fx = fixture("silk-duration", &bot.url, options);
        let long = format!("over-bound.{extension}");
        let short = format!("within-bound.{extension}");
        fx.audio(&long, &silk_packets(4500, qq_header));
        // A zero-length DTX packet contributes 100 ms to the upper bound.
        let mut short_audio = silk_packets(299, qq_header);
        short_audio.extend_from_slice(&0_i16.to_le_bytes());
        if !qq_header {
            short_audio.extend_from_slice(&(-1_i16).to_le_bytes());
        }
        fx.audio(&short, &short_audio);
        let console = Console::start(&fx);
        assert_audio_refused(&console, &fx, &bot, &long, "SILK 文件时长无法确认，请改用 mp3/wav/ogg 等常见格式");
        let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({
            "key": "group:111", "voice": short, "verify": false
        }));
        assert_eq!(status, 200, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("30秒"), "{body}");
        let (status, body) = console.operator("POST", "/api/voice/send", json!({
            "target": "private:222", "file": short, "verify": false
        }));
        assert_eq!(status, 200, "{body}");
        assert_eq!(bot.requests("send_group_msg").len(), 1);
        assert_eq!(bot.requests("send_private_msg").len(), 1);
        assert_eq!(fx.outbox().iter().filter(|r| r["k"] == "intent").count(), 2);
        assert!(fx.tool_log("ffprobe").is_empty(), "SILK uses a local packet upper bound");
    }
}

#[test]
fn malformed_silk_is_refused_before_any_send_intent() {
    let bot = FakeOneBot::start();
    let fx = fixture("malformed-silk", &bot.url, Options::default());
    let console = Console::start(&fx);
    let mut truncated_length = silk_packets(1, true);
    truncated_length.push(3);
    let mut truncated_frame = silk_packets(1, false);
    truncated_frame.extend_from_slice(&4_i16.to_le_bytes());
    truncated_frame.push(1);
    let mut negative_length = silk_packets(1, true);
    negative_length.extend_from_slice(&(-2_i16).to_le_bytes());
    let mut after_end = silk_packets(1, false);
    after_end.extend_from_slice(&(-1_i16).to_le_bytes());
    after_end.push(0);
    for (name, data) in [
        ("wrong-header.silk", bytes(64)),
        ("short-header.slk", b"\x02#!SILK".to_vec()),
        ("no-frames.silk", silk_packets(0, true)),
        ("truncated-length.slk", truncated_length),
        ("truncated-frame.silk", truncated_frame),
        ("negative-length.slk", negative_length),
        ("after-end.silk", after_end),
    ] {
        fx.audio(name, &data);
        assert_audio_refused(&console, &fx, &bot, name, "SILK 格式无效");
    }
}

#[test]
fn unavailable_probe_is_refused_before_any_send_intent() {
    let bot = FakeOneBot::start();
    let mut fx = fixture("unavailable-probe", &bot.url, Options::default());
    fx.ffprobe = fx.tools.join("missing-ffprobe");
    fx.audio("ninety.mp3", &bytes(64));
    let console = Console::start(&fx);
    assert_audio_refused(&console, &fx, &bot, "ninety.mp3", "ffprobe 不可用");
}

#[test]
fn failed_probe_is_refused_before_any_send_intent() {
    let bot = FakeOneBot::start();
    let fx = fixture("failed-probe", &bot.url, Options::default());
    let console = Console::start(&fx);
    for name in ["broken.mp3", "probe-failed.mp3"] {
        fx.audio(name, &bytes(64));
        assert_audio_refused(&console, &fx, &bot, name, "ffprobe 执行失败");
    }
}

#[test]
fn missing_or_invalid_duration_is_refused_before_any_send_intent() {
    let bot = FakeOneBot::start();
    let fx = fixture("invalid-duration", &bot.url, Options::default());
    let console = Console::start(&fx);
    for (name, cause) in [
        ("missing-duration.mp3", "ffprobe 缺少时长"),
        ("duration-unavailable.mp3", "ffprobe 时长元数据无效"),
        ("duration-NaN.mp3", "ffprobe 时长元数据无效"),
        ("duration-inf.mp3", "ffprobe 时长元数据无效"),
        ("duration-0.mp3", "ffprobe 时长元数据无效"),
        ("duration--1.mp3", "ffprobe 时长元数据无效"),
    ] {
        fx.audio(name, &bytes(64));
        assert_audio_refused(&console, &fx, &bot, name, cause);
    }
}

fn assert_duration_limit(configured: Option<Value>, limit: f64) {
    let bot = FakeOneBot::start();
    let mut options = Options::default();
    if let Some(value) = configured {
        options.voice["maxSeconds"] = value;
    }
    let fx = fixture("duration-limit", &bot.url, options);
    let at_limit = format!("duration-{limit}.mp3");
    let over_limit = format!("duration-{}.mp3", limit + 0.001);
    fx.audio(&at_limit, &bytes(64));
    fx.audio(&over_limit, &bytes(64));
    let console = Console::start(&fx);
    for dry_run in [false, true] {
        let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({
            "key": "group:111", "voice": over_limit, "volume": 2, "dryRun": dry_run
        }));
        assert_eq!(status, if dry_run { 400 } else { 500 }, "{body}");
        assert!(body["error"].as_str().unwrap().contains("超过上限"), "{body}");
        for route in ["/api/voice/send", "/api/send/voice"] {
            let (status, body) = console.operator("POST", route, json!({
                "target": "private:222", "file": over_limit, "volume": 2, "dryRun": dry_run
            }));
            assert_eq!(status, 400, "{route}: {body}");
            assert!(body["error"].as_str().unwrap().contains("超过上限"), "{body}");
        }
    }
    assert!(bot.requests("send_group_msg").is_empty());
    assert!(bot.requests("send_private_msg").is_empty());
    assert!(fx.outbox().is_empty(), "refusal precedes any send intent");
    assert!(fx.tool_log("ffmpeg").is_empty(), "refusal precedes processing");

    // The exact boundary is allowed and the refusals have not spent either channel's budget.
    let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({
        "key": "group:111", "voice": at_limit, "verify": false
    }));
    assert_eq!(status, 200, "{body}");
    let (status, body) = console.operator("POST", "/api/voice/send", json!({
        "target": "private:222", "file": at_limit, "verify": false
    }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(bot.requests("send_group_msg").len(), 1);
    assert_eq!(bot.requests("send_private_msg").len(), 1);
}

#[test]
fn default_voice_limit_accepts_sixty_seconds_and_refuses_even_slightly_longer_recordings() {
    assert_duration_limit(None, 60.0);
}

#[test]
fn legacy_and_stricter_voice_limits_are_enforced_before_sending() {
    assert_duration_limit(Some(json!(300)), 60.0);
    assert_duration_limit(Some(json!("300")), 60.0);
    assert_duration_limit(Some(json!(30)), 30.0);
}

#[test]
fn legacy_voice_limits_are_clamped_and_logged_in_console_views() {
    for (configured, expected, clamped) in [(json!(300), 60, true), (json!("300"), 60, true), (json!(30), 30, false)] {
        let bot = FakeOneBot::start();
        let fx = fixture("clamped-config", &bot.url, Options {
            voice: json!({"enabled": true, "dir": "audio", "maxSeconds": configured}),
            ..Options::default()
        });
        let console = Console::start(&fx);
        let (status, body) = console.operator("GET", "/api/socialV2/config", Value::Null);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["config"]["voice"]["maxSeconds"], expected, "{body}");
        let (status, body) = console.operator("GET", "/api/voice/list?probe=0", Value::Null);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["limits"]["maxSeconds"], expected, "{body}");
        let (status, body) = console.agent("GET", "/api/socialV2/prompt?key=group%3A111", Value::Null);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["voices"]["maxSeconds"], expected, "{body}");
        let log = fs::read_to_string(fx.root.join("console.out")).unwrap();
        if clamped {
            assert!(log.contains("[voice] socialV2.voice.maxSeconds=300 exceeds QQ limit; clamped to 60s"), "{log}");
        } else {
            assert!(!log.contains("clamped to"), "a stricter valid limit needs no clamp: {log}");
        }
        assert!(bot.requests("send_group_msg").is_empty());
        assert!(bot.requests("send_private_msg").is_empty());
    }
}

#[test]
fn console_voice_saves_cannot_raise_the_sixty_second_upper_bound() {
    let bot = FakeOneBot::start();
    let fx = fixture("save-voice-limit", &bot.url, Options::default());
    let console = Console::start(&fx);
    let (status, body) = console.operator("POST", "/api/socialV2/config", json!({
        "voice": {"enabled": true, "dir": "audio", "maxSeconds": 600}
    }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["config"]["voice"]["maxSeconds"], 60, "{body}");
    let (_, body) = console.operator("GET", "/api/socialV2/config", Value::Null);
    assert_eq!(body["config"]["voice"]["maxSeconds"], 60, "{body}");
    let log = fs::read_to_string(fx.root.join("console.out")).unwrap();
    assert!(log.contains("[voice] socialV2.voice.maxSeconds=600 exceeds QQ limit; clamped to 60s"), "{log}");
    assert!(bot.requests("send_group_msg").is_empty());
    assert!(bot.requests("send_private_msg").is_empty());
}

#[test]
fn volume_and_loudness_are_applied_locally_with_ffmpeg_and_the_temp_file_is_removed() {
    let bot = FakeOneBot::start();
    let fx = fixture("volume", &bot.url, Options { voice: json!({"enabled": true, "dir": "audio", "maxPerMinute": 50}), ..Options::default() });
    let file = fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "volume": 2}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["audioProcessing"], "音量提升至 200%");
    let sends = bot.requests("send_group_msg");
    let sent_file = sends[0]["message"][0]["data"]["file"].as_str().unwrap().to_string();
    assert_ne!(Path::new(&sent_file), fs::canonicalize(&file).unwrap(), "the processed copy is sent, not the library file");
    assert!(sent_file.contains("voice-processed") && sent_file.ends_with(".mp3"), "{sent_file}");
    assert!(!Path::new(&sent_file).exists(), "processed temp file is removed after the send");
    let args = fx.tool_log("ffmpeg");
    assert_eq!(args.len(), 1);
    assert!(args[0].starts_with("-hide_banner -loglevel error -y -i "), "{}", args[0]);
    assert!(args[0].contains(" -af volume=2 -ac 1 "), "{}", args[0]);
    // Smart loudness.
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "normalize": true, "loudness": "loud"}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["audioProcessing"], "智能音量（响度归一 -13 LUFS）");
    assert!(fx.tool_log("ffmpeg")[1].contains("-af loudnorm=I=-13:TP=-1.5:LRA=11 -ac 1 "));
    // Invalid parameters are refused before anything is sent.
    let before = bot.requests("send_group_msg").len();
    for (body, expect) in [
        (json!({"volume": 9}), "音量倍数需在 0.1~4 之间（1.0 = 原样），收到 9"),
        (json!({"volume": "loud"}), "音量必须是数字，收到「loud」"),
        (json!({"normalize": true, "loudness": "shout"}), "未知的响度档位「shout」（可选：quiet / normal / loud，或直接给 -40~-5 的 LUFS 数字）"),
        (json!({"normalize": true, "loudness": -3}), "目标响度需在 -40 ~ -5 LUFS 之间，收到 -3"),
    ] {
        let mut full = json!({"key": "group:111", "voice": "a.mp3"});
        full.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        let (s, b) = console.agent("POST", "/api/socialV2/send-voice", full);
        assert_eq!((s, b["error"].as_str()), (500, Some(expect)), "{b}");
    }
    assert_eq!(bot.requests("send_group_msg").len(), before);
}

#[test]
fn ffmpeg_failure_is_reported_and_nothing_is_sent() {
    let bot = FakeOneBot::start();
    let fx = fixture("ffmpeg-fail", &bot.url, Options::default());
    fx.audio("nofmpeg.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "nofmpeg.mp3", "volume": 2}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["error"], "音量处理失败：ffmpeg 音量处理失败（退出码 3）");
    // JS retries a non-wav input once as wav before giving up.
    assert_eq!(fx.tool_log("ffmpeg").len(), 2);
    assert!(bot.requests("send_group_msg").is_empty());
}

#[test]
fn sensitive_file_names_are_blocked_before_sending() {
    let bot = FakeOneBot::start();
    let fx = fixture("audit", &bot.url, Options::default());
    fx.audio("token=abcdef.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "token=abcdef.mp3"}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["error"], "音频文件名疑似包含敏感信息（路径/凭据/会话令牌），已阻止发送；请重命名后再发");
    assert!(bot.requests("send_group_msg").is_empty());
}

#[test]
fn outbound_guard_false_allows_a_sensitive_voice_file_name() {
    let bot = FakeOneBot::start();
    let fx = fixture("audit-off", &bot.url, Options::default());
    fx.audio("token=abcdef.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (status, body) = console.operator("POST", "/api/security", json!({"outboundGuard":false}));
    assert_eq!(status, 200, "{body}");
    let (status, body) = console.agent("POST", "/api/socialV2/send-voice", json!({"key":"group:111", "voice":"token=abcdef.mp3", "verify":false}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(bot.requests("send_group_msg").len(), 1);
}

// ----- AC2: real results, unknown is never resent -----------------------------------------------------

#[test]
fn voice_budget_is_spent_only_by_a_confirmed_send_and_reports_the_baseline_limit_message() {
    // JS voiceBudgetCommit runs only after a delivered send: a refused or an unknown send spends nothing.
    let bot = FakeOneBot::start();
    let fx = fixture("budget", &bot.url, Options { voice: json!({"enabled": true, "dir": "audio", "maxPerMinute": 1}), ..Options::default() });
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    let send = || console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "verify": false}));
    bot.script(vec![Send::Refuse, Send::Lost, Send::Ok, Send::Ok]);
    let (s, b) = send();
    assert_eq!((s, b["outcome"].as_str()), (500, Some("not_committed")), "{b}");
    let (s, b) = send();
    assert_eq!((s, b["outcome"].as_str()), (500, Some("unknown")), "{b}");
    let (s, b) = send();
    assert_eq!(s, 200, "neither the refused nor the unknown send used the quota: {b}");
    let (s, b) = send();
    assert_eq!((s, b["error"].as_str()), (500, Some("语音发送频率超限（每分钟最多 1 条），请稍后再试")), "{b}");
    assert_eq!(bot.requests("send_group_msg").len(), 3, "the limited call never reached the gateway");
}

#[test]
fn an_unknown_send_that_history_proves_delivered_spends_the_budget() {
    let bot = FakeOneBot::start();
    let fx = fixture("budget-proven", &bot.url, Options { voice: json!({"enabled": true, "dir": "audio", "maxPerMinute": 1}), ..Options::default() });
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::LostButDelivered]);
    let send = || console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "verify": false}));
    let (s, b) = send();
    assert_eq!(s, 200, "{b}");
    let (s, b) = send();
    assert_eq!((s, b["error"].as_str()), (500, Some("语音发送频率超限（每分钟最多 1 条），请稍后再试")), "{b}");
}

#[test]
fn definite_refusal_is_reported_as_not_committed_with_an_outbox_trail() {
    let bot = FakeOneBot::start();
    let fx = fixture("refuse", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::Refuse]);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3"}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["ok"], false);
    assert_eq!(b["outcome"], "not_committed");
    assert!(b["error"].as_str().unwrap().starts_with("OneBot 发送失败：retcode=1404"), "{b}");
    let kinds: Vec<String> = fx.outbox().iter().map(|r| r["k"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["intent", "attempt", "not_committed"]);
    assert_eq!(fx.outbox()[0]["kind"], "voice");
    assert_eq!(fx.outbox()[0]["messages"], json!(["[语音:a.mp3]"]));
    let recent = fx.state()["conversations"]["group:111"]["recentMessages"].clone();
    assert_eq!(recent, json!([]), "nothing was said, so nothing is recorded");
}

#[test]
fn lost_reply_is_unknown_and_never_resent_until_verified() {
    let bot = FakeOneBot::start();
    let fx = fixture("lost", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::Lost]);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3"}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["ok"], false);
    assert_eq!(b["outcome"], "unknown");
    assert!(b["error"].as_str().unwrap().contains("结果未知") && b["error"].as_str().unwrap().contains("不会自动重发"), "{b}");
    assert_eq!(b["verified"], false);
    let batch = b["batchId"].as_str().unwrap().to_string();
    let kinds: Vec<String> = fx.outbox().iter().map(|r| r["k"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["intent", "attempt", "unknown"]);
    // The gateway was asked exactly once; the bridge looked for evidence in history instead of resending.
    assert_eq!(bot.requests("send_group_msg").len(), 1);
    assert_eq!(bot.requests("get_group_msg_history").len(), 1);
    // The unresolved item is visible to operators and carries no resend path through send-message.
    let (s, view) = console.operator("GET", &format!("/api/socialV2/send-outbox?key=group%3A111"), Value::Null);
    assert_eq!(s, 200, "{view}");
    assert_eq!(view["batches"][0]["unknown"], json!([0]));
    let (s, resume) = console.agent("POST", "/api/socialV2/send-message", json!({"key": "group:111", "resumeBatchId": batch}));
    assert_ne!(s, 200, "a voice batch is never resumed as text: {resume}");
    assert_eq!(bot.requests("send_group_msg").len(), 1, "resume did not resend");
    let recent = fx.state()["conversations"]["group:111"]["recentMessages"].clone();
    assert_eq!(recent, json!([]), "an unknown send is not claimed in the conversation");
}

#[test]
fn lost_reply_with_history_evidence_is_confirmed_as_verified() {
    let bot = FakeOneBot::start();
    let fx = fixture("lost-delivered", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::LostButDelivered]);
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3", "verify": false}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["ok"], true);
    assert_eq!(b["messageId"], 5001);
    assert_eq!(bot.requests("send_group_msg").len(), 1);
    let last = fx.outbox().last().unwrap().clone();
    assert_eq!(last["k"], "confirmed");
    assert_eq!(last["verified"], true);
}

#[test]
fn a_paused_ai_cannot_send_voice() {
    let bot = FakeOneBot::start();
    let fx = fixture("guard", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    // The global agent gate refuses every v2 tool while the AI is paused.
    let mut state = fx.state();
    state["paused"] = json!(true);
    fs::write(fx.root.join("data/social-v2.json"), state.to_string()).unwrap();
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3"}));
    assert_eq!(s, 403, "{b}");
    assert!(bot.requests("send_group_msg").is_empty());
}

// ----- AC3: console operator operations ---------------------------------------------------------------

#[test]
fn operator_send_validates_target_allow_list_and_previews_with_dry_run() {
    let bot = FakeOneBot::start();
    let fx = fixture("operator", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let outside = fx.root.join("outside.mp3");
    fs::write(&outside, bytes(32)).unwrap();
    let console = Console::start(&fx);
    let post = |path: &str, body: Value| console.operator("POST", path, body);
    let (s, b) = post("/api/voice/send", json!({"target": "nonsense", "file": "a.mp3"}));
    assert_eq!((s, b["error"].as_str()), (400, Some("target 格式应为 group:群号 或 private:QQ号")));
    let (s, b) = post("/api/voice/send", json!({"target": "group:999", "file": "a.mp3"}));
    assert_eq!(s, 403);
    assert_eq!(b["error"], "目标 group:999 不在白名单内（config.json allow.groups / allow.private：群 111；私聊 222）");
    let (s, b) = post("/api/voice/send", json!({"target": "group:111", "file": "missing.mp3"}));
    assert_eq!((s, b["error"].as_str()), (400, Some("找不到音频「missing.mp3」")));
    let (s, b) = post("/api/voice/send", json!({"target": "group:111", "file": "http://x/a.mp3"}));
    assert_eq!((s, b["error"].as_str()), (400, Some("语音只接受本地文件；不接受 base64://、data: 或 http(s):// 来源")));
    let (s, b) = post("/api/voice/send", json!({"target": "group:111", "file": "a.mp3", "volume": 99}));
    assert_eq!((s, b["error"].as_str()), (400, Some("音量倍数需在 0.1~4 之间（1.0 = 原样），收到 99")));
    // Dry run previews the resolved file, limits and warnings.
    let (s, b) = post("/api/voice/send", json!({"target": "group:111", "file": "a.mp3", "dryRun": true}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["dryRun"], true);
    assert_eq!(b["key"], "group:111");
    assert_eq!(b["via"], "voice-dir");
    assert_eq!(b["bytes"], 64);
    assert_eq!(b["duration"], 12.5);
    assert_eq!(b["codec"], "mp3");
    assert_eq!(b["warnings"], json!([]));
    assert_eq!(b["limits"], json!({"maxSeconds": 60, "maxBytes": 20971520, "maxPerMinute": 2, "maxPerHour": 10}));
    assert!(bot.requests("send_group_msg").is_empty());
    // Operators may name a file outside the library by absolute path (the AI channel may not).
    let (s, b) = post("/api/voice/send", json!({"target": "private:222", "file": outside.to_string_lossy(), "dryRun": true}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["via"], "absolute");
}

#[test]
fn operator_send_works_even_when_the_ai_voice_switch_is_off_and_does_not_need_a_session() {
    let bot = FakeOneBot::start();
    let fx = fixture(
        "operator-send",
        &bot.url,
        Options { voice: json!({"enabled": false, "dir": "audio"}), tools: json!({}), ..Options::default() },
    );
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    for path in ["/api/voice/send", "/api/send/voice"] {
        let (s, b) = console.operator("POST", path, json!({"target": "private:222", "file": "a.mp3"}));
        assert_eq!(s, 200, "{path}: {b}");
        assert_eq!(b["ok"], true);
        assert_eq!(b["key"], "private:222");
        assert_eq!(b["verified"], true);
        assert_eq!(b["readback"], Value::Null);
        assert_eq!(b["audioProcessing"], "原始音量");
        assert_eq!(b["volume"], 1);
        assert_eq!(b["loudness"], Value::Null);
    }
    let sends = bot.requests("send_private_msg");
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["user_id"], 222);
    // The operator scope has its own two-per-minute budget.
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "private:222", "file": "a.mp3"}));
    assert_eq!((s, b["error"].as_str()), (500, Some("语音发送频率超限（每分钟最多 2 条），请稍后再试")), "{b}");
    // The AI is told to use its own route.
    let (s, b) = console.agent("POST", "/api/send/voice", json!({"target": "private:222", "file": "a.mp3"}));
    assert_eq!((s, b["error"].as_str()), (403, Some("AI 请改用 /api/socialV2/send-voice（带 key + token）")), "{b}");
}

#[test]
fn operator_readback_and_unverified_gateway_answer_are_reported_truthfully() {
    let bot = FakeOneBot::start();
    let fx = fixture("operator-readback", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script.lock().unwrap().readback_has_record = false;
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "a.mp3", "readback": true}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["verified"], false, "get_msg did not show a record segment");
    assert_eq!(b["readback"], "转写");
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "a.mp3", "verify": false}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["verified"], false);
}

#[test]
fn operator_unknown_outcome_is_reported_and_the_batch_is_not_resumable_as_text() {
    let bot = FakeOneBot::start();
    let fx = fixture("operator-lost", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::Lost]);
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "a.mp3"}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["outcome"], "unknown");
    assert_eq!(bot.requests("send_group_msg").len(), 1);
}

// ----- review fixes -----------------------------------------------------------------------------------

fn wait_for_event(bot: &FakeOneBot, event: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !bot.events().iter().any(|e| e == event) {
        assert!(Instant::now() < deadline, "gateway never saw {event}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_voice_send_occupies_the_shared_send_chain_so_a_later_text_send_waits_behind_it() {
    let bot = FakeOneBot::start();
    let fx = fixture("chain", &bot.url, Options { tools: json!({"sendVoice": true, "sendMessage": true}), ..Options::default() });
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::Slow(1500)]);
    let port = console.port;
    let voice = thread::spawn(move || {
        request(port, "POST", "/api/socialV2/send-voice", Some(CONSOLE), Some(AGENT), &json!({"key": "group:111", "voice": "a.mp3", "verify": false})).unwrap()
    });
    wait_for_event(&bot, "request:voice");
    let (s, text) = console.agent("POST", "/api/socialV2/send-message", json!({"key": "group:111", "messages": ["hi"]}));
    assert_eq!(s, 200, "{text}");
    let (s, v) = voice.join().unwrap();
    assert_eq!(s, 200, "{v}");
    assert_eq!(bot.events(), ["request:voice", "reply:voice", "request:text"], "JS sendChain: the text bubble is sent after the voice completed");
}

#[test]
fn the_per_item_guard_treats_a_missing_send_voice_flag_as_off_after_a_config_reload() {
    let bot = FakeOneBot::start();
    let fx = fixture("guard-reload", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    bot.script(vec![Send::Slow(1500)]);
    let port = console.port;
    let send = move || request(port, "POST", "/api/socialV2/send-voice", Some(CONSOLE), Some(AGENT), &json!({"key": "group:111", "voice": "a.mp3", "verify": false})).unwrap();
    let first = thread::spawn(send);
    wait_for_event(&bot, "request:voice");
    let second = thread::spawn(send);
    thread::sleep(Duration::from_millis(500));
    // The second call passed admission and now waits for the chain; the reload drops the key.
    let path = fx.root.join("config/config.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["socialV2"]["tools"] = json!({});
    fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    assert_eq!(first.join().unwrap().0, 200);
    let (s, b) = second.join().unwrap();
    assert_eq!((s, b["error"].as_str()), (500, Some("发送已取消：会话、模式或白名单已变化")), "{b}");
    assert_eq!(bot.requests("send_group_msg").len(), 1, "the cancelled voice never reached the gateway");
}

#[test]
fn a_missing_voice_library_is_an_empty_library_and_the_operator_can_still_send_a_file_by_path() {
    let bot = FakeOneBot::start();
    let fx = fixture("no-library", &bot.url, Options::default());
    fs::remove_dir_all(fx.root.join("resources/audio")).unwrap();
    let elsewhere = fx.root.join("elsewhere.mp3");
    fs::write(&elsewhere, bytes(64)).unwrap();
    let console = Console::start(&fx);
    let (s, list) = console.operator("GET", "/api/voice/list", Value::Null);
    assert_eq!(s, 200, "{list}");
    assert_eq!((list["count"].clone(), list["voices"].clone()), (json!(0), json!([])), "{list}");
    assert!(list["hint"].as_str().unwrap().starts_with("语音库为空"), "{list}");
    let (s, list) = console.agent("GET", "/api/socialV2/voices?key=group%3A111", Value::Null);
    assert_eq!((s, list["count"].clone()), (200, json!(0)), "{list}");
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": elsewhere.to_string_lossy()}));
    assert_eq!(s, 200, "{b}");
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "a.mp3"}));
    assert_eq!(s, 400, "{b}");
    let error = b["error"].as_str().unwrap();
    assert!(error.contains("语音库里找不到「a.mp3」") && error.contains("目前是空的"), "{b}");
    assert_eq!(bot.requests("send_group_msg").len(), 1);
}

// Creating symlinks on Windows needs a privilege (developer mode or admin) the production host's
// test account may not hold, so this one test is Unix-only.
#[cfg(unix)]
#[test]
fn links_in_the_voice_library_are_skipped_without_hiding_the_other_files() {
    let bot = FakeOneBot::start();
    let fx = fixture("links", &bot.url, Options::default());
    fx.audio("ok.mp3", &bytes(64));
    fx.audio("sub/inner.mp3", &bytes(64));
    let audio = fx.root.join("resources/audio");
    std::os::unix::fs::symlink(audio.join("ok.mp3"), audio.join("link.mp3")).unwrap();
    std::os::unix::fs::symlink(audio.join("sub"), audio.join("linkdir")).unwrap();
    std::os::unix::fs::symlink(fx.root.join("nowhere"), audio.join("dangling.mp3")).unwrap();
    let console = Console::start(&fx);
    let (s, list) = console.operator("GET", "/api/voice/list?probe=0", Value::Null);
    assert_eq!(s, 200, "{list}");
    assert_eq!(list_names(&list), ["ok.mp3", "sub/inner.mp3"], "JS lists only regular files (Dirent.isFile)");
    let (s, b) = console.agent("POST", "/api/socialV2/send-voice", json!({"key": "group:111", "voice": "ok.mp3", "verify": false}));
    assert_eq!(s, 200, "{b}");
}

#[test]
fn operator_relative_paths_resolve_against_the_resources_root_like_the_js_cwd() {
    let bot = FakeOneBot::start();
    let fx = fixture("relative", &bot.url, Options::default());
    fx.audio("a.mp3", &bytes(64));
    let console = Console::start(&fx);
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "audio/a.mp3", "verify": false}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["via"], "cwd");
    let sends = bot.requests("send_group_msg");
    let sent = sends[0]["message"][0]["data"]["file"].as_str().unwrap();
    assert_eq!(fs::canonicalize(sent).unwrap(), fs::canonicalize(fx.root.join("resources/audio/a.mp3")).unwrap(),
        "the exact resources-root file is sent, with native path separators: {sends:?}");
    // The voice directory is still tried second.
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "a.mp3", "dryRun": true}));
    assert_eq!((s, b["via"].as_str()), (200, Some("voice-dir")), "{b}");
}

// ----- AC3/AC4: prompt summary, route admission, no JS dependency -------------------------------------

#[test]
fn prompt_summarises_the_library_for_the_model_when_voice_is_enabled() {
    let bot = FakeOneBot::start();
    let fx = fixture("prompt", &bot.url, Options::default());
    for n in 0..14 {
        fx.audio(&format!("v{n:02}.mp3"), &bytes(8));
    }
    let console = Console::start(&fx);
    let (s, b) = console.agent("GET", "/api/socialV2/prompt?key=group%3A111", Value::Null);
    assert_eq!(s, 200, "{b}");
    let voices = &b["voices"];
    assert_eq!(voices["enabled"], true);
    assert_eq!(voices["total"], 14);
    assert_eq!(voices["names"].as_array().unwrap().len(), 12);
    assert_eq!(voices["names"][0], "v00.mp3");
    assert_eq!(voices["maxSeconds"], 60);
    assert_eq!(voices["hint"], "可用 qq_list_voices 查看语音库，再用 qq_send_voice 把其中一个当语音发出去");
}

#[test]
fn voice_routes_are_open_in_the_selected_runtime_with_exact_methods() {
    let bot = FakeOneBot::start();
    let fx = fixture("routes", &bot.url, Options::default());
    let console = Console::start(&fx);
    for (method, path) in [("GET", "/api/voice/send"), ("GET", "/api/send/voice"), ("GET", "/api/socialV2/send-voice")] {
        let (s, b) = console.operator(method, path, Value::Null);
        assert_eq!(s, 503, "{method} {path} stays closed: {b}");
        assert!(b["error"].as_str().unwrap().contains("not available in the selected runtime"));
    }
    let (s, b) = console.operator("POST", "/api/voice/send", json!({"target": "group:111", "file": "x", "dryRun": true}));
    assert_ne!(s, 503, "POST /api/voice/send is served by the selected runtime: {b}");
}

#[test]
fn config_view_reports_the_merged_voice_defaults_so_the_console_never_shows_voice_as_on() {
    let bot = FakeOneBot::start();
    // The file sets only `enabled`; the JS loader fills the rest and keeps sendVoice off.
    let fx = fixture("config-view", &bot.url, Options { voice: json!({"enabled": true, "maxPerMinute": 5}), tools: json!({}), ..Options::default() });
    let console = Console::start(&fx);
    let (s, b) = console.operator("GET", "/api/socialV2/config", Value::Null);
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["config"]["voice"],
        json!({"enabled": true, "dir": "audio", "maxSeconds": 60, "maxBytes": 20971520, "maxPerMinute": 5, "maxPerHour": 10,
               "allowAbsolutePath": false, "defaultVolume": 1, "loudness": "normal", "ffmpegPath": "", "autoReadback": true})
    );
    assert_eq!(b["config"]["tools"]["sendVoice"], false, "the console checkbox reads `!== false`");
    // An explicit true stays true.
    let fx2 = fixture("config-view-on", &bot.url, Options::default());
    let console2 = Console::start(&fx2);
    let (_, b) = console2.operator("GET", "/api/socialV2/config", Value::Null);
    assert_eq!(b["config"]["tools"]["sendVoice"], true);
}
