//! QQ voice core (ticket 21): the audio rules of the frozen JS `src/voice-core.js` and the voice
//! limits of `src/bridge.js`, without any HTTP or runtime-state dependency.
//!
//! External tools keep the JS contract: `ffprobe` (`FFPROBE_PATH` or PATH) writes its JSON to a
//! temporary file and `ffmpeg` (`FFMPEG_PATH`, `socialV2.voice.ffmpegPath` or PATH) writes a
//! processed copy for volume/loudness; both run without a console window and with null stdio.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Audio extensions the gateway takes directly (it converts to SILK itself).
pub const AUDIO_EXTS: [&str; 13] = [
    ".mp3", ".wav", ".ogg", ".oga", ".opus", ".m4a", ".aac", ".flac", ".amr", ".silk", ".slk", ".wma", ".spx",
];
pub const DEFAULT_MAX_BYTES: u64 = 20 * 1024 * 1024;
/// QQ non-member duration ceiling, used for both the default and the configured upper bound.
pub const DEFAULT_MAX_SECONDS: f64 = 60.0;
pub const VOLUME_MIN: f64 = 0.1;
pub const VOLUME_MAX: f64 = 4.0;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);

/// Node `path.extname` of the last path component.
pub fn extname(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rfind('.') {
        Some(0) | None => "",
        Some(index) => &base[index..],
    }
}

pub fn is_audio_file(name: &str) -> bool {
    let ext = extname(name).to_lowercase();
    AUDIO_EXTS.contains(&ext.as_str())
}

/// JS `Number.prototype.toFixed` for a non-negative finite value: exact decimal rounding where an
/// exact tie rounds up (Rust's formatter rounds a tie to even).
pub(crate) fn js_to_fixed(value: f64, digits: usize) -> String {
    let exact = format!("{value:.60}");
    let (_, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let tail = fraction.get(digits..).unwrap_or("");
    let is_tie = tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0');
    let adjusted = if is_tie { f64::from_bits(value.to_bits() + 1) } else { value };
    format!("{adjusted:.digits$}")
}

pub fn format_bytes(bytes: f64) -> String {
    let v = if bytes.is_finite() && bytes != 0.0 { bytes } else { 0.0 };
    if v < 1024.0 {
        format!("{} B", crate::qq_tools::js_number(v))
    } else if v < 1024.0 * 1024.0 {
        format!("{} KB", js_to_fixed(v / 1024.0, 1))
    } else {
        format!("{} MB", js_to_fixed(v / 1024.0 / 1024.0, 2))
    }
}

pub fn format_duration(seconds: f64) -> String {
    let s = if seconds.is_finite() { seconds.round().max(0.0) as u64 } else { 0 };
    let (m, r) = (s / 60, s % 60);
    if m > 0 {
        format!("{m}分{r:02}秒")
    } else {
        format!("{r}秒")
    }
}

/// JS `a.localeCompare(b, 'zh-Hans-CN')` for library names: Han by pinyin first, then Latin.
pub fn compare_names(a: &str, b: &str) -> std::cmp::Ordering {
    crate::role::compare_names(a, b)
}

/// Normalize `.`/`..` without touching the filesystem (Node `path.resolve`).
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Windows `canonicalize` yields a `\\?\C:\...` verbatim path; the gateway wants the plain form.
pub fn plain_path(path: &Path) -> Result<String, String> {
    crate::subprocess::external_path(path)
}

/// Recursively list the audio files of a plain directory (`listAudioFiles`): hidden entries are
/// skipped, depth is limited to four directory levels, order is the library order.
pub fn list_audio_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(current: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(current) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            // Node's Dirent reports a symlink as neither file nor directory.
            if kind.is_dir() {
                walk(&path, depth + 1, out);
            } else if kind.is_file() && is_audio_file(&name) {
                out.push(path);
            }
        }
    }
    let root = normalize_lexically(dir);
    let mut out = Vec::new();
    walk(&root, 0, &mut out);
    out.sort_by(|a, b| {
        let rel = |p: &PathBuf| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        compare_names(&rel(a), &rel(b))
    });
    out
}

