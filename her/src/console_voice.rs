//! Voice surface of the console (ticket 21): the library list, the AI send route, the operator
//! send route and the prompt summary, ported from `bridge.js` (`voiceCfg`, `sendVoiceV2`,
//! `sendVoiceForV2`, `/api/socialV2/send-voice`, `/api/voice/send`, `/api/send/voice`,
//! `/api/socialV2/voices`, `/api/voice/list`).
//!
//! This module is a child of `console.rs` (`#[path]`) and uses that file's private helpers.
//!
//! Sends go through the durable send outbox like every other send (`send_outbox.rs`): an intent
//! (`kind: "voice"`) and an attempt are recorded before the OneBot request, and the outcome after
//! it. A lost or unparsable reply is `unknown`: the history is searched for the delivered voice,
//! and without evidence the item stays unknown and is never sent again by this bridge.

use super::*;
use crate::voice::{self, AudioOptions, Loudness};
use std::path::{Path, PathBuf};

const OPERATOR_SCOPE: &str = "operator:cli";
const NOT_FROM_LIBRARY: &str = "语音只接受语音库里的本地文件；不接受 base64://、data: 或 http(s):// 来源";

// ----- configuration ---------------------------------------------------------------------------------------

/// The merged `socialV2.voice` section: the JS config loader supplies `enabled: false` and the
/// other defaults, then `voiceCfg()` normalizes each member.
pub(super) struct VoiceCfg {
    pub enabled: bool,
    pub dir_rel: String,
    pub max_seconds: f64,
    pub max_bytes: f64,
    pub max_per_minute: f64,
    pub max_per_hour: f64,
    default_volume: f64,
    default_volume_text: String,
    loudness: Value,
    ffmpeg_path: Option<String>,
    auto_readback: bool,
}

// QQ's non-member ceiling also caps older configurations that allowed five-minute recordings.
fn max_seconds(value: Option<&Value>) -> f64 {
    let seconds = agent::js_number(value);
    let seconds = if seconds > 0.0 { seconds } else { voice::DEFAULT_MAX_SECONDS };
    if seconds > voice::DEFAULT_MAX_SECONDS {
        crate::safe_println!("[voice] socialV2.voice.maxSeconds={seconds} exceeds QQ limit; clamped to 60s");
    }
    seconds.min(voice::DEFAULT_MAX_SECONDS)
}

/// Keep config views and saved settings on the same duration limit as the send path.
pub(super) fn clamp_config_max_seconds(social: &mut Value) {
    if let Some(value) = social.pointer_mut("/voice/maxSeconds") {
        *value = voice::json_number(max_seconds(Some(value)));
    }
}

