//! Ticket 20: public HTTP behavior of stickers and the self portrait in a selected runtime root.
//! Everything is synthetic: a loopback OneBot fake, a fake external `curl` (the only mocked
//! part of the image path; URL/IP guards, size checks and the routes are the real candidate) and
//! disposable roots.
use support::install_root;
use support::hide_window as hide;
mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE: &str = "ticket20-console-token-abcdefghij";
const AGENT: &str = "ticket20-agent-token";
const GROUP: i64 = 70001;
const KEY: &str = "group:70001";

struct Root(PathBuf, Option<Vec<(PathBuf, Vec<u8>)>>);
impl Root {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "ticket20-sticker-{label}-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        for dir in ["config", "data", "resources", "fake-bin", "images"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(path, None)
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    fn json(&self, rel: &str) -> Value {
        serde_json::from_slice(&fs::read(self.p(rel)).unwrap()).unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // Every test also proves the default production paths stay untouched.
        if let (Some(before), false) = (self.1.take(), thread::panicking()) {
            let after = legacy_snapshot(self);
            let _ = fs::remove_dir_all(&self.0);
            assert_eq!(after, before, "default production paths were touched");
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build a selected root: config (sticker settings merged from `social_extra`), one session
/// bound to `AGENT`, reserved2 mode and the OneBot dependency URL.
fn fixture(label: &str, onebot_url: &str, sticker_cfg: Value, stickers: Value) -> Root {
    let root = Root::new(label);
    let config = json!({
        "consoleToken": CONSOLE,
        "ownerQQ": 9001,
        "allow": { "groups": [GROUP], "private": [] },
        "snowluma": { "httpUrl": "http://127.0.0.1:1", "accessToken": "ticket20-onebot-token" },
        "socialV2": {
            "tools": { "sendSticker": true, "collectSticker": true, "getStickerImage": true,
                       "getSelfImage": true, "listStickers": true, "stickerNote": true,
                       "setStickerRemark": true },
            "send": { "maxSendPerMinute": 0, "maxSendPerHour": 0 },
            "sticker": sticker_cfg
        }
    });
    fs::write(root.p("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let state = json!({ "paused": false, "conversations": { KEY: {
        "agentToken": AGENT, "sendTimes": [], "recentMessages": []
    }}});
    fs::write(root.p("data/social-v2.json"), state.to_string()).unwrap();
    fs::write(root.p("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    crate::support::write_dependencies(&root.0, "http://127.0.0.1:1", "http://127.0.0.1:2", onebot_url, None);
    if !stickers.is_null() {
        fs::write(root.p("data/stickers.json"), stickers.to_string()).unwrap();
    }
    install_fake_curl(&root);
    install_legacy_decoy(&root);
    let mut root = root;
    root.1 = Some(legacy_snapshot(&root));
    root
}

/// The production default paths are relative `C:/qqbot/...` strings on this host, so a decoy tree
/// at the same relative location under the console's working directory is exactly what a route
/// that ignored the selected root would read or write.
fn install_legacy_decoy(root: &Root) {
    for (rel, bytes) in [
        ("C:/qqbot/qq-bridge/state/stickers.json", &b"[{\"id\":\"legacy\"}]"[..]),
        ("C:/qqbot/qq-bridge/state/social-v2.json", b"{}"),
        ("C:/qqbot/qq-bridge/config.json", b"{}"),
        ("C:/qqbot/qq-bridge/assets/deepseek娘.png", b"legacy portrait sentinel"),
    ] {
        // A drive-qualified path is absolute on Windows. Keep every decoy in
        // the disposable root; selected-root results and the foreign sentinels
        // remain asserted on both platforms.
        let rel = if cfg!(windows) { rel.strip_prefix("C:/").unwrap() } else { rel };
        let path = root.p("cwd").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

/// Every file under the decoy tree with its bytes, for before/after comparison.
fn legacy_snapshot(root: &Root) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.p("cwd"), &mut out);
    out
}

/// `curl` stand-in on PATH (the Unix name her invokes): serves `images/<basename of URL>`; 404 when absent; exit 7 when
/// `images/.fail` exists; logs every invocation's URL to `curl.log`.
#[cfg(unix)]
fn install_fake_curl(root: &Root) {
    let script = format!(
        r#"#!/bin/sh
out=""; hdr=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in -o) out="$2"; shift;; -D) hdr="$2"; shift;; esac
  url="$1"; shift
done
echo "$url" >> "{dir}/curl.log"
name=$(basename "$url")
if [ -f "{dir}/images/.fail" ]; then exit 7; fi
if [ -f "{dir}/images/$name.delay" ]; then sleep 1; fi
if [ -f "{dir}/images/$name" ]; then
  cp "{dir}/images/$name" "$out"; printf 'HTTP/1.1 200 OK\r\n\r\n' > "$hdr"; printf 200
else
  : > "$out"; printf 'HTTP/1.1 404 Not Found\r\n\r\n' > "$hdr"; printf 404
fi
"#,
        dir = root.0.display()
    );
    let path = root.p("fake-bin/curl");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn install_fake_curl(root: &Root) {
    let source = root.p("fake-bin/curl.cs");
    fs::write(&source, r#"using System; using System.IO;
class Curl { static int Main(string[] args) {
  string output=null, headers=null, url=null;
  for(int i=0;i<args.Length;i++) {
    if(args[i]=="-o" && i+1<args.Length) output=args[++i];
    else if(args[i]=="-D" && i+1<args.Length) headers=args[++i];
    else url=args[i];
  }
  string root=Environment.GetEnvironmentVariable("TICKET20_FAKE_ROOT");
  for(int attempt=0;;attempt++) {
    try { File.AppendAllText(Path.Combine(root,"curl.log"),url+"\n"); break; }
    catch(IOException) { if(attempt==99)throw; System.Threading.Thread.Sleep(10); }
  }
  string name=Path.GetFileName(new Uri(url).LocalPath);
  string image=Path.Combine(root,"images",name);
  if(File.Exists(Path.Combine(root,"images",".fail")))return 7;
  if(File.Exists(image+".delay"))System.Threading.Thread.Sleep(1000);
  bool found=File.Exists(image);
  File.WriteAllBytes(output,found ? File.ReadAllBytes(image) : new byte[0]);
  File.WriteAllText(headers,found ? "HTTP/1.1 200 OK\r\n\r\n" : "HTTP/1.1 404 Not Found\r\n\r\n");
  Console.Write(found ? "200" : "404"); return 0;
} }"#).unwrap();
    let compiler = PathBuf::from(std::env::var_os("SystemRoot").expect("Windows root"))
        .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    let mut command = Command::new(compiler);
    // Legacy csc parses forward slashes in source paths as switches. Give it
    // basenames under an explicit cwd, without mixed separators or temp aliases.
    command.current_dir(root.p("fake-bin"))
        .args(["/nologo", "/target:exe", "/out:curl.exe", "curl.cs"]);
    hide(&mut command);
    let output = command.output().expect("compile the external curl fake");
    assert!(output.status.success(), "{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

fn png(r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 2, image::Rgb([r, g, b])))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

struct Console {
    child: Child,
    port: u16,
}
impl Console {
    fn start(root: &Root) -> Self {
        let path = std::env::join_paths(std::iter::once(root.p("fake-bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        )).expect("portable fake-tool PATH");
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(10), |port| {
            let mut executable = PathBuf::from(env!("CARGO_BIN_EXE_her"));
            if cfg!(windows) {
                // Windows searches beside the executable before PATH for curl.exe.
                let copy = root.p("fake-bin/her.exe");
                fs::copy(executable, &copy).unwrap();
                executable = copy;
            }
            let mut command = Command::new(executable);
            command
                .args(["--console-serve", "--runtime-root"])
                .arg(&root.0)
                .args(["--console-port", &port.to_string()])
                .env_remove("HER_RUNTIME_ROOT")
                .env("PATH", &path)
                .env("TICKET20_FAKE_ROOT", &root.0)
                // The JS 800-2000 ms pre-send pause is covered by unit tests; no real seconds here.
                .env("HER_STICKER_PAUSE_MS", "0")
                .current_dir(root.p("cwd"))
                .stdin(Stdio::null())
                .stdout(fs::File::create(root.p("console.stdout.log")).unwrap())
                .stderr(fs::File::create(root.p("console.stderr.log")).unwrap());
            hide(&mut command);
            command.spawn().expect("start console")
        });
        let mut console = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(console.child.try_wait().unwrap().is_none(), "console exited early");
            if http(port, "GET", "/api/health", None, None, &Value::Null).map(|r| r.0 == 200).unwrap_or(false) {
                return console;
            }
            assert!(Instant::now() < deadline, "console did not become healthy");
            thread::sleep(Duration::from_millis(30));
        }
    }
    fn call(&self, method: &str, path: &str, agent: bool, body: Value) -> (u16, Value) {
        http(self.port, method, path, Some(CONSOLE), agent.then_some(AGENT), &body).unwrap()
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    console: Option<&str>,
    agent: Option<&str>,
    body: &Value,
) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = if body.is_null() { String::new() } else { body.to_string() };
    let mut headers = String::new();
    if let Some(v) = console {
        headers += &format!("X-Console-Token: {v}\r\n");
    }
    if let Some(v) = agent {
        headers += &format!("X-Agent-Token: {v}\r\n");
    }
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let raw = String::from_utf8(raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no header separator")?;
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).ok_or("bad status")?;
    Ok((status, serde_json::from_str(body).map_err(|e| format!("{e}: {body}"))?))
}

struct OneBot(crate::support::FakeGateway);

impl std::ops::Deref for OneBot {
    type Target = crate::support::FakeGateway;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl OneBot {
    fn start(rows: Vec<Value>) -> Self {
        let this = Self(crate::support::FakeGateway::start());
        this.reply("add_custom_face", json!({"status":"ok","retcode":0,"data":{"emoji_id":"collected-1"}}));
        this.set_rows(rows);
        this
    }
    fn set_rows(&self, rows: Vec<Value>) {
        self.reply("fetch_custom_face_detail", json!({"status":"ok","retcode":0,"data":rows}));
    }
}

fn row(id: &str, file: &str, desc: &str) -> Value {
    json!({"emoji_id": id, "url": format!("http://8.8.8.8/{file}"), "desc": desc})
}
fn local(id: &str, file: &str) -> Value {
    json!({"id": id, "source":"qq", "url": format!("http://8.8.8.8/{file}"), "desc":"cat",
           "localNote":"kept note", "tags":["kept"], "useCount":7})
}
fn outbox(root: &Root) -> Vec<Value> {
    let dir = root.p("data");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("outbox") {
            for l in fs::read_to_string(entry.path()).unwrap_or_default().lines() {
                if let Ok(v) = serde_json::from_str::<Value>(l) {
                    out.push(v);
                }
            }
        }
    }
    out
}
fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
#[allow(dead_code)]
fn exists(p: &Path) -> bool {
    p.exists()
}

/// Gap batch small-parity item 2 (sticker send): the quote preview is the quoted message's own words
/// through `segmentsToText`: `@name` from the member lookup and the JS media labels.
#[test]
fn sticker_quote_preview_renders_like_segments_to_text() {
    let image = png(10, 200, 30);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture("send-quote", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/a.png"), &image).unwrap();
    ob.reply("get_msg", json!({"status":"ok","retcode":0,"data":{
        "group_id": GROUP, "user_id": 711, "message_id": 4242,
        "sender": {"user_id": 711, "card": "引用者"},
        "message": [
            {"type":"text","data":{"text":"看 "}}, {"type":"at","data":{"qq":"822"}},
            {"type":"image","data":{}}, {"type":"face","data":{"id":14}}, {"type":"json","data":{}}
        ]}}));
    ob.reply("get_group_member_info", json!({"status":"ok","retcode":0,"data":{"card":"被艾特的名片","nickname":"n"}}));
    let console = Console::start(&root);
    let (status, result) = console.call("POST", "/api/socialV2/send-sticker", true,
        json!({"key": KEY, "stickerId": "stk1", "replyToMessageId": "4242"}));
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["quoted"]["text"], "看 @被艾特的名片[图片][表情14][卡片消息]");
}

#[test]
fn model_sends_a_historic_favorite_and_the_use_survives_restart() {
    let image = png(10, 200, 30);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture("send", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/a.png"), &image).unwrap();
    let legacy_before = legacy_snapshot(&root);
    let console = Console::start(&root);

    let body = json!({"key": KEY, "stickerId": "stk1"});
    let (status, _) = http(console.port, "POST", "/api/socialV2/send-sticker", Some(CONSOLE), Some("wrong"), &body).unwrap();
    assert_eq!(status, 403);
    assert_eq!(ob.count("send_group_msg"), 0, "bad token must not send");

    let (status, result) = console.call("POST", "/api/socialV2/send-sticker", true, body);
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["ok"], true);
    assert_eq!(result["stickerId"], "stk1");
    let sent = ob.seen("send_group_msg");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["group_id"], GROUP);
    assert_eq!(sent[0]["message"], json!([{"type":"image","data":{"file": format!("base64://{}", b64(&image))}}]));

    let stickers = root.json("data/stickers.json");
    assert_eq!(stickers[0]["useCount"], 8);
    assert_eq!(stickers[0]["localNote"], "kept note");
    let recent = &root.json("data/social-v2.json")["conversations"][KEY]["recentMessages"];
    assert_eq!(recent[0]["text"], "[表情包:cat]");
    assert_eq!(recent[0]["sticker"]["id"], "stk1");
    let records = outbox(&root);
    assert!(records.iter().any(|r| r["k"] == "confirmed"), "{records:?}");

    drop(console);
    let console = Console::start(&root);
    let (status, list) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(status, 200);
    let item = list["stickers"].as_array().unwrap().iter().find(|s| s["id"] == "stk1").unwrap();
    assert_eq!(item["useCount"], 8);
    assert_eq!(legacy_snapshot(&root), legacy_before, "default production paths were touched");
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn send_with_missing_image_or_removed_favorite_sends_nothing_and_changes_nothing() {
    let ob = OneBot::start(vec![row("stk1", "gone.png", "cat")]);
    let root = fixture(
        "send-missing",
        &ob.url,
        json!({"enabled": true}),
        json!([local("stk1", "gone.png"), json!({"id":"old","source":"qq","url":"http://8.8.8.8/old.png"})]),
    );
    let console = Console::start(&root);
    let state_before = fs::read(root.p("data/social-v2.json")).unwrap();

    // The CDN no longer serves the picture: nothing is sent, the quota slot is given back.
    let (status, result) = console.call("POST", "/api/socialV2/send-sticker", true, json!({"key": KEY, "stickerId": "stk1"}));
    assert_eq!(status, 500, "{result}");
    assert!(result["error"].as_str().unwrap().contains("图片下载失败"), "{result}");
    assert_eq!(ob.count("send_group_msg"), 0);
    let state = root.json("data/social-v2.json");
    assert_eq!(state["conversations"][KEY]["sendTimes"], json!([]));
    assert_eq!(state["conversations"][KEY]["recentMessages"], json!([]));
    assert_eq!(root.json("data/stickers.json")[0]["useCount"], 7);

    // "old" is no longer a favorite in QQ: the forced refresh before sending drops it.
    let (status, result) = console.call("POST", "/api/socialV2/send-sticker", true, json!({"key": KEY, "stickerId": "old"}));
    assert_eq!(status, 500, "{result}");
    assert!(result["error"].as_str().unwrap().contains("找不到表情 old"), "{result}");
    assert_eq!(ob.count("send_group_msg"), 0);
    assert!(root.json("data/stickers.json").as_array().unwrap().iter().all(|s| s["id"] != "old"));
    let _ = state_before;
}

#[test]
fn startup_warms_the_sticker_library_once_and_starts_the_ttl() {
    let ob = OneBot::start(vec![row("stk1", "a.png", "fresh")]);
    let root = fixture("warmup", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    let console = Console::start(&root);
    wait_until("startup warm-up fetch", || ob.count("fetch_custom_face_detail") == 1);
    wait_until("warm-up merge", || root.json("data/stickers.json")[0]["desc"] == "fresh");
    let entry = root.json("data/stickers.json")[0].clone();
    assert_eq!(entry["localNote"], "kept note");
    assert_eq!(entry["useCount"], 7);
    // The warm-up counts as a sync: a list inside the TTL is served from cache.
    let (status, list) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(status, 200);
    assert_eq!(list["fromCache"], true);
    assert_eq!(ob.count("fetch_custom_face_detail"), 1);
    drop(console);
}

#[test]
fn startup_warmup_is_skipped_when_disabled_and_never_blocks_when_it_fails() {
    let ob = OneBot::start(vec![]);
    let root = fixture("warmup-off", &ob.url, json!({"enabled": false}), json!([local("stk1", "a.png")]));
    let console = Console::start(&root);
    thread::sleep(Duration::from_millis(600));
    assert_eq!(ob.count("fetch_custom_face_detail"), 0);
    drop(console);

    let ob = OneBot::start(vec![]);
    ob.reply("fetch_custom_face_detail", json!({"status":"failed","retcode":1200,"wording":"fixture down"}));
    let root = fixture("warmup-fail", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    let before = fs::read(root.p("data/stickers.json")).unwrap();
    let console = Console::start(&root);
    wait_until("failed warm-up attempt", || ob.count("fetch_custom_face_detail") == 1);
    assert_eq!(fs::read(root.p("data/stickers.json")).unwrap(), before, "a failed sync never touches the store");
    // A failed warm-up leaves no cache: the first list tries again and serves the cached store.
    let (status, list) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(status, 200);
    assert_eq!(list["stickers"][0]["id"], "stk1");
    assert_eq!(ob.count("fetch_custom_face_detail"), 2);
    drop(console);
}

#[test]
fn model_reads_a_favorite_image_and_gets_js_errors_for_missing_pictures() {
    let image = png(200, 30, 90);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat"), row("stk2", "gone.png", "dog")]);
    let root = fixture("image", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/a.png"), &image).unwrap();
    let console = Console::start(&root);
    let path = |id: &str| format!("/api/socialV2/sticker-image?key=group%3A70001&stickerId={id}");

    let (status, ok) = console.call("GET", &path("stk1"), true, Value::Null);
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["sticker"]["id"], "stk1");
    assert_eq!(ok["sticker"]["localNote"], "kept note");
    assert_eq!(ok["image"]["mimeType"], "image/png");
    assert_eq!(ok["image"]["data"], b64(&image));

    // Known favorite whose CDN picture is gone.
    let (status, bad) = console.call("GET", &path("stk2"), true, Value::Null);
    assert_eq!(status, 500, "{bad}");
    assert!(bad["error"].as_str().unwrap().starts_with("获取表情图片失败："), "{bad}");

    // Unknown id: one forced refresh is attempted, then the JS error.
    let before = ob.count("fetch_custom_face_detail");
    let (status, bad) = console.call("GET", &path("nope"), true, Value::Null);
    assert_eq!(status, 500, "{bad}");
    assert!(bad["error"].as_str().unwrap().contains("找不到表情 nope"), "{bad}");
    assert!(ob.count("fetch_custom_face_detail") > before);
}

fn chat_message(id: &str, media: Value, is_self: bool) -> Value {
    json!({"messageId": id, "seq": 5, "sender": "friend", "text": "[图片]", "plain": "[图片]",
           "isSelf": is_self, "media": media, "time": 1})
}

#[test]
fn model_collects_a_chat_sticker_with_remark_and_it_becomes_usable() {
    let image = png(1, 2, 3);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat"), row("collected-1", "c.png", "funny")]);
    let root = fixture("collect", &ob.url, json!({"enabled": true, "collect": {"enabled": true}}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/chat.png"), &image).unwrap();
    let mut state = root.json("data/social-v2.json");
    state["conversations"][KEY]["recentMessages"] = json!([
        chat_message("m100", json!([{"kind":"image","url":"http://8.8.8.8/chat.png"}]), false),
        chat_message("m101", json!([{"kind":"image","url":"http://8.8.8.8/chat.png"}]), true),
        chat_message("m102", json!([]), false),
    ]);
    fs::write(root.p("data/social-v2.json"), state.to_string()).unwrap();
    let console = Console::start(&root);

    for (id, needle) in [("m101", "不能收藏自己发的表情"), ("m102", "没有可收藏的图片"), ("zzz", "找不到这条消息")] {
        let (status, bad) = console.call("POST", "/api/socialV2/collect-sticker", true, json!({"key": KEY, "messageId": id}));
        assert_eq!(status, 500, "{bad}");
        assert!(bad["error"].as_str().unwrap().contains(needle), "{bad}");
    }
    assert_eq!(ob.count("add_custom_face"), 0);

    let (status, ok) = console.call("POST", "/api/socialV2/collect-sticker", true, json!({"key": KEY, "messageId": "m100", "remark": "funny"}));
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["emojiId"], "collected-1");
    assert_eq!(ok["remark"], "funny");
    assert_eq!(ok["sticker"]["id"], "collected-1");
    assert_eq!(ob.seen("add_custom_face"), vec![json!({"file": format!("base64://{}", b64(&image))})]);
    assert_eq!(ob.seen("modify_custom_face"), vec![json!({"emoji_id": "collected-1", "desc": "funny"})]);
    let stickers = root.json("data/stickers.json");
    assert!(stickers.as_array().unwrap().iter().any(|s| s["id"] == "collected-1" && s["desc"] == "funny"));
    assert!(stickers.as_array().unwrap().iter().any(|s| s["id"] == "stk1" && s["localNote"] == "kept note"));
    let conv = &root.json("data/social-v2.json")["conversations"][KEY];
    assert_eq!(conv["stickerCollectTimes"].as_array().unwrap().len(), 1);
}

#[test]
fn model_lists_with_agent_force_throttle_and_edits_notes_and_remarks_through_v2_routes() {
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture("model-edit", &ob.url, json!({"enabled": true, "syncTtlMs": 600000}), json!([local("stk1", "a.png")]));
    let console = Console::start(&root);
    wait_until("warm-up", || ob.count("fetch_custom_face_detail") == 1);

    let list = |refresh: bool| console.call("POST", "/api/socialV2/sticker-list", true, json!({"key": KEY, "refresh": refresh}));
    let (status, cached) = list(false);
    assert_eq!((status, &cached["fromCache"]), (200, &json!(true)));
    let (status, forced) = list(true);
    assert_eq!((status, &forced["fromCache"]), (200, &json!(false)));
    assert_eq!(ob.count("fetch_custom_face_detail"), 2);
    let (status, throttled) = list(true);
    assert_eq!((status, &throttled["fromCache"]), (200, &json!(true)));
    assert_eq!(ob.count("fetch_custom_face_detail"), 2, "agent force is throttled for 10 s");
    assert_eq!(throttled["stickers"][0]["localNote"], "kept note");

    let note = |token: Option<&str>, body: Value| {
        http(console.port, "POST", "/api/socialV2/sticker-note", Some(CONSOLE), token, &body).unwrap()
    };
    let body = json!({"key": KEY, "stickerId": "stk1", "note": "agent note", "tags": ["a", " b "], "usage": "greet"});
    assert_eq!(note(Some("wrong"), body.clone()).0, 403);
    assert_eq!(root.json("data/stickers.json")[0]["localNote"], "kept note");
    let (status, ok) = note(Some(AGENT), body);
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["sticker"]["localNote"], "agent note");
    assert_eq!(ok["sticker"]["tags"], json!(["a", "b"]));
    assert_eq!(ok["sticker"]["usage"], "greet");
    assert_eq!(ok["sticker"]["useCount"], 7);
    let (status, missing) = note(Some(AGENT), json!({"key": KEY, "stickerId": "nope", "note": "x"}));
    assert_eq!(status, 404, "{missing}");

    let (status, ok) = console.call("POST", "/api/socialV2/sticker-remark", true, json!({"key": KEY, "stickerId": "stk1", "remark": "new name"}));
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["sticker"]["desc"], "new name");
    assert_eq!(ob.seen("modify_custom_face"), vec![json!({"emoji_id": "stk1", "desc": "new name"})]);

    drop(console);
    // QQ itself now reports the new remark, as the real gateway would after modify_custom_face.
    ob.set_rows(vec![row("stk1", "a.png", "new name")]);
    let console = Console::start(&root);
    let (_, after) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(after["stickers"][0]["localNote"], "agent note");
    assert_eq!(after["stickers"][0]["tags"], json!(["a", "b"]));
    assert_eq!(after["stickers"][0]["desc"], "new name");
}

#[test]
fn concurrent_send_note_and_refresh_lose_no_update_and_survive_restart() {
    let image = png(9, 9, 9);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture(
        "concurrent",
        &ob.url,
        json!({"enabled": true, "syncTtlMs": 600000}),
        json!([local("stk1", "a.png"), json!({"id":"stk2","source":"qq","url":"http://8.8.8.8/b.png","desc":"two","useCount":1})]),
    );
    fs::write(root.p("images/a.png"), &image).unwrap();
    let console = Arc::new(Console::start(&root));
    wait_until("warm-up", || ob.count("fetch_custom_face_detail") == 1);
    // stk2 vanished from QQ and a new favorite appeared while the send is in flight.
    ob.set_rows(vec![row("stk1", "a.png", "cat"), row("stk3", "c.png", "three")]);
    ob.delay("send_group_msg", 1200);

    let sender = {
        let console = console.clone();
        thread::spawn(move || console.call("POST", "/api/socialV2/send-sticker", true, json!({"key": KEY, "stickerId": "stk1"})))
    };
    wait_until("send request in flight", || ob.count("send_group_msg") == 1);
    // Each writer touches a different field of the same entry: a stale read-modify-write that
    // replaced the whole entry would silently revert at least one of the others.
    let mut workers = Vec::new();
    for edit in [
        json!({"stickerId": "stk1", "note": "note-A"}),
        json!({"stickerId": "stk1", "tags": ["tag-B"]}),
        json!({"stickerId": "stk1", "usage": "usage-C"}),
    ] {
        let console = console.clone();
        workers.push(thread::spawn(move || console.call("POST", "/api/stickers/note", false, edit)));
    }
    let refresher = {
        let console = console.clone();
        thread::spawn(move || console.call("POST", "/api/stickers/sync", false, json!({})))
    };
    let (status, sent) = sender.join().unwrap();
    assert_eq!(status, 200, "{sent}");
    for worker in workers {
        assert_eq!(worker.join().unwrap().0, 200);
    }
    assert_eq!(refresher.join().unwrap().0, 200);

    let check = |root: &Root| {
        let stickers = root.json("data/stickers.json");
        let stickers = stickers.as_array().unwrap();
        let one = stickers.iter().find(|s| s["id"] == "stk1").unwrap();
        assert_eq!(one["useCount"], 8, "send count lost: {one}");
        assert_eq!(one["localNote"], "note-A", "note edit lost: {one}");
        assert_eq!(one["tags"], json!(["tag-B"]), "tags edit lost: {one}");
        assert_eq!(one["usage"], "usage-C", "usage edit lost: {one}");
        assert!(stickers.iter().any(|s| s["id"] == "stk3"), "refresh lost");
        assert!(stickers.iter().all(|s| s["id"] != "stk2"), "removed favorite kept");
        let conv = &root.json("data/social-v2.json")["conversations"][KEY];
        assert_eq!(conv["recentMessages"].as_array().unwrap().len(), 1);
    };
    check(&root);
    drop(console);
    let console = Console::start(&root);
    let (_, list) = console.call("GET", "/api/stickers", false, Value::Null);
    assert!(list["stickers"].as_array().unwrap().iter().any(|s| s["id"] == "stk3"));
    check(&root);
}

#[test]
fn a_sticker_never_overtakes_an_in_flight_text_burst() {
    let image = png(1, 2, 3);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture("order", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/a.png"), &image).unwrap();
    let mut config = root.json("config/config.json");
    config["socialV2"]["send"] = json!({"maxSendPerMinute": 0, "maxSendPerHour": 0, "firstGapEnabled": false,
                                        "burstIntervalMinMs": 300, "maxGapMs": 5000, "mergeMultiIntoOne": false});
    fs::write(root.p("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let console = Arc::new(Console::start(&root));
    wait_until("warm-up", || ob.count("fetch_custom_face_detail") == 1);

    // Two bubbles 900 ms apart; the sticker request arrives right after the first one is out.
    let text = {
        let console = console.clone();
        thread::spawn(move || {
            console.call(
                "POST",
                "/api/socialV2/send-message",
                true,
                json!({"key": KEY, "messages": ["第一条", "第二条"], "gapMode": "fixed", "gaps": [900]}),
            )
        })
    };
    wait_until("first text bubble", || ob.count("send_group_msg") == 1);
    let (status, sticker) = console.call("POST", "/api/socialV2/send-sticker", true, json!({"key": KEY, "stickerId": "stk1"}));
    assert_eq!(status, 200, "{sticker}");
    let (status, burst) = text.join().unwrap();
    assert_eq!(status, 200, "{burst}");

    let kinds: Vec<&str> = ob
        .seen("send_group_msg")
        .iter()
        .map(|body| if body["message"][0]["type"] == "image" { "sticker" } else { "text" })
        .collect();
    assert_eq!(kinds, ["text", "text", "sticker"], "the sticker jumped the in-flight burst");
}

#[test]
fn expired_cache_refresh_reflects_changed_and_removed_favorites_but_keeps_local_knowledge() {
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat"), row("stk2", "b.png", "dog")]);
    let root = fixture(
        "expiry",
        &ob.url,
        json!({"enabled": true, "syncTtlMs": 1500}),
        json!([local("stk1", "a.png"), json!({"id":"manual","source":"manual","desc":"mine","localNote":"manual note"})]),
    );
    let console = Console::start(&root);
    wait_until("warm-up", || ob.count("fetch_custom_face_detail") == 1);
    ob.set_rows(vec![row("stk1", "a.png", "renamed")]);
    let (_, fresh) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(fresh["fromCache"], true);
    assert!(fresh["stickers"].as_array().unwrap().iter().any(|s| s["id"] == "stk2"));
    thread::sleep(Duration::from_millis(1700));
    let (_, expired) = console.call("GET", "/api/stickers", false, Value::Null);
    assert_eq!(expired["fromCache"], false);
    let ids: Vec<_> = expired["stickers"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.contains(&"stk1".to_string()) && ids.contains(&"manual".to_string()), "{ids:?}");
    assert!(!ids.contains(&"stk2".to_string()), "removed favorite: {ids:?}");
    let one = expired["stickers"].as_array().unwrap().iter().find(|s| s["id"] == "stk1").unwrap();
    assert_eq!((one["desc"].as_str(), one["localNote"].as_str(), one["useCount"].as_i64()), (Some("renamed"), Some("kept note"), Some(7)));
    assert_eq!(ob.count("fetch_custom_face_detail"), 2);
}

#[test]
fn model_reads_the_selected_root_portrait_each_time_and_never_the_default_one() {
    let ob = OneBot::start(vec![]);
    let root = fixture("portrait", &ob.url, json!({"enabled": true}), Value::Null);
    let assets = root.p("resources/assets");
    fs::create_dir_all(&assets).unwrap();
    let first = png(250, 10, 10);
    fs::write(assets.join("deepseek娘.png"), &first).unwrap();
    let console = Console::start(&root);
    let path = "/api/socialV2/self-image?key=group%3A70001";

    let (status, ok) = console.call("GET", path, true, Value::Null);
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["image"]["mimeType"], "image/png");
    assert_eq!(ok["image"]["data"], b64(&first));
    // A new portrait written under the selected root is served by the next request, after a restart too.
    let second = png(10, 10, 250);
    fs::write(assets.join("deepseek娘.png"), &second).unwrap();
    assert_eq!(console.call("GET", path, true, Value::Null).1["image"]["data"], b64(&second));
    drop(console);
    let console = Console::start(&root);
    assert_eq!(console.call("GET", path, true, Value::Null).1["image"]["data"], b64(&second));
    // Wrong agent token and disabled tool are refused before any file is read.
    assert_eq!(http(console.port, "GET", path, Some(CONSOLE), Some("wrong"), &Value::Null).unwrap().0, 403);
    fs::remove_file(assets.join("deepseek娘.png")).unwrap();
    let (status, gone) = console.call("GET", path, true, Value::Null);
    assert_eq!(status, 404, "{gone}");
    assert!(gone["error"].as_str().unwrap().contains("未找到 AI 形象图片"));
}

struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<Value>,
    next: i64,
}
impl Mcp {
    fn start(root: &Root, console_port: u16) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_her"))
            .args(["--mcp-serve", "--runtime-root"])
            .arg(&root.0)
            .args(["--console-port", &console_port.to_string()])
            .env_remove("HER_RUNTIME_ROOT")
            .env_remove("QQ_SESSION_KEY")
            .env_remove("QQ_SESSION_TOKEN")
            .current_dir(root.p("cwd"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        let mut mcp = Self { child, stdin, lines, next: 0 };
        let init = mcp.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}));
        assert!(init["result"]["serverInfo"]["name"].is_string());
        writeln!(mcp.stdin, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
        mcp
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        writeln!(self.stdin, "{}", json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap();
        loop {
            let answer = self.lines.recv_timeout(Duration::from_secs(30)).expect("mcp answer");
            if answer["id"] == id {
                return answer;
            }
        }
    }
    fn call(&mut self, tool: &str, mut args: Value) -> Value {
        args["key"] = json!(KEY);
        args["token"] = json!(AGENT);
        self.request("tools/call", json!({"name": tool, "arguments": args}))
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn blocks<'a>(answer: &'a Value, kind: &str) -> Vec<&'a Value> {
    answer["result"]["content"].as_array().unwrap().iter().filter(|b| b["type"] == kind).collect()
}

#[test]
fn model_tools_list_view_note_send_and_read_portrait_through_mcp() {
    let favorite = png(40, 160, 220);
    let portrait = png(220, 160, 40);
    let ob = OneBot::start(vec![row("stk1", "a.png", "cat")]);
    let root = fixture("mcp", &ob.url, json!({"enabled": true}), json!([local("stk1", "a.png")]));
    fs::write(root.p("images/a.png"), &favorite).unwrap();
    fs::create_dir_all(root.p("resources/assets")).unwrap();
    fs::write(root.p("resources/assets/deepseek娘.png"), &portrait).unwrap();
    let console = Console::start(&root);
    let mut mcp = Mcp::start(&root, console.port);

    let list = mcp.call("qq_list_stickers", json!({}));
    assert_ne!(list["result"]["isError"], true, "{list}");
    assert!(list.to_string().contains("stk1") && list.to_string().contains("kept note"), "{list}");

    let view = mcp.call("qq_get_sticker_image", json!({"stickerId": "stk1"}));
    assert_ne!(view["result"]["isError"], true, "{view}");
    let images = blocks(&view, "image");
    assert_eq!(images.len(), 1, "{view}");
    assert_eq!(images[0]["data"], b64(&favorite));

    let note = mcp.call("qq_sticker_note", json!({"stickerId": "stk1", "note": "model note", "tags": ["x"]}));
    assert_ne!(note["result"]["isError"], true, "{note}");
    assert_eq!(root.json("data/stickers.json")[0]["localNote"], "model note");

    let sent = mcp.call("qq_send_sticker", json!({"stickerId": "stk1"}));
    assert_ne!(sent["result"]["isError"], true, "{sent}");
    assert_eq!(ob.count("send_group_msg"), 1);
    assert_eq!(root.json("data/stickers.json")[0]["useCount"], 8);

    let me = mcp.call("qq_get_self_image", json!({}));
    assert_ne!(me["result"]["isError"], true, "{me}");
    let images = blocks(&me, "image");
    assert_eq!(images.len(), 1, "{me}");
    assert_eq!(images[0]["data"], b64(&portrait));
}