/// `safeJoinLibrary`: a library-relative path made absolute, refusing traversal and symlink escape.
pub fn safe_join_library(root: &Path, relative: &str) -> Option<PathBuf> {
    let root = normalize_lexically(root);
    let rel = relative.replace('\\', "/");
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let abs = normalize_lexically(&root.join(rel));
    if abs != root && !abs.starts_with(&root) {
        return None;
    }
    match (std::fs::canonicalize(&abs), std::fs::canonicalize(&root)) {
        (Ok(real), Ok(real_root)) => {
            if real != real_root && !real.starts_with(&real_root) {
                return None;
            }
            Some(real)
        }
        _ => Some(abs),
    }
}

/// `parseTargetKey`: `group:N` / `private:N` with N a positive integer without leading zero.
pub fn parse_target_key(key: &str) -> Option<(String, String)> {
    let key = key.trim();
    let (kind, id) = key.split_once(':')?;
    if (kind != "group" && kind != "private") || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || id.starts_with('0') {
        return None;
    }
    Some((kind.to_string(), id.to_string()))
}

// ----- source resolution ---------------------------------------------------------------------------------

pub struct Resolved {
    pub path: Option<PathBuf>,
    pub via: String,
    pub candidates: Vec<PathBuf>,
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// `resolveAudioSource`: absolute path / `file://` -> relative to `cwd` -> the same name in the
/// library -> a missing extension filled in -> fuzzy library match by base name.
pub fn resolve_audio_source(input: &str, voice_dir: Option<&Path>, cwd: &Path) -> Resolved {
    let raw = input.trim();
    if raw.is_empty() {
        return Resolved { path: None, via: "empty".into(), candidates: vec![] };
    }
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    let mut add = |path: PathBuf, via: &str| {
        let abs = if path.is_absolute() { normalize_lexically(&path) } else { normalize_lexically(&cwd.join(path)) };
        if !candidates.iter().any(|(p, _)| *p == abs) {
            candidates.push((abs, via.to_string()));
        }
    };
    let is_url = raw.len() >= 7 && raw[..7].eq_ignore_ascii_case("file://");
    if is_url {
        let rest = &raw[7..];
        let path_part = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
        if let Some(mut decoded) = percent_decode(path_part.split(['?', '#']).next().unwrap_or("")) {
            let b = decoded.as_bytes();
            if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
                decoded.remove(0);
            }
            add(PathBuf::from(decoded), "file-url");
        }
    } else if Path::new(raw).has_root() || raw.starts_with('~') {
        if let Some(rest) = raw.strip_prefix('~') {
            let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
            add(PathBuf::from(home).join(rest.trim_start_matches(['/', '\\'])), "absolute");
        } else {
            add(PathBuf::from(raw), "absolute");
        }
    } else {
        add(cwd.join(raw), "cwd");
    }
    let dir = voice_dir.map(normalize_lexically);
    if let Some(dir) = &dir {
        if !Path::new(raw).has_root() && !is_url {
            add(dir.join(raw), "voice-dir");
        }
    }
    for (path, via) in &candidates {
        if is_file(path) {
            return Resolved { path: Some(path.clone()), via: via.clone(), candidates: vec![] };
        }
    }
    if extname(raw).is_empty() {
        for (path, via) in &candidates {
            for ext in AUDIO_EXTS {
                let with_ext = PathBuf::from(format!("{}{ext}", path.to_string_lossy()));
                if is_file(&with_ext) {
                    return Resolved { path: Some(with_ext), via: format!("{via}+ext"), candidates: vec![] };
                }
            }
        }
    }
    let mut fuzzy = Vec::new();
    if let Some(dir) = &dir {
        let want = base_name(raw).to_lowercase();
        let want_no_ext = strip_ext(&want);
        for file in list_audio_files(dir) {
            let base = base_name(&file.to_string_lossy()).to_lowercase();
            if base == want || strip_ext(&base) == want_no_ext {
                fuzzy.push(file);
            }
        }
    }
    Resolved { path: None, via: "not-found".into(), candidates: fuzzy }
}