pub(super) fn voice_cfg(cfg: &Value) -> VoiceCfg {
    let section = cfg.pointer("/socialV2/voice").filter(|v| v.is_object());
    let get = |name: &str| section.and_then(|s| s.get(name));
    let positive_or = |name: &str, fallback: f64| {
        let n = agent::js_number(get(name));
        if n > 0.0 { n } else { fallback }
    };
    let non_negative_or = |name: &str, fallback: f64| {
        let n = agent::js_number(get(name));
        if n >= 0.0 { n } else { fallback }
    };
    let default_volume = positive_or("defaultVolume", 1.0);
    let default_volume_text = match get("defaultVolume") {
        Some(v) if default_volume != 1.0 || !v.is_null() => social_policy_js_string(v),
        _ => "1".to_string(),
    };
    let loudness = get("loudness")
        .filter(|v| match v {
            Value::Null | Value::Bool(false) => false,
            Value::String(s) => !s.is_empty(),
            Value::Number(n) => n.as_f64() != Some(0.0),
            _ => true,
        })
        .cloned()
        .unwrap_or_else(|| json!("normal"));
    let ffmpeg_path = std::env::var("FFMPEG_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| get("ffmpegPath").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string));
    VoiceCfg {
        // `enabled` is false unless the file says otherwise, and only an explicit `false` closes it again.
        enabled: get("enabled").map(|v| v != &json!(false)).unwrap_or(false),
        dir_rel: match get("dir") {
            None | Some(Value::Null) => "audio".to_string(),
            Some(v) => social_policy_js_string(v),
        },
        max_seconds: max_seconds(get("maxSeconds")),
        max_bytes: positive_or("maxBytes", voice::DEFAULT_MAX_BYTES as f64),
        max_per_minute: non_negative_or("maxPerMinute", 2.0),
        max_per_hour: non_negative_or("maxPerHour", 10.0),
        default_volume,
        default_volume_text,
        loudness,
        ffmpeg_path,
        auto_readback: get("autoReadback").map(|v| v != &json!(false)).unwrap_or(true),
    }
}

impl VoiceCfg {
    fn limits(&self) -> Value {
        json!({
            "maxSeconds": voice::json_number(self.max_seconds),
            "maxBytes": voice::json_number(self.max_bytes),
            "maxPerMinute": voice::json_number(self.max_per_minute),
            "maxPerHour": voice::json_number(self.max_per_hour),
        })
    }
}

/// The voice part of the JS config merge, for the console's config view: `socialV2.voice` gets its
/// defaults under the file's members and `tools.sendVoice` is off unless the file says otherwise
/// (the console checkbox reads `!== false`, so an absent flag would show voice as on).
pub(super) fn apply_config_defaults(social: &mut Value) {
    let Some(map) = social.as_object_mut() else { return };
    let mut voice = json!({
        "enabled": false, "dir": "audio", "maxSeconds": voice::json_number(voice::DEFAULT_MAX_SECONDS),
        "maxBytes": 20971520, "maxPerMinute": 2, "maxPerHour": 10,
        "allowAbsolutePath": false, "defaultVolume": 1, "loudness": "normal", "ffmpegPath": "", "autoReadback": true
    });
    if let (Some(defaults), Some(file)) = (voice.as_object_mut(), map.get("voice").and_then(Value::as_object)) {
        for (k, v) in file {
            defaults.insert(k.clone(), v.clone());
        }
    }
    map.insert("voice".to_string(), voice);
    let tools = map.entry("tools".to_string()).or_insert_with(|| json!({}));
    if let Some(tools) = tools.as_object_mut() {
        tools.entry("sendVoice".to_string()).or_insert(json!(false));
    }
    clamp_config_max_seconds(social);
}

/// `v2ToolEnabled('sendVoice')`: the JS config merge defaults the switch to false.
pub(super) fn send_voice_tool_enabled(cfg: &Value) -> bool {
    cfg.pointer("/socialV2/tools/sendVoice").map(|v| v != &json!(false)).unwrap_or(false)
}

fn loudness_arg(value: &Value) -> Loudness {
    match value {
        Value::Number(n) => Loudness::Number(n.as_f64().unwrap_or(f64::NAN)),
        other => Loudness::Name(social_policy_js_string(other)),
    }
}

/// Audio options from request members: `body.x ?? config default`.
fn audio_options(vc: &VoiceCfg, volume: Option<&Value>, normalize: bool, loudness: Option<&Value>) -> AudioOptions {
    let volume_value = volume.filter(|v| !v.is_null());
    AudioOptions {
        volume: volume_value.map(|v| agent::js_number(Some(v))).unwrap_or(vc.default_volume),
        volume_text: volume_value.map(social_policy_js_string).unwrap_or_else(|| vc.default_volume_text.clone()),
        normalize,
        loudness: loudness_arg(loudness.filter(|v| !v.is_null()).unwrap_or(&vc.loudness)),
    }
}

// ----- the library -----------------------------------------------------------------------------------------

struct Library {
    root: PathBuf,
    dir_rel: String,
    isolated: bool,
    cwd: PathBuf,
}

/// True when `dir_rel` names a directory that simply does not exist below the resources root (every
/// existing component is a real directory). A traversal, a link or a non-directory is not "missing".
fn library_dir_missing(resources: &Path, dir_rel: &str) -> bool {
    let mut candidate = resources.to_path_buf();
    for component in Path::new(dir_rel).components() {
        let std::path::Component::Normal(part) = component else { return false };
        candidate.push(part);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_dir() && !crate::guarded_file::is_link(&metadata) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            _ => return false,
        }
    }
    false
}

/// The library of the selected configuration. A missing directory is an empty library (JS
/// `listAudioFiles` swallows `readdir` errors) instead of a failure, so the operator route still sends
/// any file by path; a directory that escapes the resources area or is a link stays refused. Every
/// file still passes the contained resolver before it is read.
fn open_library(context: &RuntimeContext, vc: &VoiceCfg) -> Result<Library, (u16, String)> {
    if context.is_isolated() {
        // The analogue of the JS `ROOT`: relative operator paths resolve against it.
        let cwd = context
            .resolve_path(RuntimeArea::Resources, "", PathIntent::ReadDirectory)
            .unwrap_or_else(|_| context.resources_dir().to_path_buf());
        let root = match context.resolve_path(RuntimeArea::Resources, &vc.dir_rel, PathIntent::ReadDirectory) {
            Ok(root) => root,
            Err(_) if library_dir_missing(&cwd, &vc.dir_rel) => cwd.join(&vc.dir_rel),
            Err(_) => return Err((503, "selected voice directory is unavailable".to_string())),
        };
        Ok(Library { root: voice::normalize_lexically(&root), dir_rel: vc.dir_rel.clone(), isolated: true, cwd })
    } else {
        let configured = Path::new(&vc.dir_rel);
        let root = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            PathBuf::from(format!("C:\\qqbot\\qq-bridge\\{}", vc.dir_rel.replace('/', "\\")))
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
        Ok(Library { root: voice::normalize_lexically(&root), dir_rel: vc.dir_rel.clone(), isolated: false, cwd })
    }
}