pub fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `name.replace(/\.[^.]+$/, '')`.
fn strip_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => &name[..i],
        _ => name,
    }
}

// ----- probing and inspection ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Inspect {
    pub ok: bool,
    pub size: Option<u64>,
    pub duration: Option<f64>,
    pub codec: String,
    pub format_name: String,
    pub sample_rate: f64,
    pub channels: f64,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

fn io_reason(program: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("spawn {program} ENOENT"),
        std::io::ErrorKind::PermissionDenied => format!("spawn {program} EACCES"),
        _ => error.to_string(),
    }
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{}-{:x}", std::process::id(), nanos / 1_000_000, nanos as u64 ^ (SEQ.fetch_add(1, Ordering::Relaxed) << 20))
}

enum Run {
    Exit(Option<i32>),
    SpawnFailed(std::io::Error),
    TimedOut,
}

/// Start a hidden child with null stdio and wait for it, killing it at the deadline.
fn run_tool(program: &str, args: &[String], timeout: Duration) -> Run {
    let external = |path: &str| crate::subprocess::external_path(path).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error));
    let (program, args) = match external(program).and_then(|program| args.iter().map(|arg| external(arg)).collect::<Result<Vec<_>, _>>().map(|args| (program, args))) {
        Ok(values) => values,
        Err(error) => return Run::SpawnFailed(error),
    };
    let mut command = crate::subprocess::command_without_console(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Run::SpawnFailed(error),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Run::Exit(status.code()),
            Ok(None) => {}
            Err(error) => return Run::SpawnFailed(error),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Run::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub struct ProbeOk {
    pub duration: f64,
    pub format_name: String,
    pub codec: String,
    pub sample_rate: f64,
    pub channels: f64,
}

fn number_of(value: Option<&Value>) -> f64 {
    let n = match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    if n.is_finite() {
        n
    } else {
        0.0
    }
}

/// `probeAudio`: ffprobe's JSON goes through a temporary file (restricted sandboxes forbid pipes).
pub fn probe_audio(file: &Path, ffprobe: &str, tmp_dir: Option<&Path>) -> Result<ProbeOk, String> {
    let mut dirs: Vec<PathBuf> = tmp_dir.map(Path::to_path_buf).into_iter().collect();
    dirs.push(std::env::temp_dir().join("voice-probe"));
    let dir = dirs
        .into_iter()
        .find(|d| std::fs::create_dir_all(d).is_ok() && std::fs::metadata(d).map(|m| !m.permissions().readonly()).unwrap_or(false))
        .ok_or_else(|| "没有可写的临时目录用于 ffprobe 输出".to_string())?;
    let out_file = dir.join(format!("p-{}.json", unique_suffix()));
    let args: Vec<String> = [
        "-v", "error", "-show_entries", "format=duration,format_name", "-show_entries",
        "stream=codec_name,codec_type,sample_rate,channels", "-of", "json", "-o",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([out_file.to_string_lossy().into_owned(), file.to_string_lossy().into_owned()])
    .collect();
    let result = (|| {
        let code = match run_tool(ffprobe, &args, PROBE_TIMEOUT) {
            Run::SpawnFailed(error) => return Err(format!("ffprobe 不可用：{}", io_reason(ffprobe, &error))),
            Run::TimedOut => return Err("ffprobe 超时".to_string()),
            Run::Exit(code) => code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
        };
        if code != "0" {
            return Err(format!("ffprobe 执行失败（退出码 {code}）"));
        }
        let raw = std::fs::read_to_string(&out_file)
            .map_err(|error| format!("ffprobe 未产出结果（退出码 {code}，{error}）"))?;
        if raw.trim().is_empty() {
            return Err(format!("ffprobe 结果为空（退出码 {code}）"));
        }
        let json: Value = serde_json::from_str(&raw).map_err(|error| format!("ffprobe 输出解析失败：{error}"))?;
        let duration = json.pointer("/format/duration").filter(|v| !v.is_null())
            .ok_or_else(|| "ffprobe 缺少时长元数据（format.duration）".to_string())?;
        let duration = match duration {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| "ffprobe 时长元数据无效（需为有限正数）".to_string())?;
        let audio = json
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.iter().find(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio")));
        let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
        Ok(ProbeOk {
            duration,
            format_name: text(json.pointer("/format/format_name")),
            codec: text(audio.and_then(|a| a.get("codec_name"))),
            sample_rate: number_of(audio.and_then(|a| a.get("sample_rate"))),
            channels: number_of(audio.and_then(|a| a.get("channels"))),
        })
    })();
    let _ = std::fs::remove_file(&out_file);
    result
}

pub fn ffprobe_path() -> String {
    std::env::var("FFPROBE_PATH").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "ffprobe".to_string())
}

/// Validate SILK v3 packets and report a safe duration upper bound, not a decoded duration.
/// Each packet holds up to five 20 ms frames; DTX packets also count as up to 100 ms.
fn probe_silk(file: &Path) -> Result<ProbeOk, String> {
    use std::io::{BufRead, Read};
    let parse = || -> Result<u64, String> {
        let input = std::fs::File::open(file).map_err(|e| format!("无法读取文件：{e}"))?;
        let mut input = std::io::BufReader::new(input);
        if input.fill_buf().map_err(|e| e.to_string())?.first() == Some(&0x02) {
            input.consume(1);
        }
        let mut header = [0; 9];
        input.read_exact(&mut header).map_err(|_| "文件头不完整".to_string())?;
        if &header != b"#!SILK_V3" {
            return Err("文件头不是 #!SILK_V3".into());
        }
        let mut packets = 0_u64;
        let mut payload = [0; i16::MAX as usize];
        while !input.fill_buf().map_err(|e| e.to_string())?.is_empty() {
            let mut length = [0; 2];
            input.read_exact(&mut length).map_err(|_| "帧长度不完整".to_string())?;
            let length = i16::from_le_bytes(length);
            // Standard SILK may end with -1; QQ files normally end after the last packet.
            if length == -1 && input.fill_buf().map_err(|e| e.to_string())?.is_empty() {
                break;
            }
            if length < 0 {
                return Err("帧长度无效".into());
            }
            // DTX silence has a zero-length payload but still occupies one packet.
            input.read_exact(&mut payload[..length as usize]).map_err(|_| "帧数据不完整".to_string())?;
            packets += 1;
        }
        if packets == 0 {
            return Err("没有音频帧".into());
        }
        Ok(packets)
    };
    let packets = parse().map_err(|reason| format!("SILK 格式无效：{reason}"))?;
    Ok(ProbeOk {
        duration: packets as f64 / 10.0,
        format_name: "silk".into(),
        codec: "silk".into(),
        sample_rate: 0.0,
        channels: 0.0,
    })
}

/// `inspectAudio`: is this file fit to be sent as a QQ voice?
pub fn inspect_audio(file: &Path, max_bytes: u64, max_seconds: f64, ffprobe: &str) -> Inspect {
    let mut warnings = Vec::new();
    let meta = match std::fs::metadata(file) {
        Ok(meta) => meta,
        Err(error) => {
            return Inspect { error: Some(format!("文件不存在或不可读：{error}")), ..Inspect::default() };
        }
    };
    if !meta.is_file() {
        return Inspect { error: Some("不是文件（目录？）".into()), ..Inspect::default() };
    }
    let size = meta.len();
    if size == 0 {
        return Inspect { error: Some("文件为空".into()), ..Inspect::default() };
    }
    if size > max_bytes {
        return Inspect {
            size: Some(size),
            error: Some(format!("文件过大（{} > 上限 {}）", format_bytes(size as f64), format_bytes(max_bytes as f64))),
            ..Inspect::default()
        };
    }
    let name = file.to_string_lossy();
    if !is_audio_file(&name) {
        let ext = extname(&name);
        warnings.push(format!("扩展名 {} 不在常见音频列表内，将交给网关自行识别", if ext.is_empty() { "(无)" } else { ext }));
    }
    let lower_ext = extname(&name).to_lowercase();
    let is_silk = lower_ext == ".silk" || lower_ext == ".slk";
    let probe = if is_silk {
        probe_silk(file)
    } else {
        probe_audio(file, ffprobe, None)
    };
    let probe = match probe {
        Ok(probe) => probe,
        Err(reason) => {
            return Inspect { size: Some(size), warnings, error: Some(format!("无法确定音频时长：{reason}")), ..Inspect::default() };
        }
    };
    if probe.duration > max_seconds {
        let error = if is_silk {
            "SILK 文件时长无法确认，请改用 mp3/wav/ogg 等常见格式".to_string()
        } else {
            format!("时长 {} 超过上限 {}", format_duration(probe.duration), format_duration(max_seconds))
        };
        return Inspect {
            size: Some(size),
            duration: Some(probe.duration),
            codec: probe.codec,
            error: Some(error),
            ..Inspect::default()
        };
    }
    if probe.duration > 60.0 {
        warnings.push(format!("时长 {} 较长，QQ 会显示为长语音", format_duration(probe.duration)));
    }
    if probe.codec.is_empty() {
        warnings.push("没找到音频流，网关转码可能失败".into());
    }
    Inspect {
        ok: true,
        size: Some(size),
        duration: Some(probe.duration),
        codec: probe.codec,
        format_name: probe.format_name,
        sample_rate: probe.sample_rate,
        channels: probe.channels,
        warnings,
        error: None,
    }
}

/// `describeAudio`.
pub fn describe_audio(info: &Inspect) -> String {
    let mut parts = Vec::new();
    if let Some(d) = info.duration.filter(|d| *d != 0.0) {
        parts.push(format_duration(d));
    }
    if let Some(s) = info.size.filter(|s| *s != 0) {
        parts.push(format_bytes(s as f64));
    }
    if !info.codec.is_empty() {
        parts.push(info.codec.clone());
    }
    if info.sample_rate != 0.0 {
        parts.push(format!("{}kHz", js_to_fixed(info.sample_rate / 1000.0, 1)));
    }
    if info.channels != 0.0 {
        parts.push(if info.channels == 1.0 { "单声道".to_string() } else { format!("{}声道", crate::qq_tools::js_number(info.channels)) });
    }
    if parts.is_empty() {
        "未知音频".to_string()
    } else {
        parts.join(" · ")
    }
}

/// A JSON number the way JS serializes it (an integral double has no fraction).
pub fn json_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

// ----- volume and loudness -----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Loudness {
    Number(f64),
    Name(String),
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    /// `Number(volume)`; `volume_text` is `String(volume)` for the error message.
    pub volume: f64,
    pub volume_text: String,
    pub normalize: bool,
    pub loudness: Loudness,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub volume: f64,
    pub loudness: Option<f64>,
}

/// `resolveAudioProcessing`: loudness normalization wins over a gain factor.
pub fn resolve_audio_processing(options: &AudioOptions) -> Result<Plan, String> {
    if options.normalize {
        return match &options.loudness {
            Loudness::Number(n) => {
                if !n.is_finite() || *n < -40.0 || *n > -5.0 {
                    Err(format!("目标响度需在 -40 ~ -5 LUFS 之间，收到 {}", crate::qq_tools::js_number(*n)))
                } else {
                    Ok(Plan { volume: 1.0, loudness: Some(*n) })
                }
            }
            Loudness::Name(name) => {
                let target = match name.as_str() {
                    "quiet" => -20.0,
                    "normal" => -16.0,
                    "loud" => -13.0,
                    _ => {
                        return Err(format!(
                            "未知的响度档位「{name}」（可选：quiet / normal / loud，或直接给 -40~-5 的 LUFS 数字）"
                        ))
                    }
                };
                Ok(Plan { volume: 1.0, loudness: Some(target) })
            }
        };
    }
    let v = options.volume;
    if !v.is_finite() {
        return Err(format!("音量必须是数字，收到「{}」", options.volume_text));
    }
    if !(VOLUME_MIN..=VOLUME_MAX).contains(&v) {
        return Err(format!(
            "音量倍数需在 {}~{} 之间（1.0 = 原样），收到 {}",
            crate::qq_tools::js_number(VOLUME_MIN),
            crate::qq_tools::js_number(VOLUME_MAX),
            crate::qq_tools::js_number(v)
        ));
    }
    Ok(Plan { volume: v, loudness: None })
}

/// `resolveFfmpeg`: an existing configured file, else an existing `FFMPEG_PATH`, else the bare command.
pub fn resolve_ffmpeg(configured: Option<&str>) -> String {
    let env = std::env::var("FFMPEG_PATH").ok();
    for candidate in [configured.map(str::to_string), env].into_iter().flatten().filter(|c| !c.is_empty()) {
        if is_file(Path::new(&candidate)) {
            return candidate;
        }
    }
    "ffmpeg".to_string()
}

pub struct Processed {
    pub path: PathBuf,
    pub converted: bool,
    pub volume: f64,
    pub loudness: Option<f64>,
    pub mode: &'static str,
    pub detail: String,
}

/// `processAudioVolume`: the original file when nothing changes, else an ffmpeg-made temp copy
/// the caller must delete (`converted`).
pub fn process_audio_volume(file: &Path, plan: &Plan, ffmpeg_configured: Option<&str>) -> Result<Processed, String> {
    if plan.loudness.is_none() && (plan.volume - 1.0).abs() < 1e-9 {
        return Ok(Processed {
            path: file.to_path_buf(),
            converted: false,
            volume: 1.0,
            loudness: None,
            mode: "original",
            detail: "原始音量".into(),
        });
    }
    let ffmpeg = resolve_ffmpeg(ffmpeg_configured);
    let dir = std::env::temp_dir().join("voice-processed");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let name = file.to_string_lossy();
    let ext = extname(&name).to_string();
    let mut out_path = dir.join(format!("vol-{}{}", unique_suffix(), if ext.is_empty() { ".wav" } else { &ext }));
    let filter = match plan.loudness {
        Some(target) => format!("loudnorm=I={}:TP=-1.5:LRA=11", crate::qq_tools::js_number(target)),
        None => format!("volume={}", crate::qq_tools::js_number(plan.volume)),
    };
    let args_for = |out: &Path| -> Vec<String> {
        ["-hide_banner", "-loglevel", "error", "-y", "-i"]
            .iter()
            .map(|s| s.to_string())
            .chain([name.to_string(), "-af".to_string(), filter.clone(), "-ac".to_string(), "1".to_string(), out.to_string_lossy().into_owned()])
            .collect()
    };
    enum Outcome {
        Code(Option<i32>),
        Error(String),
    }
    let run = |out: &Path, first: bool| -> Outcome {
        match run_tool(&ffmpeg, &args_for(out), FFMPEG_TIMEOUT) {
            Run::Exit(code) => Outcome::Code(code),
            Run::TimedOut => Outcome::Error("ffmpeg 超时".into()),
            Run::SpawnFailed(error) if first && error.kind() == std::io::ErrorKind::NotFound => Outcome::Error(
                "找不到 ffmpeg（请设置 FFMPEG_PATH 或在 config.json 里指定 socialV2.voice.ffmpegPath）".into(),
            ),
            Run::SpawnFailed(error) => Outcome::Error(format!("ffmpeg 不可用：{}", io_reason(&ffmpeg, &error))),
        }
    };
    let mut leftovers = vec![out_path.clone()];
    let mut result = run(&out_path, true);
    let failed = |result: &Outcome, out: &Path| !matches!(result, Outcome::Code(Some(0))) || !out.exists();
    if failed(&result, &out_path) && !ext.is_empty() && ext.to_lowercase() != ".wav" {
        // The original extension may have no encoder (.amr/.silk are read-only): retry as wav.
        let wav = out_path.with_extension("wav");
        leftovers.push(wav.clone());
        let retry = run(&wav, false);
        if matches!(retry, Outcome::Code(Some(0))) && wav.exists() {
            out_path = wav;
        }
        result = retry;
    }
    let cleanup = |keep: Option<&Path>| {
        for path in &leftovers {
            if Some(path.as_path()) != keep {
                let _ = std::fs::remove_file(path);
            }
        }
    };
    match result {
        Outcome::Error(message) => {
            cleanup(None);
            return Err(message);
        }
        Outcome::Code(code) if !matches!(code, Some(0)) || !out_path.exists() => {
            cleanup(None);
            return Err(format!("ffmpeg 音量处理失败（退出码 {}）", code.map(|c| c.to_string()).unwrap_or_else(|| "null".into())));
        }
        Outcome::Code(_) => {}
    }
    if std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0) == 0 {
        cleanup(None);
        return Err("ffmpeg 输出为空文件（源音频可能损坏）".into());
    }
    cleanup(Some(&out_path));
    let detail = match plan.loudness {
        Some(target) => format!("智能音量（响度归一 {} LUFS）", crate::qq_tools::js_number(target)),
        None if plan.volume > 1.0 => format!("音量提升至 {}%", (plan.volume * 100.0).round() as i64),
        None => format!("音量压低至 {}%", (plan.volume * 100.0).round() as i64),
    };
    Ok(Processed {
        path: out_path,
        converted: true,
        volume: plan.volume,
        loudness: plan.loudness,
        mode: if plan.loudness.is_some() { "loudnorm" } else { "volume" },
        detail,
    })
}

// ----- send budget ---------------------------------------------------------------------------------------------

fn budgets() -> &'static Mutex<HashMap<String, Vec<i64>>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<String, Vec<i64>>>> = std::sync::OnceLock::new();
    BUDGETS.get_or_init(Default::default)
}

/// `voiceBudgetCheck`: checks only; the quota is spent by `budget_commit` after a send.
pub fn budget_check(scope: &str, now: i64, per_minute: f64, per_hour: f64) -> Result<(), String> {
    let map = budgets().lock().unwrap_or_else(|p| p.into_inner());
    let bucket = map.get(scope).cloned().unwrap_or_default();
    let in_minute = bucket.iter().filter(|t| now - **t < 60_000).count() as f64;
    let in_hour = bucket.iter().filter(|t| now - **t < 3_600_000).count() as f64;
    if per_minute > 0.0 && in_minute + 1.0 > per_minute {
        return Err(format!("语音发送频率超限（每分钟最多 {} 条），请稍后再试", crate::qq_tools::js_number(per_minute)));
    }
    if per_hour > 0.0 && in_hour + 1.0 > per_hour {
        return Err(format!("语音发送频率超限（每小时最多 {} 条），请稍后再试", crate::qq_tools::js_number(per_hour)));
    }
    Ok(())
}