impl Library {
    /// `(relative name, size)` of every audio file, in library order.
    fn files(&self, context: &RuntimeContext) -> Vec<(String, u64)> {
        if self.isolated {
            return list_audio_files_for_runtime(context, &self.dir_rel);
        }
        voice::list_audio_files(&self.root)
            .into_iter()
            .map(|path| {
                let name = path.strip_prefix(&self.root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                (name, size)
            })
            .collect()
    }

    /// A library file by its listed name; in a selected runtime the path is resolved again through
    /// the contained resolver (links and escapes fail closed).
    fn file(&self, context: &RuntimeContext, name: &str) -> Option<PathBuf> {
        if self.isolated {
            let full = if self.dir_rel.is_empty() { name.to_string() } else { format!("{}/{name}", self.dir_rel.trim_end_matches('/')) };
            return context.resolve_path(RuntimeArea::Resources, &full, PathIntent::ReadFile).ok();
        }
        Some(self.root.join(name))
    }

    /// Existing regular file for a library-relative reference (`safeJoinLibrary` plus the checks the
    /// AI resolver applies to the result).
    fn join_existing(&self, context: &RuntimeContext, relative: &str) -> Option<PathBuf> {
        let joined = voice::safe_join_library(&self.root, relative)?;
        if !joined.is_file() {
            return None;
        }
        if !self.isolated {
            return Some(joined);
        }
        let real_root = std::fs::canonicalize(&self.root).ok()?;
        let rel = joined.strip_prefix(&real_root).ok()?.to_string_lossy().replace('\\', "/");
        self.file(context, &rel)?;
        Some(joined)
    }

    /// `voicePathInsideLibrary`: an absolute path is accepted only when it lies inside the library.
    fn confine(&self, context: &RuntimeContext, absolute: &Path) -> Option<PathBuf> {
        let rel = voice::normalize_lexically(absolute).strip_prefix(&self.root).ok()?.to_string_lossy().into_owned();
        if rel.is_empty() || rel.starts_with("..") {
            return None;
        }
        self.join_existing(context, &rel)
    }

    fn dir_name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn display_dir(&self) -> String {
        crate::subprocess::external_path(&self.root).unwrap_or_else(|error| format!("<{error}>"))
    }
}

/// `resolveVoicePathForV2`: the AI may only name files inside the library.
fn resolve_for_ai(lib: &Library, context: &RuntimeContext, input: &str) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("缺少音频：给语音库里的文件名（可用 qq_list_voices 查看）".to_string());
    }
    let lower = raw.to_lowercase();
    if lower.starts_with("base64://") || lower.starts_with("data:") || lower.starts_with("http://") || lower.starts_with("https://") {
        return Err(NOT_FROM_LIBRARY.to_string());
    }
    // Node path.isAbsolute also accepts drive-less Windows roots (/foo, \foo).
    let is_abs = Path::new(raw).has_root() || lower.starts_with("file://") || raw.starts_with('~');
    let mut resolved: Option<PathBuf> = None;
    if is_abs {
        let found = voice::resolve_audio_source(raw, None, &lib.cwd);
        match found.path.and_then(|p| lib.confine(context, &p)) {
            Some(path) => resolved = Some(path),
            None => {
                // Only the directory name: this message goes back to a model facing untrusted chat.
                return Err(format!(
                    "语音只能从语音库取文件（语音库目录：{}）；即使是绝对路径也必须位于语音库之内（socialV2.voice.allowAbsolutePath 已不再放宽 AI 通道）。请把音频放进语音库，或用操作者通道（CLI / 控制台）发送库外文件。",
                    lib.dir_name()
                ));
            }
        }
    } else {
        resolved = lib.join_existing(context, raw);
        if resolved.is_none() && voice::extname(raw).is_empty() {
            resolved = voice::AUDIO_EXTS.iter().find_map(|ext| lib.join_existing(context, &format!("{raw}{ext}")));
        }
        if resolved.is_none() {
            let escapes = raw.replace('\\', "/").split('/').any(|part| part == "..") || Path::new(raw).has_root();
            if escapes {
                return Err("语音只能从语音库取文件：路径里出现了「..」或绝对路径，已拒绝（防止读到库外的本机文件）".to_string());
            }
        }
    }
    if let Some(path) = resolved {
        return Ok(path);
    }
    let names: Vec<String> = lib.files(context).into_iter().map(|(n, _)| n).collect();
    let hint = if names.is_empty() {
        format!("语音库（{}）目前是空的，先把音频文件放进去", lib.dir_name())
    } else {
        let more = if names.len() > 12 { format!(" 等 {} 个", names.len()) } else { String::new() };
        format!("语音库里现有：{}{more}", names.iter().take(12).cloned().collect::<Vec<_>>().join("、"))
    };
    let want = voice::base_name(raw).to_lowercase();
    let want_no_ext = want.rsplit_once('.').map(|(stem, _)| stem.to_string()).unwrap_or_else(|| want.clone());
    let fuzzy: Vec<&String> = names
        .iter()
        .filter(|name| {
            let base = voice::base_name(name).to_lowercase();
            let stem = base.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_else(|| base.clone());
            base == want || stem == want_no_ext
        })
        .collect();
    let fuzzy = if fuzzy.is_empty() { String::new() } else { format!("；近似匹配：{}", fuzzy.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("、")) };
    Err(format!("语音库里找不到「{raw}」{fuzzy}。{hint}"))
}

/// `resolveVoicePathForOperator`: operators may name any local file; library names resolve too.
fn resolve_for_operator(lib: &Library, input: &str) -> Result<(PathBuf, String), String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("缺少音频路径".to_string());
    }
    let lower = raw.to_lowercase();
    if lower.starts_with("base64://") || lower.starts_with("data:") || lower.starts_with("http://") || lower.starts_with("https://") {
        return Err("语音只接受本地文件；不接受 base64://、data: 或 http(s):// 来源".to_string());
    }
    let found = voice::resolve_audio_source(raw, Some(&lib.root), &lib.cwd);
    if let Some(path) = found.path {
        return Ok((path, found.via));
    }
    let fuzzy = if found.candidates.is_empty() {
        String::new()
    } else {
        format!(
            "；语音库近似匹配：{}",
            found.candidates.iter().map(|c| voice::base_name(&c.to_string_lossy()).to_string()).collect::<Vec<_>>().join("、")
        )
    };
    Err(format!("找不到音频「{raw}」{fuzzy}"))
}