/// `voiceBudgetCommit`: spend one slot (older than an hour are dropped).
pub fn budget_commit(scope: &str, at: i64) {
    let mut map = budgets().lock().unwrap_or_else(|p| p.into_inner());
    let bucket = map.entry(scope.to_string()).or_default();
    bucket.retain(|t| at - *t < 3_600_000);
    bucket.push(at);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_match_js_including_exact_ties() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(2048.0), "2.0 KB");
        // 1280 / 1024 = 1.25 is an exact tie: JS toFixed rounds it up, Rust's {:.1} would give 1.2.
        assert_eq!(format_bytes(1280.0), "1.3 KB");
        assert_eq!(format_bytes(3.0 * 1024.0 * 1024.0), "3.00 MB");
        assert_eq!(format_duration(45.0), "45秒");
        assert_eq!(format_duration(95.0), "1分35秒");
        assert_eq!(format_duration(12.5), "13秒");
        assert_eq!(format_duration(f64::NAN), "0秒");
    }

    #[test]
    fn extensions_and_target_keys_follow_the_baseline() {
        assert!(is_audio_file("a.mp3") && is_audio_file("A.WAV") && is_audio_file("x.silk") && is_audio_file("y.spx"));
        assert!(!is_audio_file("a.txt") && !is_audio_file("a") && !is_audio_file(".mp3"));
        assert_eq!(parse_target_key(" group:123 "), Some(("group".into(), "123".into())));
        assert_eq!(parse_target_key("private:456"), Some(("private".into(), "456".into())));
        for bad in ["123", "group:abc", "group:0", "group:007", "friend:1", "group:"] {
            assert_eq!(parse_target_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn library_join_refuses_traversal_and_keeps_inside_paths() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("voice-join-{}", unique_suffix())));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.mp3"), b"x").unwrap();
        assert!(safe_join_library(&root, "sub/a.mp3").is_some());
        assert!(safe_join_library(&root, "/sub\\a.mp3").is_some(), "leading slash and backslashes are normalized");
        assert!(safe_join_library(&root, "../x.mp3").is_none());
        assert!(safe_join_library(&root, "sub/../../x.mp3").is_none());
        assert!(safe_join_library(&root, "").is_none());
        #[cfg(unix)]
        {
            let outside = crate::install_root::development_root(std::env::temp_dir().join(format!("voice-out-{}", unique_suffix())));
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("s.mp3"), b"x").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
            assert!(safe_join_library(&root, "link/s.mp3").is_none(), "symlink escape is refused");
            let _ = std::fs::remove_dir_all(&outside);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn processing_validation_messages_match_the_baseline() {
        let options = |volume: f64, text: &str, normalize: bool, loudness: Loudness| AudioOptions {
            volume,
            volume_text: text.into(),
            normalize,
            loudness,
        };
        assert_eq!(
            resolve_audio_processing(&options(2.0, "2", false, Loudness::Name("normal".into()))),
            Ok(Plan { volume: 2.0, loudness: None })
        );
        assert_eq!(resolve_audio_processing(&options(9.0, "9", false, Loudness::Name("normal".into()))).unwrap_err(), "音量倍数需在 0.1~4 之间（1.0 = 原样），收到 9");
        assert_eq!(resolve_audio_processing(&options(f64::NAN, "loud", false, Loudness::Name("normal".into()))).unwrap_err(), "音量必须是数字，收到「loud」");
        assert_eq!(
            resolve_audio_processing(&options(1.0, "1", true, Loudness::Name("loud".into()))),
            Ok(Plan { volume: 1.0, loudness: Some(-13.0) })
        );
        assert_eq!(resolve_audio_processing(&options(1.0, "1", true, Loudness::Number(-18.0))), Ok(Plan { volume: 1.0, loudness: Some(-18.0) }));
        assert_eq!(resolve_audio_processing(&options(1.0, "1", true, Loudness::Number(-3.0))).unwrap_err(), "目标响度需在 -40 ~ -5 LUFS 之间，收到 -3");
    }

    #[test]
    fn budget_check_does_not_spend_and_commit_does() {
        let scope = format!("unit-{}", unique_suffix());
        let now = 1_000_000;
        assert!(budget_check(&scope, now, 1.0, 10.0).is_ok());
        assert!(budget_check(&scope, now, 1.0, 10.0).is_ok(), "checking is free");
        budget_commit(&scope, now);
        assert_eq!(budget_check(&scope, now + 1, 1.0, 10.0).unwrap_err(), "语音发送频率超限（每分钟最多 1 条），请稍后再试");
        assert!(budget_check(&scope, now + 60_001, 1.0, 10.0).is_ok(), "a minute later the slot is free");
        assert_eq!(budget_check(&scope, now + 60_001, 0.0, 1.0).unwrap_err(), "语音发送频率超限（每小时最多 1 条），请稍后再试");
        assert!(budget_check(&scope, now + 1, 0.0, 0.0).is_ok(), "zero means unlimited");
    }

    #[test]
    fn plain_path_converts_drive_and_unc_paths_for_external_tools() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\a\b.mp3")).unwrap(), r"C:\a\b.mp3");
        assert_eq!(plain_path(Path::new(r"\\?\UNC\host\share\b.mp3")).unwrap(), r"\\host\share\b.mp3");
        assert_eq!(plain_path(Path::new("/tmp/a.mp3")).unwrap(), "/tmp/a.mp3");
    }
}