// ----- list ---------------------------------------------------------------------------------------------------

/// `GET /api/voice/list` and `GET /api/socialV2/voices` (one handler in JS).
///
/// `voice.enabled` closes only the AI channel; the operator panel can always list the library.
/// The AI answer carries no filesystem path (neither in `dir` nor in the hint).
pub(super) fn handle_voice_list(
    context: &RuntimeContext,
    params: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    cfg: &Value,
) -> Result<Value, (u16, String)> {
    let key = params.get("key").map(|s| s.trim().to_string()).unwrap_or_default();
    let token = headers.get("x-agent-token").cloned().unwrap_or_default();
    let agent_call = !token.is_empty();
    let vc = voice_cfg(cfg);
    if agent_call && !vc.enabled {
        return Err((403, "语音功能已关闭".to_string()));
    }
    if agent_call && key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    if agent_call {
        if !agent_token_ok_for(context, &key, &token)? {
            return Err((403, "agent token 无效".to_string()));
        }
        if !social_v2_projection_session_allowed(cfg, &key) {
            return Err((403, "目标不在当前模式允许范围内".to_string()));
        }
        if !send_voice_tool_enabled(cfg) {
            return Err((403, "工具未启用：qq_send_voice".to_string()));
        }
    }
    let lib = open_library(context, &vc)?;
    let files = lib.files(context);
    let want_probe = params.get("probe").map(|p| p != "0").unwrap_or(true);
    let ffprobe = voice::ffprobe_path();
    let mut items: Vec<Value> = Vec::new();
    for (name, size) in files.iter().take(100) {
        let mut entry = json!({ "name": name });
        if want_probe {
            let info = match lib.file(context, name) {
                Some(path) => voice::inspect_audio(&path, vc.max_bytes as u64, vc.max_seconds, &ffprobe),
                None => voice::Inspect { error: Some("文件不存在或不可读".to_string()), ..voice::Inspect::default() },
            };
            entry["ok"] = json!(info.ok);
            entry["bytes"] = json!(info.size.unwrap_or(*size));
            entry["detail"] = if info.ok { json!(voice::describe_audio(&info)) } else { Value::Null };
            entry["seconds"] = info.duration.map(voice::json_number).unwrap_or(Value::Null);
            if let Some(error) = &info.error {
                entry["error"] = json!(error);
            }
        }
        items.push(entry);
    }
    let hint = if !items.is_empty() {
        "用 qq_send_voice 传 name 即可把其中一个当语音发出去".to_string()
    } else if agent_call {
        "语音库为空：请让管理员先把音频放进语音库".to_string()
    } else {
        format!("语音库为空：把音频文件放进 {} 后重试", lib.display_dir())
    };
    let mut out = json!({
        "ok": true,
        "key": if key.is_empty() { Value::Null } else { json!(key) },
        "count": items.len(),
        "voices": items,
        "limits": vc.limits(),
        "hint": hint,
    });
    if !agent_call {
        out["dir"] = json!(lib.display_dir());
    }
    Ok(out)
}

/// The library summary `GET /api/socialV2/prompt` gives the model.
pub(super) fn prompt_summary(context: &RuntimeContext, cfg: &Value) -> Value {
    let vc = voice_cfg(cfg);
    if !vc.enabled {
        return json!({ "enabled": false });
    }
    let names: Vec<String> = open_library(context, &vc)
        .map(|lib| lib.files(context).into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default();
    json!({
        "enabled": true,
        "total": names.len(),
        "names": names.iter().take(12).collect::<Vec<_>>(),
        "maxSeconds": voice::json_number(vc.max_seconds),
        "hint": "可用 qq_list_voices 查看语音库，再用 qq_send_voice 把其中一个当语音发出去",
    })
}

// ----- sending --------------------------------------------------------------------------------------------------

struct SendArgs<'a> {
    /// Canonical conversation key.
    key: &'a str,
    kind: &'a str,
    id_s: &'a str,
    file: &'a Path,
    audio: AudioOptions,
    /// AI channel: the agent token's epoch and the validity policy checked right before the request.
    guard: Option<(&'a str, GuardPolicy)>,
    epoch: String,
    budget_scope: String,
    /// Timestamp the budget slot is spent at after a confirmed send. JS: the AI channel stamps after
    /// the send (`voiceBudgetCommit(key)`), the operator channel with the time taken before it.
    budget_at: Option<i64>,
}

struct Sent {
    message_id: Option<i64>,
    info: voice::Inspect,
    detail: String,
    audio_processing: String,
    volume: f64,
    loudness: Option<f64>,
    audio_mode: &'static str,
}

enum Failure {
    Plain(String),
    Outcome { outcome: &'static str, message: String, batch_id: String },
}

impl Failure {
    fn reply(self) -> (u16, Value) {
        match self {
            Failure::Plain(message) => (500, json!({ "ok": false, "error": message })),
            Failure::Outcome { outcome, message, batch_id } => (
                500,
                json!({ "ok": false, "error": message, "outcome": outcome, "batchId": batch_id, "verified": false }),
            ),
        }
    }
}

fn known_tokens(context: &RuntimeContext) -> Vec<String> {
    load_social_state_for(context)
        .ok()
        .and_then(|state| {
            state.get("conversations").and_then(Value::as_object).map(|all| {
                all.values().filter_map(|c| c.get("agentToken").and_then(Value::as_str)).filter(|t| !t.is_empty()).map(str::to_string).collect()
            })
        })
        .unwrap_or_default()
}

/// `shouldAuditKey`: closed-agent skips the audit only for the owner's private chat.
fn should_audit_key(context: &RuntimeContext, cfg: &Value, key: &str) -> bool {
    if current_mode_for(context).map(|m| m == "closed-agent").unwrap_or(false) {
        let owner = cfg.get("ownerQQ").map(social_policy_js_string).unwrap_or_default();
        return key != format!("private:{owner}");
    }
    true
}

/// `sendVoiceV2` plus the outbox discipline: audit, inspect, local volume processing, then
/// intent -> attempt -> request -> outcome. Returns the delivered voice or the exact failure.
fn send_voice_core(
    context: &RuntimeContext,
    cfg: &Value,
    vc: &VoiceCfg,
    args: &SendArgs<'_>,
) -> Result<Sent, Failure> {
    let file_name = args.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if should_audit_key(context, cfg, args.key) {
        let subject = format!("[语音:{file_name}]");
        if crate::pipeline::configured_outbound_violation(cfg, &subject, &known_tokens(context)).is_some() {
            crate::safe_println!("[voice] ⚠️ 语音发送被安全策略拦截 ({})：文件名疑似含敏感信息", args.key);
            return Err(Failure::Plain("音频文件名疑似包含敏感信息（路径/凭据/会话令牌），已阻止发送；请重命名后再发".to_string()));
        }
    }
    let info = voice::inspect_audio(args.file, vc.max_bytes as u64, vc.max_seconds, &voice::ffprobe_path());
    if !info.ok {
        return Err(Failure::Plain(format!("音频不可发送：{}", info.error.clone().unwrap_or_default())));
    }
    let plan = voice::resolve_audio_processing(&args.audio).map_err(|e| Failure::Plain(format!("音量处理失败：{e}")))?;
    let processed = voice::process_audio_volume(args.file, &plan, vc.ffmpeg_path.as_deref())
        .map_err(|e| Failure::Plain(format!("音量处理失败：{e}")))?;
    let temp_path = processed.converted.then(|| processed.path.clone());
    let result = send_processed(context, cfg, args, &processed.path, &file_name);
    if let Some(path) = temp_path {
        let _ = std::fs::remove_file(path);
    }
    let message_id = result?;
    Ok(Sent {
        message_id,
        detail: voice::describe_audio(&info),
        info,
        audio_processing: processed.detail,
        volume: processed.volume,
        loudness: processed.loudness,
        audio_mode: processed.mode,
    })
}

/// `voiceBudgetCommit`: only a delivered (confirmed) send spends the budget.
fn commit_budget(args: &SendArgs<'_>) {
    voice::budget_commit(&args.budget_scope, args.budget_at.unwrap_or_else(now_ms));
}

/// The recorded part of the send: validity check, intent, attempt, request, outcome.
fn send_processed(
    context: &RuntimeContext,
    cfg: &Value,
    args: &SendArgs<'_>,
    send_path: &Path,
    file_name: &str,
) -> Result<Option<i64>, Failure> {
    use crate::onebot::Target;
    use crate::recorded_send::{BeginError, History, Proof, Recorded, BATCH_BUSY};
    use crate::send_outbox::ItemState;
    let external_send_path = voice::plain_path(send_path).map_err(Failure::Plain)?;
    // JS `sendChain`: a voice is one more item on the chain every text, burst and sticker send uses.
    let _chain = agent::send_chain();
    if let Some((epoch, policy)) = args.guard {
        send_guard_for(context, cfg, args.key, epoch, policy, "sendVoice")
            .map_err(|(_, message)| Failure::Plain(message))?;
    }
    let ob = onebot_for_runtime(context, cfg).map_err(Failure::Plain)?;
    let target = if args.kind == "private" { Target::Private(args.id_s.parse().unwrap_or(0)) } else { Target::Group(args.id_s.parse().unwrap_or(0)) };
    let label = format!("[语音:{file_name}]");
    let fields = json!({
        "kind": "voice", "key": args.key, "epoch": args.epoch,
        "messages": [label], "delays": [], "replyTo": Value::Null, "atUser": Value::Null
    });
    let recorded = Recorded::begin(context, now_ms, now_ms(), fields).map_err(|error| Failure::Plain(match error {
        BeginError::Busy => BATCH_BUSY.to_string(),
        BeginError::Intent(error) => format!("发送意图未能持久记录，未发送：{error}"),
    }))?;
    let batch_id = recorded.batch_id().to_string();
    let attempt_ms = match recorded.attempt(0, None) {
        Ok(at) => at,
        Err(error) => {
            let _ = recorded.cancelled(0, Some(&error));
            return Err(Failure::Plain(format!("发送意图未能持久记录，未发送：{error}")));
        }
    };
    // An unknown send spends nothing (JS commits only after a delivered send); a record segment
    // of ours in history after the attempt can still confirm it.
    let history = History { ob: &ob, kind: args.kind, id_s: args.id_s };
    let outcome = ob.send_record_outcome(&target, &external_send_path);
    let settled = recorded.settle(0, attempt_ms, outcome, &history, Proof::Segment("record"), &[]);
    if let Some(error) = &settled.record_error {
        crate::safe_println!("[voice] send outbox record failed: {error}");
    }
    let error = settled.error.unwrap_or_default();
    match settled.state {
        ItemState::Confirmed { message_id, .. } => {
            commit_budget(args);
            Ok(message_id)
        }
        ItemState::NotCommitted => Err(Failure::Outcome { outcome: "not_committed", message: error, batch_id }),
        _ => Err(Failure::Outcome {
            outcome: "unknown",
            message: format!(
                "语音发送结果未知（请求已发出但没有收到可信回执：{error}）；不会自动重发，请先在 QQ 里确认这条语音是否已出现，再决定是否重新发送"
            ),
            batch_id,
        }),
    }
}

/// `verifyVoiceMessageSent`: read the message back and check it carries a `record` segment.
/// The reply keeps only "confirmed" and the duration, never a file path.
fn verify_voice_message(cfg: &Value, context: &RuntimeContext, message_id: Option<i64>) -> Option<Option<f64>> {
    let message_id = message_id?;
    let ob = onebot_for_runtime(context, cfg).ok()?;
    let reply = ob.request_with_timeout("get_msg", json!({ "message_id": message_id }), std::time::Duration::from_secs(30))?;
    if reply.get("status").and_then(Value::as_str) != Some("ok") || reply.get("retcode").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let segments = reply.pointer("/data/message").and_then(Value::as_array)?;
    let record = segments.iter().find(|s| s.get("type").and_then(Value::as_str) == Some("record"))?;
    Some(record.pointer("/data/duration").filter(|d| !d.is_null()).map(|d| agent::js_number(Some(d))))
}

/// `fetch_ptt_text` readback; `None` when the gateway has no transcription.
fn fetch_transcription(cfg: &Value, context: &RuntimeContext, message_id: Option<i64>) -> Option<String> {
    let message_id = message_id?;
    let ob = onebot_for_runtime(context, cfg).ok()?;
    let reply = ob.request_with_timeout("fetch_ptt_text", json!({ "message_id": message_id.to_string() }), std::time::Duration::from_secs(30))?;
    if reply.get("status").and_then(Value::as_str) != Some("ok") || reply.get("retcode").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    Some(reply.pointer("/data/text").and_then(Value::as_str).unwrap_or("").to_string())
}

/// `recordVoiceSentV2`: let the model see what it just said in its own conversation record.
fn record_voice_sent(context: &RuntimeContext, key: &str, sent: &Sent, label: &str) -> Result<(), (u16, String)> {
    let display = if label.is_empty() { format!("[语音 {}]", sent.detail) } else { label.to_string() };
    let text = crate::social_mem::truncate_text(&display, 200);
    let at = now_ms();
    let message_id = sent.message_id.filter(|id| *id != 0).map(|id| json!(id.to_string())).unwrap_or(Value::Null);
    mutate_social_state_for(context, |state| {
        let conv = state
            .get_mut("conversations")
            .and_then(|c| c.get_mut(key))
            .and_then(Value::as_object_mut)
            .ok_or((404, format!("会话不存在：{key}")))?;
        let recent = conv.entry("recentMessages".to_string()).or_insert_with(|| json!([]));
        if let Some(list) = recent.as_array_mut() {
            list.push(json!({
                "messageId": message_id, "sender": "我", "text": text, "plain": text, "quoteTargetIsSelf": false,
                "isOwner": true, "ownerLabel": "我", "isSelf": true, "at": at
            }));
            if list.len() > 200 {
                let excess = list.len() - 200;
                list.drain(0..excess);
            }
        }
        conv.insert("lastAiReplyAt".to_string(), json!(at));
        conv.insert("lastActionAt".to_string(), json!(at));
        // A delivered voice message is her send: it (re)starts her engagement window and, like a
        // text, sticker or poke send, a new pre-sleep observation.
        for field in ["preSleepWaitSatisfiedAt", "preSleepWaitObservedAt", "preSleepWaitAccumMs"] {
            conv.insert(field.to_string(), json!(0));
        }
        conv.insert("engagedAt".to_string(), json!(at));
        if let Some(wake) = conv.get_mut("wakeConfig").and_then(Value::as_object_mut) {
            wake.insert("noActionCount".to_string(), json!(0));
        }
        Ok(())
    })
}

fn budget_scope(context: &RuntimeContext, key: &str) -> String {
    format!("{}|{key}", context.runtime_root().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
}

fn js_member(body: &Value, name: &str) -> String {
    body.get(name).filter(|v| !v.is_null()).map(social_policy_js_string).unwrap_or_default()
}

/// First of `a ?? b ?? ''` as a string.
fn first_member(body: &Value, names: &[&str]) -> String {
    names.iter().find_map(|n| body.get(*n).filter(|v| !v.is_null()).map(social_policy_js_string)).unwrap_or_default()
}

fn err(status: u16, message: impl Into<String>) -> (u16, Value) {
    (status, json!({ "ok": false, "error": message.into() }))
}

fn parse_body(body: &str) -> Value {
    serde_json::from_str::<Value>(body).ok().filter(Value::is_object).unwrap_or_else(|| json!({}))
}

fn preview_of(key: &str, path: &Path, via: &str, info: &voice::Inspect, vc: &VoiceCfg) -> Result<serde_json::Map<String, Value>, String> {
    let mut map = serde_json::Map::new();
    map.insert("key".into(), json!(key));
    map.insert("file".into(), json!(crate::subprocess::external_path(path)?));
    map.insert("via".into(), json!(via));
    map.insert("bytes".into(), info.size.map(Value::from).unwrap_or(Value::Null));
    map.insert("duration".into(), info.duration.map(voice::json_number).unwrap_or(Value::Null));
    map.insert("codec".into(), json!(info.codec));
    map.insert("detail".into(), json!(voice::describe_audio(info)));
    map.insert("warnings".into(), json!(info.warnings));
    map.insert("limits".into(), vc.limits());
    Ok(map)
}

/// `POST /api/voice/send` and `POST /api/send/voice` (operator channel, console token).
pub(super) fn operator_send(
    context: &RuntimeContext,
    cfg: &Value,
    path: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> (u16, Value) {
    let b = parse_body(body);
    let vc = voice_cfg(cfg);
    let agent_token = headers.get("x-agent-token").map(|t| t.trim().to_string()).unwrap_or_default();
    // /api/send/voice is the AI tool entrance when a token is present: never a way around "own session only".
    if path == "/api/send/voice" && !agent_token.is_empty() {
        return err(403, "AI 请改用 /api/socialV2/send-voice（带 key + token）");
    }
    let key = first_member(&b, &["target", "key"]).trim().to_string();
    let Some((kind, id_s)) = voice::parse_target_key(&key) else {
        return err(400, "target 格式应为 group:群号 或 private:QQ号");
    };
    let mode = match current_mode_for(context) {
        Ok(mode) => mode,
        Err((status, message)) => return err(status, message),
    };
    if !agent::mode_allowed(cfg, &mode, &key) {
        let list = |name: &str| {
            let joined = cfg
                .pointer(&format!("/allow/{name}"))
                .and_then(Value::as_array)
                .map(|a| a.iter().map(social_policy_js_string).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            if joined.is_empty() { "空".to_string() } else { joined }
        };
        return err(403, format!("目标 {key} 不在白名单内（config.json allow.groups / allow.private：群 {}；私聊 {}）", list("groups"), list("private")));
    }
    let lib = match open_library(context, &vc) {
        Ok(lib) => lib,
        Err((status, message)) => return err(status, message),
    };
    let (file, via) = match resolve_for_operator(&lib, &first_member(&b, &["file", "path"])) {
        Ok(found) => found,
        Err(message) => return err(400, message),
    };
    let audio = audio_options(&vc, b.get("volume"), b.get("normalize") == Some(&json!(true)), b.get("loudness"));
    if let Err(message) = voice::resolve_audio_processing(&audio) {
        return err(400, message);
    }
    let probe = voice::inspect_audio(&file, vc.max_bytes as u64, vc.max_seconds, &voice::ffprobe_path());
    if !probe.ok {
        return err(400, probe.error.clone().unwrap_or_default());
    }
    let preview = match preview_of(&key, &file, &via, &probe, &vc) {
        Ok(preview) => preview,
        Err(error) => return err(400, error),
    };
    if b.get("dryRun") == Some(&json!(true)) {
        let mut out = json!({ "ok": true, "dryRun": true });
        out.as_object_mut().unwrap().extend(preview);
        return (200, out);
    }
    let now = now_ms();
    let scope = budget_scope(context, OPERATOR_SCOPE);
    if let Err(message) = voice::budget_check(&scope, now, vc.max_per_minute, vc.max_per_hour) {
        return err(500, message);
    }
    let canonical_key = canonical_v2_key(&key).unwrap_or_else(|| key.clone());
    let args = SendArgs {
        key: &canonical_key, kind: &kind, id_s: &id_s, file: &file, audio, guard: None, epoch: String::new(),
        budget_scope: scope, budget_at: Some(now),
    };
    let sent = match send_voice_core(context, cfg, &vc, &args) {
        Ok(sent) => sent,
        Err(failure) => return failure.reply(),
    };
    crate::safe_println!(
        "[voice] (操作者) {key} 已发送语音 {} ({}{}, messageId={})",
        file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        sent.detail,
        if sent.audio_mode == "original" { String::new() } else { format!("，{}", sent.audio_processing) },
        sent.message_id.map(|id| id.to_string()).unwrap_or_else(|| "?".into())
    );
    let verified = b.get("verify") != Some(&json!(false)) && verify_voice_message(cfg, context, sent.message_id).is_some();
    let readback = if b.get("readback") == Some(&json!(true)) { fetch_transcription(cfg, context, sent.message_id) } else { None };
    let mut out = json!({ "ok": true });
    out.as_object_mut().unwrap().extend(preview);
    out["messageId"] = sent.message_id.map(Value::from).unwrap_or(Value::Null);
    out["verified"] = json!(verified);
    out["readback"] = readback.map(Value::from).unwrap_or(Value::Null);
    out["audioProcessing"] = json!(sent.audio_processing);
    out["volume"] = voice::json_number(sent.volume);
    out["loudness"] = sent.loudness.map(voice::json_number).unwrap_or(Value::Null);
    (200, out)
}

/// `POST /api/socialV2/send-voice` (AI channel: reserved2, agent token, its own conversation only).
pub(super) fn ai_send(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> (u16, Value) {
    let b = parse_body(body);
    let token = {
        let from_body = b.get("token").filter(|v| !v.is_null()).map(social_policy_js_string);
        from_body.or_else(|| headers.get("x-agent-token").cloned()).unwrap_or_default().trim().to_string()
    };
    let key = first_member(&b, &["key", "target"]).trim().to_string();
    let vc = voice_cfg(cfg);
    if !vc.enabled {
        return err(403, "语音功能已关闭");
    }
    if key.is_empty() {
        return err(400, "key 不能为空（格式 group:群号 或 private:QQ号）");
    }
    match current_mode_for(context) {
        Ok(mode) if mode == "reserved2" => {}
        Ok(_) => return err(403, "该接口仅 reserved2 模式可用"),
        Err((status, message)) => return err(status, message),
    }
    if token.is_empty() {
        return err(403, "reserved2 模式发送必须携带 agent token");
    }
    match agent_token_ok_for(context, &key, &token) {
        Ok(true) => {}
        Ok(false) => return err(403, "agent token 无效"),
        Err((status, message)) => return err(status, message),
    }
    if !send_voice_tool_enabled(cfg) {
        return err(403, "工具未启用：qq_send_voice");
    }
    if !social_v2_projection_session_allowed(cfg, &key) {
        return err(403, "目标不在当前模式允许范围内");
    }
    match silent_blocks_send(context, cfg, &key) {
        Ok(false) => {}
        Ok(true) => return err(403, "静默模式已开启，当前不允许发送"),
        Err((status, message)) => return err(status, message),
    }
    let Some((kind, id_s, id)) = agent::parse_key(&key) else {
        return err(400, "key 格式应为 group:群号 或 private:QQ号");
    };
    if id <= 0 || !agent::mode_allowed(cfg, "reserved2", &key) {
        return err(403, "目标不在当前模式允许范围内");
    }
    let lib = match open_library(context, &vc) {
        Ok(lib) => lib,
        Err((status, message)) => return err(status, message),
    };
    let reference = first_member(&b, &["voice", "file"]);
    let file = match resolve_for_ai(&lib, context, &reference) {
        Ok(path) => path,
        Err(message) => return err(400, message),
    };
    let scope = budget_scope(context, &key);
    let now = now_ms();
    if let Err(message) = voice::budget_check(&scope, now, vc.max_per_minute, vc.max_per_hour) {
        return err(500, message);
    }
    let ffprobe = voice::ffprobe_path();
    if b.get("dryRun") == Some(&json!(true)) {
        let probe = voice::inspect_audio(&file, vc.max_bytes as u64, vc.max_seconds, &ffprobe);
        if !probe.ok {
            return err(400, probe.error.clone().unwrap_or_default());
        }
        let external_file = match crate::subprocess::external_path(&file) {
            Ok(file) => file,
            Err(error) => return err(400, error),
        };
        return (200, json!({ "ok": true, "dryRun": true, "key": key, "file": external_file, "detail": voice::describe_audio(&probe) }));
    }
    let has_audio = b.get("volume").is_some() || b.get("normalize") == Some(&json!(true)) || b.get("loudness").is_some();
    let audio = audio_options(&vc, b.get("volume"), b.get("normalize") == Some(&json!(true)), b.get("loudness"));
    if has_audio {
        // Checked first so a wrong value is a readable error rather than an ffmpeg failure.
        if let Err(message) = voice::resolve_audio_processing(&audio) {
            return err(500, message);
        }
    }
    let storage_key = match canonical_v2_key(&key) {
        Some(k) => k,
        None => return err(400, format!("无效的会话 key：{key}")),
    };
    let epoch = crate::send_outbox::token_epoch(&token);
    let args = SendArgs {
        key: &storage_key, kind, id_s, file: &file, audio, guard: Some((&epoch, GuardPolicy::Reserved2)), epoch: epoch.clone(),
        budget_scope: scope, budget_at: None,
    };
    let sent = match send_voice_core(context, cfg, &vc, &args) {
        Ok(sent) => sent,
        Err(failure) => return failure.reply(),
    };
    let label: String = js_member(&b, "label").chars().take(100).collect();
    let file_name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::safe_println!(
        "[voice] {key} 已发送语音 {file_name} ({}, messageId={})",
        sent.detail,
        sent.message_id.map(|id| id.to_string()).unwrap_or_else(|| "?".into())
    );
    if let Err((_, message)) = record_voice_sent(context, &storage_key, &sent, &label) {
        return err(500, format!("语音已发送（messageId={}），但会话记录更新失败：{message}", sent.message_id.map(|id| id.to_string()).unwrap_or_else(|| "?".into())));
    }
    let readback = if b.get("readback") == Some(&json!(true)) { fetch_transcription(cfg, context, sent.message_id) } else { None };
    let want_verify = b.get("verify") == Some(&json!(true)) || (b.get("verify") != Some(&json!(false)) && vc.auto_readback);
    let verified = want_verify && verify_voice_message(cfg, context, sent.message_id).is_some();
    (
        200,
        json!({
            "ok": true,
            "key": key,
            "messageId": sent.message_id.map(Value::from).unwrap_or(Value::Null),
            "file": file_name,
            "detail": sent.detail,
            "audioProcessing": sent.audio_processing,
            "verified": verified,
            "readback": readback.map(Value::from).unwrap_or(Value::Null),
            "warnings": sent.info.warnings,
        }),
    )
}
