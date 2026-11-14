//! Chat mode (ticket 12): ordinary conversation in allowed groups and private chats, the public
//! slash commands, the automatic reply and its recorded sending.
//!
//! JS reference: `bridge.js` `handleIncoming` (chat branch), `segmentsToText`, `sendToQQ`,
//! `auditAndSend`, and `md-to-plain.js`. The decision functions here are pure; the hub at the end
//! owns the side effects (journal, role state, resets, per-conversation turn workers).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::inbound::SharedJournal;
use crate::onebot::{OneBot, Target};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

pub const DEFAULT_ACK: &str = "🤔 收到，正在思考…";
/// QQ single-message limit used by `splitForQQ`.
pub const QQ_PART_LIMIT: usize = 4000;
const NAME_TTL: Duration = Duration::from_secs(10 * 60);
const REPLY_TTL: Duration = Duration::from_secs(10 * 60);
const REPLY_NEGATIVE_TTL: Duration = Duration::from_secs(5);

fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
}

// ───────────────────────── configuration helpers ─────────────────────────

/// JS `String(v).trim()` of a scalar; null/missing is the empty string.
fn scalar(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// `normalizeIdList`: non-arrays are empty, entries are trimmed and kept only when all digits.
fn id_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|v| scalar(Some(v)))
                .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .collect()
        })
        .unwrap_or_default()
}

/// The two alias spellings of the allow/deny lists, in JS precedence (`private ?? privates`,
/// `groups ?? group`).
fn policy_list(cfg: &Value, root: &str, kind: &str) -> Vec<String> {
    let table = cfg.get(root);
    let pick = |first: &str, second: &str| -> Vec<String> {
        let v = table.and_then(|t| t.get(first)).filter(|v| !v.is_null()).or_else(|| table.and_then(|t| t.get(second)));
        id_list(v)
    };
    if kind == "group" {
        pick("groups", "group")
    } else {
        pick("private", "privates")
    }
}

/// JS `allowed(kind, id, cfg)`: deny list first, then the allow list, then `allowAllWhenEmpty`.
pub fn allowed(cfg: &Value, kind: &str, id: &str) -> bool {
    let id = id.trim();
    if policy_list(cfg, "deny", kind).iter().any(|v| v == id) {
        return false;
    }
    let allow = policy_list(cfg, "allow", kind);
    if !allow.is_empty() {
        return allow.iter().any(|v| v == id);
    }
    cfg.get("allowAllWhenEmpty").and_then(Value::as_bool) == Some(true)
}

/// `allowed` for a `group:<id>` / `private:<id>` key.
pub fn allowed_key(cfg: &Value, key: &str) -> bool {
    match key.split_once(':') {
        Some((kind @ ("group" | "private"), id)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => allowed(cfg, kind, id),
        _ => false,
    }
}

pub fn owner_qq(cfg: &Value) -> String {
    scalar(cfg.get("ownerQQ"))
}

pub fn is_owner_key(cfg: &Value, key: &str) -> bool {
    let owner = owner_qq(cfg);
    !owner.is_empty() && key == format!("private:{owner}")
}

/// Closed-agent admits only the owner's private conversation; other modes use the allow list.
pub fn allowed_in_mode(cfg: &Value, mode: &str, key: &str) -> bool {
    if mode == "closed-agent" { is_owner_key(cfg, key) } else { allowed_key(cfg, key) }
}

/// `cfg.ackMessage ?? default`; an empty string switches the acknowledgement off.
pub fn ack_message(cfg: &Value) -> Option<String> {
    let text = match cfg.get("ackMessage") {
        Some(Value::Null) | None => DEFAULT_ACK.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    (!text.is_empty()).then_some(text)
}

/// `cfg.sendDelayMs ?? 300`.
pub fn send_delay_ms(cfg: &Value) -> u64 {
    cfg.get("sendDelayMs").and_then(Value::as_u64).unwrap_or(300)
}

/// The agent preset of chat sessions: `cfg.agentPreset`, default `qq-chat` (never the reserved2 one).
pub fn chat_preset(cfg: &Value) -> String {
    cfg.get("agentPreset").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("qq-chat").to_string()
}


/// The whole-message forms `/update` and `/update deps` are intercepted before mode dispatch,
/// including group/stranger refusals; other text starting with `/update` is ordinary slash text.
pub fn is_update_command(plain: &str) -> bool {
    let mut words = plain.split_whitespace();
    words.next().is_some_and(|command| command.eq_ignore_ascii_case("/update"))
        && match words.next() {
            None => true,
            Some(argument) => argument.eq_ignore_ascii_case("deps") && words.next().is_none(),
        }
}

pub fn handle_update_command(
    context: &RuntimeContext,
    key: &str,
    sender: &str,
    plain: &str,
) -> Result<(), String> {
    let cfg = context.load_config().map_err(str::to_string)?;
    let administrator = is_owner_key(&cfg, key) && owner_qq(&cfg) == sender;
    let ob = onebot_of(context, &cfg)?;
    let say = |text: &str| -> Result<(), String> {
        let guard = || {
            let latest = context.load_config().map_err(str::to_string)?;
            if administrator {
                if !is_owner_key(&latest, key) || owner_qq(&latest) != sender {
                    return Err("administrator changed".into());
                }
            } else if !allowed_key(&latest, key) {
                return Err("conversation is not allowed".into());
            }
            Ok(())
        };
        let report = send_recorded(context, &ob, key, "chat", "update-command", text, 0, &guard);
        if report.error.is_some() || report.parts == 0 || report.confirmed != report.parts {
            return Err("update reply was not confirmed; no update approved".into());
        }
        Ok(())
    };
    if !administrator {
        return say("更新命令仅限管理员私聊。");
    }
    let args: Vec<_> = plain.split_whitespace().map(str::to_ascii_lowercase).collect();
    if args == ["/update", "deps"] {
        let state = match crate::supervisor_dependencies::upgrades::status(context, &cfg) {
            Ok(state) => state,
            Err(_) => return say("依赖更新状态不可用，请在控制台检查。"),
        };
        let mut lines = vec!["依赖锁定版本：".to_string()];
        let mut proposed = None;
        for name in ["snowluma", "dsh", "relay"] {
            let dependency = &state["dependencies"][name];
            let pin = dependency["current"].as_str().unwrap_or("未配置");
            if let Some(version) = dependency["available"].as_str() {
                lines.push(format!("{name}: {pin} → {version}"));
                if proposed.is_none() { proposed = Some((name, version.to_string())); }
            } else if let Some(reason) = dependency["sourceUnavailable"].as_str() {
                lines.push(format!("{name}: {pin}；{reason}"));
            } else if dependency["error"].is_string() {
                if let Some(latest) = dependency["latest"].as_str() {
                    lines.push(format!("{name}: {pin}；最新版本 {latest}，但安装信息不可用，请在控制台检查。"));
                } else {
                    lines.push(format!("{name}: {pin}；版本发现失败，请在控制台检查。"));
                }
            } else if let Some(latest) = dependency["latest"].as_str() {
                lines.push(format!("{name}: {pin}；最新版本 {latest}；暂无可批准更新。"));
            } else {
                lines.push(format!("{name}: {pin}；暂无可批准更新（或更新已关闭）。"));
            }
        }
        if let Some((name, version)) = &proposed {
            lines.push(format!("准备升级依赖 {name}：{} → {version}。其他依赖需分别批准。", state["dependencies"][name]["current"].as_str().unwrap_or("未知版本")));
        }
        // The command itself is explicit approval, as for /update. Never persist it until
        // this exact report has a confirmed QQ receipt and the owner is rechecked.
        say(&lines.join("\n"))?;
        let Some((name, version)) = proposed else { return Ok(()); };
        let _gate = crate::console::acquire_social_state_gate(context).map_err(|(_, error)| error)?;
        let latest = context.load_config().map_err(str::to_string)?;
        if !is_owner_key(&latest, key) || owner_qq(&latest) != sender { return Err("administrator changed".into()); }
        return crate::supervisor_dependencies::upgrades::approve(context, &latest, name, &version).map(|_| ());
    }
    if args != ["/update"] {
        return say("用法：/update 或 /update deps。");
    }
    let state = match crate::release::status(context, &cfg) {
        Ok(state) => state,
        Err(_) => return say("更新状态不可用，请在控制台检查。"),
    };
    let Some(tag) = state["available"]["tag"].as_str() else {
        return say(&no_update_reply(&state));
    };
    // A refused, timed-out or uncertain QQ reply cannot admit an update.
    say(&format!(
        "准备更新 HER：{} → {tag}。",
        state["current"].as_str().unwrap_or("未知版本")
    ))?;
    let _gate = crate::console::acquire_social_state_gate(context).map_err(|(_, error)| error)?;
    let latest = context.load_config().map_err(str::to_string)?;
    if !is_owner_key(&latest, key) || owner_qq(&latest) != sender {
        return Err("administrator changed".into());
    }
    crate::release::approve(context, &latest, tag).map(|_| ())
}

/// The /update reply when nothing can be approved: a discovery refusal or error is
/// reported, never hidden behind "no update".
fn no_update_reply(state: &Value) -> String {
    let current = state["current"].as_str().unwrap_or("未知版本");
    match state["error"].as_str() {
        Some(error) => format!("HER 当前版本：{current}；更新检查未通过：{error}。请在控制台检查。"),
        None => format!("HER 当前版本：{current}；暂无可用更新（或更新已关闭）。"),
    }
}

// ───────────────────────── mdToPlain ─────────────────────────

/// `md-to-plain.js` `mdToPlain`: the agent's Markdown flattened to text suited for QQ.
pub fn md_to_plain(md: &str) -> String {
    struct Res {
        fence: Regex,
        inline: Regex,
        image: Regex,
        link: Regex,
        bold3: Regex,
        bold2: Regex,
        italic: Regex,
        strike: Regex,
        under: Regex,
        heading: Regex,
        quote: Regex,
        bullet: Regex,
        numbered: Regex,
        table_lead: Regex,
        table_trail: Regex,
        blank_lines: Regex,
        sep_line: Regex,
    }
    static RES: OnceLock<Res> = OnceLock::new();
    let r = RES.get_or_init(|| Res {
        fence: Regex::new(r"(?s)```[a-zA-Z0-9_+-]*\n?(.*?)```").unwrap(),
        inline: Regex::new(r"`([^`\n]+)`").unwrap(),
        image: Regex::new(r"!\[([^\]\n]{0,500})\]\(([^)\s]{1,2000})\)").unwrap(),
        link: Regex::new(r"\[([^\]\n]{1,500})\]\(([^)\s]{1,2000})\)").unwrap(),
        bold3: Regex::new(r"\*\*\*([^*]+)\*\*\*").unwrap(),
        bold2: Regex::new(r"\*\*([^*]+)\*\*").unwrap(),
        // JS: \*(?!\s)([^*\n]*?[^\s*])\*  (no lookahead in this engine: first char is spelled out)
        italic: Regex::new(r"\*([^\s*](?:[^*\n]*?[^\s*])?)\*").unwrap(),
        strike: Regex::new(r"~~([^~]+)~~").unwrap(),
        under: Regex::new(r"__([^_]+)__").unwrap(),
        heading: Regex::new(r"(?m)^#{1,6}\s+").unwrap(),
        quote: Regex::new(r"(?m)^>\s?").unwrap(),
        bullet: Regex::new(r"(?m)^\s*[-*+]\s+").unwrap(),
        numbered: Regex::new(r"(?m)^\s*\d+\.\s+").unwrap(),
        table_lead: Regex::new(r"(?m)^\s*\|").unwrap(),
        table_trail: Regex::new(r"(?m)\|\s*$").unwrap(),
        blank_lines: Regex::new(r"\n{3,}").unwrap(),
        sep_line: Regex::new(r"^\s*\|?[\s:|-]+\|?\s*$").unwrap(),
    });
    let mut s = md.to_string();
    s = r.fence.replace_all(&s, |c: &regex::Captures| c[1].trim_end_matches('\n').to_string()).into_owned();
    s = r.inline.replace_all(&s, "$1").into_owned();
    s = r
        .image
        .replace_all(&s, |c: &regex::Captures| if c[1].is_empty() { c[2].to_string() } else { c[1].to_string() })
        .into_owned();
    s = r.link.replace_all(&s, "$1 ($2)").into_owned();
    s = r.bold3.replace_all(&s, "$1").into_owned();
    s = r.bold2.replace_all(&s, "$1").into_owned();
    s = r.italic.replace_all(&s, "$1").into_owned();
    s = r.strike.replace_all(&s, "$1").into_owned();
    s = r.under.replace_all(&s, "$1").into_owned();
    s = r.heading.replace_all(&s, "").into_owned();
    s = r.quote.replace_all(&s, "").into_owned();
    s = r.bullet.replace_all(&s, "• ").into_owned();
    s = r.numbered.replace_all(&s, |c: &regex::Captures| format!("{} ", c[0].trim())).into_owned();
    // Table separator rows (only `|`, `-`, `:` and spaces around a pipe) are dropped.
    s = s
        .split('\n')
        .filter(|line| {
            !r.sep_line.is_match(line)
                || !line.contains('|')
                || line.chars().filter(|c| !c.is_whitespace() && !matches!(c, ':' | '|' | '-')).count() > 0
        })
        .collect::<Vec<_>>()
        .join("\n");
    s = r.table_lead.replace_all(&s, "").into_owned();
    s = r.table_trail.replace_all(&s, "").into_owned();
    s = r.blank_lines.replace_all(&s, "\n\n").into_owned();
    crate::role::js_trim(&s).to_string()
}

// ───────────────────────── message segments → text ─────────────────────────

/// What `segmentsToText` needs from the gateway.
pub trait Resolver {
    /// Display name of the member `qq` in the current group.
    fn at_name(&mut self, qq: &str) -> Option<String>;
    /// Sender and text of a quoted message.
    fn reply(&mut self, message_id: &str) -> Option<ReplyInfo>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplyInfo {
    pub sender: String,
    pub text: String,
    pub user_id: Option<String>,
}

/// `segmentsToText`. `include_reply = false` drops quote segments (the message's own words).
pub fn segments_to_text(segments: &Value, res: &mut dyn Resolver, include_reply: bool) -> String {
    if let Value::String(s) = segments {
        return crate::role::js_trim(s).to_string();
    }
    let mut out = String::new();
    for seg in segments.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let data = seg.get("data").cloned().unwrap_or(Value::Null);
        let field = |name: &str| data.get(name).filter(|v| !v.is_null());
        // JS `switch (seg?.type)` is strict, so a non-string type reaches `default`, which prints `[${type}]`.
        let kind = match seg.get("type") {
            Some(Value::String(kind)) => Some(kind.as_str()),
            _ => None,
        };
        let other_type = seg.get("type").filter(|v| !v.is_null());
        match kind {
            // `out.push(d.text ?? '')` and `join('')` stringify a non-string value like `String(value)`.
            Some("text") => out.push_str(&field("text").map(crate::aux_text::js_string).unwrap_or_default()),
            Some("at") => {
                if field("qq").and_then(Value::as_str) == Some("all") {
                    out.push_str("@全体成员");
                } else {
                    let qq = scalar_raw(field("qq"));
                    match res.at_name(&qq) {
                        Some(name) if !name.is_empty() => out.push_str(&format!("@{name}")),
                        _ => out.push_str(&format!("@{qq}")),
                    }
                }
            }
            Some("face") => out.push_str(&format!("[表情{}]", scalar_raw(field("id")))),
            Some("mface") => {
                let summary = scalar_raw(field("summary"));
                out.push_str(&if summary.is_empty() { "[表情]".to_string() } else { format!("[表情:{summary}]") });
            }
            Some("image") => out.push_str("[图片]"),
            Some("record") => out.push_str("[语音]"),
            Some("video") => out.push_str("[视频]"),
            Some("file") => out.push_str(&format!("[文件{}]", scalar_raw(field("name")))),
            Some("reply") => {
                if !include_reply {
                    continue;
                }
                let mut text = String::new();
                if let Some(info) = res.reply(&scalar_raw(field("id"))) {
                    if !info.sender.is_empty() || !info.text.is_empty() {
                        let parts: Vec<&str> = [info.sender.as_str(), info.text.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
                        text = format!("[引用 {}]", parts.join("："));
                    }
                }
                out.push_str(&if text.is_empty() { "[引用消息]".to_string() } else { text });
            }
            Some("json") => out.push_str("[卡片消息]"),
            Some("markdown") => {
                let value = field("text").or_else(|| field("content"));
                out.push_str(&value.map(|v| scalar_raw(Some(v))).unwrap_or_else(|| "[Markdown]".to_string()));
            }
            Some("forward") => {
                let id = crate::forward_fmt::forward_id_from_data(&data);
                out.push_str(&if id.is_empty() { "[转发消息]".to_string() } else { format!("[转发消息 id={id}]") });
            }
            Some(other) => out.push_str(&format!("[{other}]")),
            None => match other_type {
                Some(value) => out.push_str(&format!("[{}]", crate::aux_text::js_string(value))),
                None => out.push_str("[未知]"),
            },
        }
    }
    crate::role::js_trim(&out).to_string()
}

/// JS template-literal stringification of a scalar (no trimming).
pub(crate) fn scalar_raw(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Number of image/sticker/face segments (`extractMediaFromSegments(...).length`).
pub fn media_count(segments: &Value) -> usize {
    segments
        .as_array()
        .map(|segs| segs.iter().filter(|s| matches!(s.get("type").and_then(Value::as_str), Some("image" | "mface" | "face"))).count())
        .unwrap_or(0)
}

/// `mediaHintFor` outside reserved2.
pub fn media_hint(message_ref: &str, count: usize) -> String {
    if count == 0 || message_ref.is_empty() {
        return String::new();
    }
    format!("\n【图片/表情】本条消息包含 {count} 个图片/表情（消息ID={message_ref}）。")
}

// ───────────────────────── decisions ─────────────────────────

pub struct Facts<'a> {
    /// The message's own words (no quote).
    pub plain: &'a str,
    pub is_owner: bool,
    pub quote_self: bool,
    pub role: Option<String>,
    /// `active` or `silent` (the role-mode file, not the bridge mode).
    pub role_mode: String,
    pub session: Option<String>,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Ignore(&'static str),
    Say(String),
    SetRole { name: Option<String>, say: String },
    SetRoleMode { mode: &'static str, say: &'static str },
    Reset,
    /// Hand the message to the model.
    Forward,
}

/// Everything `handleIncoming` decides before a message reaches the model.
pub fn decide(f: &Facts, role_exists: &dyn Fn(&str) -> bool) -> Decision {
    if f.plain.is_empty() && !f.quote_self {
        return Decision::Ignore("empty");
    }
    if f.role_mode == "silent" && !f.is_owner {
        return Decision::Ignore("silent-mode");
    }
    // A member's slash text is never input: DSH executes leading-slash prompts as its own
    // commands, so it is dropped here (no refusal, nothing sent to DSH or a model).
    if !f.is_owner && f.plain.trim_start().starts_with('/') {
        return Decision::Ignore("member-slash");
    }
    // Commands are the owner's whole-message slash forms and nothing else: command words inside
    // a message are ordinary input for the model.
    if f.is_owner && f.plain.starts_with('/') {
        match f.plain.to_ascii_lowercase().as_str() {
            "/reset" | "/new" => return Decision::Reset,
            "/status" => {
                return Decision::Say(format!(
                    "会话 {}；白名单 {}；角色 {}；模式 {}",
                    f.session.as_deref().unwrap_or("未创建"),
                    if f.allowed { "通过" } else { "拦截" },
                    f.role.as_deref().unwrap_or("无"),
                    f.role_mode
                ))
            }
            "/silent" | "/quiet" => {
                return Decision::SetRoleMode { mode: "silent", say: "已进入静默模式：群友消息不再回复，仅管理员可对话。" }
            }
            "/active" | "/speak" => return Decision::SetRoleMode { mode: "active", say: "已退出静默模式，恢复正常回复。" },
            other if other == "/role" || other.starts_with("/role ") => {
                let name = crate::console::sanitize_role_name(crate::role::js_trim(&f.plain.chars().skip(5).collect::<String>()));
                if name.is_empty() || name == "off" || name == "clear" {
                    return Decision::SetRole { name: None, say: "已清除角色，恢复正常人格。".to_string() };
                }
                if !role_exists(&name) {
                    return Decision::Say(format!("角色「{name}」不存在。角色文件放 qq-bridge/roles/ 目录。"));
                }
                let say = format!("已切换角色：{name}。");
                return Decision::SetRole { name: Some(name), say };
            }
            _ => {} // other slash text goes to DSH unchanged
        }
    }
    Decision::Forward
}

/// The prompt text handed to the model (before the clock line is added at delivery).
pub fn build_prompt(role_hint: &str, is_owner: bool, group_sender: Option<&str>, text: &str, media_hint: &str) -> String {
    let mut out = String::new();
    if !role_hint.is_empty() {
        out.push_str(role_hint);
        out.push_str("\n\n");
    }
    if is_owner {
        out.push_str("【管理员】");
    }
    match group_sender {
        Some(sender) => out.push_str(&format!("{sender}：{text}")),
        None => out.push_str(text),
    }
    out.push_str(media_hint);
    out
}

/// What to do with a finished turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Nothing is sent (reason for the local log).
    Silent(&'static str),
    /// The turn was stopped on the DSH side.
    Stopped,
    Send(String),
    /// The reply is held back by the outbound sentinel; the reason stays local.
    Blocked(String),
}

/// The ordered checks of the JS turn-end branch: empty text, silent marker, tool already sent,
/// sentinel audit.
pub fn reply_plan(end_kind: &str, text: &str, send_tool_ok: bool, secrets: &[String]) -> Reply {
    reply_plan_with_audit(end_kind, text, send_tool_ok, secrets, true)
}

pub fn reply_plan_with_audit(end_kind: &str, text: &str, send_tool_ok: bool, secrets: &[String], audit: bool) -> Reply {
    match end_kind {
        "completed" if !text.trim().is_empty() => {
            let plain = md_to_plain(text);
            if plain.trim().is_empty() {
                return Reply::Silent("empty-after-markdown");
            }
            if crate::pipeline::is_silent_marker(&plain) {
                return Reply::Silent("silent-marker");
            }
            if send_tool_ok {
                return Reply::Silent("tool-already-sent");
            }
            if audit {
                if let Some(why) = crate::pipeline::outbound_violation(&plain, secrets)
                    .or_else(|| crate::pipeline::outbound_violation(&crate::outbound_message::normalize(&plain), secrets)) {
                    return Reply::Blocked(why);
                }
            }
            Reply::Send(plain)
        }
        "aborted" => Reply::Stopped,
        _ => Reply::Silent("no-text"),
    }
}

/// `renameSessionForUi` title.
pub fn session_title(key: &str, group_name: &str) -> String {
    match key.split_once(':') {
        Some(("group", id)) if !group_name.is_empty() => {
            let _ = id;
            format!("QQ 群 · {group_name}")
        }
        Some(("group", id)) => format!("QQ 群 · {id}"),
        Some((_, id)) => format!("QQ 私聊 · {id}"),
        None => key.to_string(),
    }
}

// ───────────────────────── recorded sending ─────────────────────────

#[derive(Debug, Default)]
pub struct SendReport {
    pub parts: usize,
    pub confirmed: usize,
    pub error: Option<String>,
}

fn target_of(key: &str) -> Option<Target> {
    match key.split_once(':')? {
        ("group", id) => id.parse().ok().map(Target::Group),
        ("private", id) => id.parse().ok().map(Target::Private),
        _ => None,
    }
}

/// `sendToQQ`: split to the QQ limit, send part by part with the configured delay, check the
/// guard right before every part, and record every part through the recorded send. Chat sends
/// wait for the process-wide send chain like tool sends, but off the runtime worker
/// (`recorded_send::off_worker`, issue #82); a part whose outcome is unknown is checked against
/// gateway history and is never sent again.
pub fn send_recorded(
    context: &RuntimeContext,
    ob: &OneBot,
    key: &str,
    epoch: &str,
    purpose: &str,
    text: &str,
    delay_ms: u64,
    guard: &dyn Fn() -> Result<(), String>,
) -> SendReport {
    crate::recorded_send::off_worker(|| {
        send_recorded_inline(context, ob, key, epoch, purpose, text, delay_ms, guard)
    })
}

fn send_recorded_inline(
    context: &RuntimeContext,
    ob: &OneBot,
    key: &str,
    epoch: &str,
    purpose: &str,
    text: &str,
    delay_ms: u64,
    guard: &dyn Fn() -> Result<(), String>,
) -> SendReport {
    use crate::recorded_send::{send_chain, BeginError, History, Proof, Recorded, BATCH_BUSY};
    use crate::send_outbox::ItemState;
    let mut report = SendReport::default();
    let (Some(target), Some((kind, id_s))) = (target_of(key), key.split_once(':')) else {
        report.error = Some(format!("无效会话 {key}"));
        return report;
    };
    let canonical = crate::outbound_message::normalize(text);
    // The wire renderer trims bubble boundaries. Persist those exact parts, including when
    // the preferred newline boundary or a whitespace-only part came from the length splitter.
    let parts: Vec<_> = crate::onebot::split_message(&canonical, QQ_PART_LIMIT).into_iter()
        .map(|part| part.trim().to_owned()).filter(|part| !part.is_empty()).collect();
    report.parts = parts.len();
    if parts.is_empty() {
        return report;
    }
    // Owner rule (issue #70): a chat send may wait briefly behind another send in exchange for
    // one runner per batch and no blind duplicate.
    let _chain = send_chain();
    let delays: Vec<i64> = vec![delay_ms as i64; parts.len().saturating_sub(1)];
    let fields = json!({
        "key": key, "epoch": epoch, "purpose": purpose, "messages": parts,
        "delays": delays, "replyTo": Value::Null, "atUser": Value::Null
    });
    let recorded = match Recorded::begin(context, now_ms, now_ms(), fields) {
        Ok(recorded) => recorded,
        Err(BeginError::Busy) => {
            report.error = Some(BATCH_BUSY.to_string());
            return report;
        }
        Err(BeginError::Intent(error)) => {
            report.error = Some(format!("发送意图未能持久记录，未发送：{error}"));
            return report;
        }
    };
    let history = History { ob, kind, id_s };
    let mut confirmed_ids: Vec<i64> = Vec::new();
    let mut stop: Option<String> = None;
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 && delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        if stop.is_none() {
            if let Err(why) = guard() {
                stop = Some(format!("发送已取消：{why}"));
            }
        }
        let mut attempt_ms = 0;
        if stop.is_none() {
            match recorded.attempt(idx, None) {
                Ok(at) => attempt_ms = at,
                Err(error) => stop = Some(format!("发送意图未能持久记录，未发送：{error}")),
            }
        }
        if let Some(why) = &stop {
            let _ = recorded.cancelled(idx, Some(why));
            report.error.get_or_insert(why.clone());
            continue;
        }
        let outcome = ob.send_one_outcome(&target, part, None, None);
        let settled = recorded.settle(idx, attempt_ms, outcome, &history, Proof::Text(part), &confirmed_ids);
        if let ItemState::Confirmed { message_id, .. } = settled.state {
            report.confirmed += 1;
            confirmed_ids.extend(message_id);
            if let Some(error) = settled.record_error {
                // Preserve the known delivery count, surface its missing durable proof, and
                // stop later parts. Recovery retains the attempted item as uncertain.
                let why = format!("发送已确认，但确认结果未能持久记录：{error}");
                report.error = Some(why.clone());
                stop = Some(why);
            }
        } else if let Some(error) = settled.error {
            report.error.get_or_insert(error);
        }
    }
    report
}

// ───────────────────────── gateway lookups ─────────────────────────

/// Group member names and quoted messages through the OneBot HTTP API, cached like the old bridge.
pub struct Lookups {
    ob: OneBot,
    names: HashMap<String, (String, Instant)>,
    replies: HashMap<String, (Option<ReplyInfo>, Instant)>,
    group_names: HashMap<String, String>,
}

fn data_of(response: Option<Value>) -> Option<Value> {
    let v = response?;
    if v.get("status").and_then(Value::as_str) != Some("ok") || v.get("retcode").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    v.get("data").cloned()
}

impl Lookups {
    pub fn new(ob: OneBot) -> Self {
        Self { ob, names: HashMap::new(), replies: HashMap::new(), group_names: HashMap::new() }
    }

    fn display(info: &Value) -> Option<String> {
        ["card", "nickname"]
            .iter()
            .filter_map(|k| info.get(k).and_then(Value::as_str))
            .find(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn member_name(&mut self, group: &str, user: &str) -> Option<String> {
        let key = format!("{group}:{user}");
        if let Some((name, at)) = self.names.get(&key) {
            if at.elapsed() < NAME_TTL {
                return Some(name.clone());
            }
        }
        let (Ok(g), Ok(u)) = (group.parse::<i64>(), user.parse::<i64>()) else { return None };
        if let Some(name) = data_of(self.ob.request("get_group_member_info", json!({"group_id": g, "user_id": u, "no_cache": false})))
            .and_then(|info| Self::display(&info))
        {
            self.names.insert(key, (name.clone(), Instant::now()));
            return Some(name);
        }
        // Gateways without get_group_member_info: one full member list fills the cache.
        let list = data_of(self.ob.request("get_group_member_list", json!({"group_id": g})))?;
        let now = Instant::now();
        for member in list.as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if let (Some(name), id) = (Self::display(member), scalar_raw(member.get("user_id"))) {
                self.names.insert(format!("{group}:{id}"), (name, now));
            }
        }
        self.names.get(&key).map(|(n, _)| n.clone())
    }

    /// `resolveGroupName`: names are cached for the process; failures are not cached.
    pub fn group_name(&mut self, group: &str) -> String {
        if let Some(name) = self.group_names.get(group) {
            return name.clone();
        }
        if let Some(list) = data_of(self.ob.request("get_group_list", json!({}))) {
            for g in list.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                self.group_names.insert(scalar_raw(g.get("group_id")), scalar_raw(g.get("group_name")));
            }
        }
        self.group_names.get(group).cloned().unwrap_or_default()
    }

    /// `resolveReplyInfo`: only messages that belong to this conversation are readable.
    pub fn reply_info(&mut self, kind: &str, conv: &str, message_id: &str, self_id: i64) -> Option<ReplyInfo> {
        let cache_key = format!("{kind}:{conv}:{message_id}");
        if let Some((info, at)) = self.replies.get(&cache_key) {
            let ttl = if info.is_some() { REPLY_TTL } else { REPLY_NEGATIVE_TTL };
            if at.elapsed() < ttl {
                return info.clone();
            }
        }
        let info = self.fetch_reply(kind, conv, message_id, self_id);
        self.replies.insert(cache_key, (info.clone(), Instant::now()));
        if self.replies.len() > 1000 {
            self.replies.retain(|_, (info, at)| at.elapsed() < if info.is_some() { REPLY_TTL } else { REPLY_NEGATIVE_TTL });
        }
        info
    }

    fn fetch_reply(&mut self, kind: &str, conv: &str, message_id: &str, self_id: i64) -> Option<ReplyInfo> {
        // JS Number.isSafeInteger
        let numeric = message_id.trim().parse::<i64>().ok().filter(|n| n.abs() <= 9_007_199_254_740_991)?;
        let raw = data_of(self.ob.request("get_msg", json!({"message_id": numeric})))?;
        if raw.is_null() {
            return None;
        }
        let raw_group = raw.get("group_id").or_else(|| raw.get("groupId")).filter(|v| !v.is_null());
        if kind == "group" {
            if scalar_raw(raw_group) != conv || raw_group.is_none() {
                return None;
            }
        } else {
            let raw_user = raw.get("user_id").or_else(|| raw.get("userId")).or_else(|| raw.pointer("/sender/user_id")).filter(|v| !v.is_null());
            if raw_group.is_some() {
                return None;
            }
            let user = scalar_raw(raw_user);
            if raw_user.is_none() || (user != conv && user != self_id.to_string()) {
                return None;
            }
        }
        let sender_obj = raw.get("sender").cloned().unwrap_or(Value::Null);
        let sender = Self::display(&sender_obj)
            .unwrap_or_else(|| {
                let id = scalar_raw(sender_obj.get("user_id").or_else(|| raw.get("user_id")));
                if id.is_empty() { "未知".to_string() } else { id }
            });
        let text = {
            let mut res = GroupResolver { lookups: self, group: if kind == "group" { Some(conv.to_string()) } else { None }, kind: kind.to_string(), conv: conv.to_string(), self_id };
            segments_to_text(raw.get("message").unwrap_or(&Value::Null), &mut res, false)
        };
        let user_id = sender_obj.get("user_id").or_else(|| raw.get("user_id")).filter(|v| !v.is_null()).map(|v| scalar_raw(Some(v)));
        Some(ReplyInfo { sender, text: crate::role::utf16_take(&text, 200), user_id })
    }
}

/// Resolver bound to one conversation.
pub struct GroupResolver<'a> {
    pub lookups: &'a mut Lookups,
    pub group: Option<String>,
    pub kind: String,
    pub conv: String,
    pub self_id: i64,
}

impl Resolver for GroupResolver<'_> {
    fn at_name(&mut self, qq: &str) -> Option<String> {
        let group = self.group.clone()?;
        self.lookups.member_name(&group, qq)
    }
    fn reply(&mut self, message_id: &str) -> Option<ReplyInfo> {
        self.lookups.reply_info(&self.kind.clone(), &self.conv.clone(), message_id, self.self_id)
    }
}

// ───────────────────────── hub ─────────────────────────

/// One inbound chat message in the shape the hub needs.
pub struct Inbound {
    pub kind: &'static str,
    pub id: i64,
    pub user_id: i64,
    pub self_id: i64,
    pub card: String,
    pub nickname: String,
    pub message_id: Option<String>,
    pub message: Value,
}

pub enum Outcome {
    /// Nothing more to do for the event (it is settled in the journal).
    Done,
    /// The message went to the conversation's turn worker; `plain` feeds the slang window.
    Dispatched { plain: String, sender: String },
}

struct Job {
    prompt: String,
    media: Vec<Value>,
    ack: Option<String>,
    inbound: Vec<String>,
    mode: String,
    /// The owner sent the message that started this job.
    owner: bool,
    /// Resets of the conversation seen at dispatch; a later reset discards this queued job.
    resets: Option<usize>,
    /// `mode.json` as seen at dispatch: a mode transition (even away and back) discards the job.
    mode_stamp: Option<String>,
}

pub struct Hub {
    context: RuntimeContext,
    journal: SharedJournal,
    ob: OneBot,
    lookups: Lookups,
    workers: HashMap<String, tokio::sync::mpsc::UnboundedSender<Job>>,
    interactions: Option<std::sync::Arc<crate::closed_agent::Hub>>,
}

pub(crate) fn onebot_of(context: &RuntimeContext, cfg: &Value) -> Result<OneBot, String> {
    let url = context.dependency_url("onebot").ok_or_else(|| "runtime OneBot dependency is unavailable".to_string())?;
    let token = cfg.pointer("/snowluma/accessToken").and_then(Value::as_str).unwrap_or("").to_string();
    OneBot::try_new_url(url, token)
}

impl Hub {
    pub fn new(context: &RuntimeContext, journal: SharedJournal, cfg: &Value, interactions: Option<std::sync::Arc<crate::closed_agent::Hub>>) -> Result<Self, String> {
        Ok(Self {
            context: context.clone(),
            journal,
            ob: onebot_of(context, cfg)?,
            lookups: Lookups::new(onebot_of(context, cfg)?),
            workers: HashMap::new(),
            interactions,
        })
    }

    fn say(&self, cfg: &Value, key: &str, text: &str) {
        let context = self.context.clone();
        let guard_cfg = cfg.clone();
        let guard_key = key.to_string();
        let mode = crate::console::runtime_mode(&self.context);
        let guard = move || -> Result<(), String> {
            if crate::console::runtime_mode(&context) != mode || !matches!(mode.as_str(), "chat" | "closed-agent") {
                return Err("模式已变化".to_string());
            }
            if !allowed_in_mode(&guard_cfg, &mode, &guard_key) {
                return Err("会话或白名单已变化".to_string());
            }
            Ok(())
        };
        let report = send_recorded(&self.context, &self.ob, key, "chat", "command", text, send_delay_ms(cfg), &guard);
        if let Some(error) = report.error {
            println!("[chat] {key} 回复发送失败：{error}");
        }
    }

    async fn settle(&self, inbound_key: &str) -> Result<(), String> {
        self.journal.mark_handled(vec![inbound_key.to_string()], now_ms()).await
    }

    /// Handle one allowed, non-self, journaled chat message.
    pub async fn handle(&mut self, cfg: &Value, inb: &Inbound, inbound_key: &str) -> Result<Outcome, String> {
        let key = format!("{}:{}", inb.kind, inb.id);
        let conv = inb.id.to_string();
        let group = (inb.kind == "group").then(|| conv.clone());
        let mut res = GroupResolver { lookups: &mut self.lookups, group: group.clone(), kind: inb.kind.to_string(), conv: conv.clone(), self_id: inb.self_id };
        let text = segments_to_text(&inb.message, &mut res, true);
        let plain = segments_to_text(&inb.message, &mut res, false);
        // A quote of the bot's own message addresses the bot even without words of its own.
        let mut quote_self = false;
        for seg in inb.message.as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if seg.get("type").and_then(Value::as_str) == Some("reply") {
                if let Some(id) = seg.pointer("/data/id").filter(|v| !v.is_null()) {
                    if let Some(info) = res.lookups.reply_info(inb.kind, &conv, &scalar_raw(Some(id)), inb.self_id) {
                        if info.user_id.as_deref() == Some(inb.self_id.to_string().as_str()) {
                            quote_self = true;
                            break;
                        }
                    }
                }
            }
        }
        let owner = owner_qq(cfg);
        let is_owner = !owner.is_empty() && inb.user_id.to_string() == owner;
        if is_owner && !plain.is_empty() && crate::console::runtime_mode(&self.context) == "closed-agent" {
            if let Some(interactions) = &self.interactions {
                if interactions.answer(&key, &plain).await {
                    self.settle(inbound_key).await?;
                    return Ok(Outcome::Done);
                }
            }
        }
        let role_state = crate::console::chat_role_state(&self.context)?;
        let role = role_state.get("role").and_then(Value::as_str).map(str::to_string);
        let role_mode = role_state.get("mode").and_then(Value::as_str).unwrap_or("active").to_string();
        let session = crate::session_map::lookup(&self.context, &key)?;
        let context = self.context.clone();
        let exists = move |name: &str| {
            context
                .resolve_path(RuntimeArea::Resources, &format!("roles/{name}.md"), PathIntent::ReadFile)
                .map(|p| p.is_file())
                .unwrap_or(false)
        };
        let facts = Facts { plain: &plain, is_owner, quote_self, role: role.clone(), role_mode: role_mode.clone(), session, allowed: allowed(cfg, inb.kind, &conv) };
        let decision = decide(&facts, &exists);
        match decision {
            Decision::Ignore(why) => {
                if why == "silent-mode" {
                    crate::activity::append(
                        &self.context,
                        &format!("{key}（静默模式）群友 {}：{}", inb.user_id, crate::activity::slice(&text, 80)),
                    );
                }
                println!("[chat] {key} 忽略（{why}）");
                self.settle(inbound_key).await?;
                Ok(Outcome::Done)
            }
            Decision::Say(text) => {
                self.say(cfg, &key, &text);
                self.settle(inbound_key).await?;
                Ok(Outcome::Done)
            }
            Decision::SetRole { name, say } => {
                crate::console::chat_write_role_state(&self.context, name.as_deref(), &role_mode)?;
                self.say(cfg, &key, &say);
                self.settle(inbound_key).await?;
                Ok(Outcome::Done)
            }
            Decision::SetRoleMode { mode, say } => {
                crate::console::chat_write_role_state(&self.context, role.as_deref(), mode)?;
                self.say(cfg, &key, say);
                self.settle(inbound_key).await?;
                Ok(Outcome::Done)
            }
            Decision::Reset => {
                match crate::console::chat_reset_conversation(&self.context, &key) {
                    Ok(()) => self.say(cfg, &key, "已重置会话，下次消息将开新上下文"),
                    Err((404, _)) => {}
                    Err((_, error)) => {
                        println!("[chat] {key} 重置失败：{error}");
                        self.say(cfg, &key, &format!("重置失败：{error}"));
                    }
                }
                self.settle(inbound_key).await?;
                Ok(Outcome::Done)
            }
            Decision::Forward => {
                crate::activity::append(
                    &self.context,
                    &format!(
                        "{key} {} {}：{}",
                        if is_owner { "管理员" } else { "群友" },
                        if inb.nickname.is_empty() { inb.user_id.to_string() } else { inb.nickname.clone() },
                        crate::activity::slice(&text, 80)
                    ),
                );
                let message_ref = inb.message_id.clone().unwrap_or_default();
                let sender = if !inb.card.is_empty() {
                    inb.card.clone()
                } else if !inb.nickname.is_empty() {
                    inb.nickname.clone()
                } else {
                    inb.user_id.to_string()
                };
                let hint = crate::prompt::role_hint_v1(&self.context, cfg, &key);
                let prompt = build_prompt(
                    &hint,
                    is_owner,
                    group.as_ref().map(|_| sender.as_str()),
                    &text,
                    &media_hint(&message_ref, media_count(&inb.message)),
                );
                self.dispatch(cfg, &key, Job { prompt, media: crate::forward_fmt::collect_segment_meta(inb.message.as_array().map(Vec::as_slice).unwrap_or(&[])).0, ack: ack_message(cfg), inbound: vec![inbound_key.to_string()], mode: crate::console::runtime_mode(&self.context), owner: is_owner, resets: None, mode_stamp: None });
                Ok(Outcome::Dispatched { plain, sender })
            }
        }
    }

    fn dispatch(&mut self, cfg: &Value, key: &str, mut job: Job) {
        let _ = cfg;
        job.resets = crate::turn_ledger::reset_count(&self.context, key).ok();
        job.mode_stamp = crate::reserved::mode_stamp(&self.context).ok();
        let sender = self
            .workers
            .entry(key.to_string())
            .or_insert_with(|| {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::spawn(worker(self.context.clone(), self.journal.clone(), key.to_string(), rx));
                tx
            })
            .clone();
        if sender.send(job).is_err() {
            println!("[chat] {key} 的回合队列已关闭");
        }
    }
}

/// Why a queued job must not run any more (its epoch ended), or `None` when it is still current.
fn stale_job(context: &RuntimeContext, cfg: &Value, key: &str, job: &Job) -> Option<&'static str> {
    if crate::console::runtime_mode(context) != job.mode || !allowed_in_mode(cfg, &job.mode, key) {
        return Some("");
    }
    // Fail closed: a job whose epoch cannot be proven current (not read at dispatch, or not
    // readable now) is dropped like a stale one.
    let Some(seen) = job.resets else {
        return Some("排队时未能读取会话重置记录，丢弃排队的指令");
    };
    match crate::turn_ledger::reset_count(context, key) {
        Ok(now) if now == seen => {}
        Ok(_) => return Some("会话已重置或权限已撤销，丢弃排队的指令"),
        Err(_) => return Some("读取会话重置记录失败，丢弃排队的指令"),
    }
    let Some(seen) = &job.mode_stamp else {
        return Some("排队时未能读取模式记录，丢弃排队的指令");
    };
    match crate::reserved::mode_stamp(context) {
        Ok(now) if &now == seen => None,
        Ok(_) => Some("模式已切换，丢弃排队的指令"),
        Err(_) => Some("读取模式记录失败，丢弃排队的指令"),
    }
}

/// One conversation's turns, strictly one after the other (a reply never overtakes the next
/// message's turn), while other conversations and commands such as `/reset` keep running.
///
/// While DSH is down the turns wait here (at most 50 per conversation, the oldest dropped, no time limit: JS `QUEUE_MAX`) and no call is made;
/// after recovery each one goes through the same epoch checks as any queued job (`stale_job`, then the check right before
/// the turn begins) before it runs, so a conversation revoked, reset or switched meanwhile never gets its old message
/// delivered.
async fn worker(
    context: RuntimeContext,
    journal: SharedJournal,
    key: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
) {
    use crate::dsh_health::{is_down, BoundedQueue, QUEUE_MAX};
    let mut pipeline: Option<crate::pipeline::Pipeline> = None;
    let mut waiting: BoundedQueue<Job> = BoundedQueue::new(QUEUE_MAX);
    // The cap drops the oldest: its events are settled (never silently left open) and the loss is on the record.
    macro_rules! park {
        ($job:expr, $front:expr) => {{
            let dropped = if $front { waiting.push_front($job) } else { waiting.push_back($job) };
            if let Some(old) = dropped {
                println!("[chat] {key} 等待 DSH 的队列已满（{QUEUE_MAX}），丢弃最早的一条");
                crate::recovery_log::record(&context, crate::run_record::Trigger::Turn, crate::run_record::Component::Queue, "dropped-oldest", crate::run_record::Outcome::Failed, &format!("{key}: queue of {QUEUE_MAX} waiting messages is full"));
                let _ = journal.mark_handled(old.inbound, now_ms()).await;
            }
        }};
    }
    loop {
        let job = if !waiting.is_empty() {
            let needs_dsh = context.load_config().map(|cfg|
                matches!(crate::harness::Engine::selected(&cfg, &key), Ok(crate::harness::Engine::Dsh))
            ).unwrap_or(true);
            if is_down(&context) && needs_dsh {
                // Still down: keep receiving (bounded) and look again shortly; nothing is sent to DSH.
                tokio::select! {
                    next = rx.recv() => match next {
                        Some(job) => park!(job, false),
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
                continue;
            }
            let job = waiting.pop_front().expect("checked non-empty");
            println!("[chat] {key} DSH 已恢复，补投排队的消息（先核对权限与会话状态）");
            job
        } else {
            match rx.recv().await {
                Some(job) => job,
                None => break,
            }
        };
        if let Err(error) = journal.mark_processing(job.inbound.clone(), now_ms()).await {
            println!("[chat] ❌ {key} 事件状态写入失败：{error}");
        }
        let cfg = match context.load_config() {
            Ok(cfg) => cfg,
            Err(error) => {
                println!("[chat] ❌ {key} 读取配置失败：{error}");
                let _ = journal.mark_handled(job.inbound, now_ms()).await;
                continue;
            }
        };
        if let Some(reason) = stale_job(&context, &cfg, &key, &job) {
            println!("[chat] {key} {}", if reason.is_empty() { "模式或权限已变化，丢弃排队的指令" } else { reason });
            let _ = journal.mark_handled(job.inbound, now_ms()).await;
            continue;
        }
        let needs_dsh = matches!(crate::harness::Engine::selected(&cfg, &key), Ok(crate::harness::Engine::Dsh));
        if is_down(&context) && needs_dsh {
            // Known down: hold it without a call (a storm of doomed requests is what the queue is for).
            println!("[chat] {key} DSH 不可用，消息排队等待恢复");
            park!(job, true);
            continue;
        }
        if pipeline.is_none() {
            match crate::pipeline::Pipeline::new_with_runtime(&context, &cfg, true) {
                Ok(p) => pipeline = Some(p),
                Err(error) => {
                    println!("[chat] ❌ {key} 无法初始化回合管线：{error}");
                    let _ = journal.mark_handled(job.inbound, now_ms()).await;
                    continue;
                }
            }
        }
        let pipe = pipeline.as_mut().expect("pipeline set above");
        // The same epoch check runs again right before the turn begins (after media downloads).
        let (check_context, check_key) = (context.clone(), key.clone());
        let epoch = Job {
            prompt: String::new(), media: Vec::new(), ack: None, inbound: Vec::new(),
            mode: job.mode.clone(), owner: job.owner, resets: job.resets, mode_stamp: job.mode_stamp.clone(),
        };
        pipe.recheck_before_turn(Box::new(move || {
            let cfg = match check_context.load_config() {
                Ok(cfg) => cfg,
                Err(_) => return Some("读取配置失败，丢弃排队的指令".to_string()),
            };
            stale_job(&check_context, &cfg, &check_key, &epoch)
                .map(|reason| if reason.is_empty() { "模式或权限已变化，丢弃排队的指令" } else { reason }.to_string())
        }));
        // DSH cookies expire; a fresh login per turn is cheap.
        if needs_dsh { let _ = pipe.rpc.relogin(); }
        let result = pipe.run_chat_turn(&key, &job.prompt, job.ack.clone(), &cfg, &job.inbound, &job.media, &job.mode, job.owner).await;
        match result {
            Ok(()) => println!("[chat] {key} 回合完成"),
            Err(error) => {
                println!("[chat] ❌ {key} 回合失败：{error}");
                // The prompt never reached DSH and DSH is unreachable: wait for it instead of losing the message.
                if needs_dsh && pipe.dispatch_definitely_unsubmitted() && is_down(&context) {
                    println!("[chat] {key} DSH 不可用，该消息排队等待恢复");
                    crate::admin_alert::notify_notice(
                        &context,
                        &cfg,
                        &format!("[{key}] DSH 不可用，消息已排队，恢复后会重新核对权限再补投"),
                        "turn-failure",
                    );
                    park!(job, true);
                    continue;
                }
                // Model errors and refusals were already reported by the pipeline with their own text.
                if crate::pipeline::take_model_error(&context, &key).is_none() && !error.contains(" 被拒：") && !crate::pipeline::is_cancellation_error(&error) {
                    crate::admin_alert::notify_notice(
                        &context,
                        &cfg,
                        &format!("[{key}] 回合失败：{}", crate::activity::slice(&error, 300)),
                        "turn-failure",
                    );
                }
            }
        }
        if let Err(error) = journal.mark_handled(job.inbound, now_ms()).await {
            println!("[chat] ❌ {key} 事件状态写入失败：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_reply_surfaces_a_discovery_refusal_instead_of_no_update() {
        let refused = "release checksum signature missing, invalid or untrusted";
        let reply = no_update_reply(&json!({"current":"v1.2.3","available":null,"error":refused}));
        assert!(reply.contains(refused), "{reply}");
        assert!(reply.contains("v1.2.3"), "{reply}");
        assert!(!reply.contains("暂无可用更新"), "{reply}");
        let reply = no_update_reply(&json!({"current":"v1.2.3","available":null,"error":null}));
        assert!(reply.contains("暂无可用更新"), "{reply}");
    }

    struct Fake {
        names: HashMap<String, String>,
        replies: HashMap<String, ReplyInfo>,
        asked: Vec<String>,
    }
    impl Resolver for Fake {
        fn at_name(&mut self, qq: &str) -> Option<String> {
            self.asked.push(format!("at:{qq}"));
            self.names.get(qq).cloned()
        }
        fn reply(&mut self, id: &str) -> Option<ReplyInfo> {
            self.asked.push(format!("reply:{id}"));
            self.replies.get(id).cloned()
        }
    }
    fn fake() -> Fake {
        Fake {
            names: HashMap::from([("1002".to_string(), "Alice".to_string())]),
            replies: HashMap::from([("5".to_string(), ReplyInfo { sender: "bot".into(), text: "原话".into(), user_id: Some("9".into()) })]),
            asked: Vec::new(),
        }
    }

    /// Ticket 24 S5: a queued job whose epoch cannot be proven current does not run.
    #[test]
    fn ticket24_a_queued_job_fails_closed_when_its_epoch_cannot_be_read() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-t24-chat-stale-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","allow":{"groups":[42]}}"#).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"chat"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let cfg = context.load_config().unwrap();
        let key = "group:42";
        let job = |resets: Option<usize>, mode_stamp: Option<String>| Job {
            prompt: String::new(), media: Vec::new(), ack: None, inbound: Vec::new(), mode: "chat".into(), owner: false, resets, mode_stamp,
        };
        let stamp = crate::reserved::mode_stamp(&context).ok();
        assert_eq!(stale_job(&context, &cfg, key, &job(Some(0), stamp.clone())), None, "a current job runs");
        let unproven = [
            ("no reset count at dispatch", stale_job(&context, &cfg, key, &job(None, stamp.clone()))),
            ("no mode stamp at dispatch", stale_job(&context, &cfg, key, &job(Some(0), None))),
        ];
        std::fs::create_dir_all(root.join("data/turn-ledger.jsonl")).unwrap();
        let unreadable = stale_job(&context, &cfg, key, &job(Some(0), stamp));
        std::fs::remove_dir_all(&root).unwrap();
        for (case, verdict) in unproven {
            assert!(verdict.is_some(), "{case}: the job ran without a provable epoch");
        }
        assert!(unreadable.is_some(), "an unreadable reset count let the queued job run");
    }

    /// Ticket 24 second fix round (Codex 6): under `allowAllWhenEmpty` a first-contact conversation
    /// has no mapping, record or ledger turn yet. Narrowing the policy and re-granting it before its
    /// queued job runs must still end that job's epoch.
    #[test]
    fn ticket24_fix2_a_recordless_allow_all_job_does_not_survive_a_narrowing_and_regrant() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-t24-chat-allowall-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghijabcdef","allow":{"groups":[]},"allowAllWhenEmpty":true}"#).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"chat"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let key = "group:77";
        let cfg = context.load_config().unwrap();
        assert!(allowed_in_mode(&cfg, "chat", key), "allow-all admits the first contact");
        // Dispatch captures the epoch exactly as `Hub::dispatch` does.
        let job = Job {
            prompt: String::new(), media: Vec::new(), ack: None, inbound: Vec::new(), mode: "chat".into(), owner: false,
            resets: crate::turn_ledger::reset_count(&context, key).ok(),
            mode_stamp: crate::reserved::mode_stamp(&context).ok(),
        };
        let (status, reply) = crate::console::admin_request(&context, "POST", "/api/whitelist", Some(&serde_json::json!({"allow": {"groups": [42]}})));
        assert_eq!(status, 200, "{reply}");
        let (_, recent) = crate::console::admin_request(&context, "GET", "/api/audit/recent", None);
        let id = recent["entries"].as_array().unwrap().iter().find(|e| e["path"] == "/api/whitelist").unwrap()["id"].clone();
        let (status, reply) = crate::console::admin_request(&context, "POST", "/api/audit/undo", Some(&serde_json::json!({"id": id})));
        assert_eq!(status, 200, "{reply}");
        let cfg = context.load_config().unwrap();
        let regranted = allowed_in_mode(&cfg, "chat", key);
        let verdict = stale_job(&context, &cfg, key, &job);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(regranted, "the undo restores allow-all");
        assert!(verdict.is_some(), "a job queued before the narrowing ran after the re-grant");
    }

    #[test]
    fn md_to_plain_flattens_like_the_old_helper() {
        assert_eq!(md_to_plain("**加粗** 和 [链接](http://x.invalid/a)"), "加粗 和 链接 (http://x.invalid/a)");
        assert_eq!(md_to_plain("# 标题\n- 一\n* 二\n1. 三\n> 引用"), "标题\n• 一\n• 二\n1. 三\n引用");
        assert_eq!(md_to_plain("```rust\nlet a = 1;\n```\n后文"), "let a = 1;\n后文");
        assert_eq!(md_to_plain("`行内` ~~删~~ __下__ ![alt](http://i.invalid/p.png) ![](http://i.invalid/q.png)"), "行内 删 下 alt http://i.invalid/q.png");
        assert_eq!(md_to_plain("2 * 3 * 4"), "2 * 3 * 4", "spaces next to the stars: not emphasis");
        assert_eq!(md_to_plain("*斜体*"), "斜体");
        assert_eq!(md_to_plain("| a | b |\n| --- | --- |\n| 1 | 2 |"), "a | b \n 1 | 2");
        assert_eq!(md_to_plain("甲\n\n\n\n乙"), "甲\n\n乙");
        assert_eq!(md_to_plain("  \n"), "");
    }

    #[test]
    fn segments_become_text_with_names_quotes_and_placeholders() {
        let msg = json!([
            {"type": "reply", "data": {"id": "5"}},
            {"type": "at", "data": {"qq": "1002"}},
            {"type": "at", "data": {"qq": "777"}},
            {"type": "at", "data": {"qq": "all"}},
            {"type": "text", "data": {"text": " 好"}},
            {"type": "face", "data": {"id": 14}},
            {"type": "mface", "data": {"summary": "给你糖吃"}},
            {"type": "image", "data": {}}, {"type": "record", "data": {}}, {"type": "video", "data": {}},
            {"type": "file", "data": {"name": "a.zip"}}, {"type": "json", "data": {}},
            {"type": "forward", "data": {"id": "fw-1"}}, {"type": "weird", "data": {}}
        ]);
        let mut r = fake();
        assert_eq!(
            segments_to_text(&msg, &mut r, true),
            "[引用 bot：原话]@Alice@777@全体成员 好[表情14][表情:给你糖吃][图片][语音][视频][文件a.zip][卡片消息][转发消息 id=fw-1][weird]"
        );
        let mut r = fake();
        let own = segments_to_text(&msg, &mut r, false);
        assert!(own.starts_with("@Alice@777"), "{own}");
        assert!(!r.asked.iter().any(|a| a.starts_with("reply:")), "quotes are not resolved when excluded");
        assert_eq!(segments_to_text(&json!("  纯文本  "), &mut fake(), true), "纯文本");
        assert_eq!(segments_to_text(&json!([{"type": "reply", "data": {"id": "404"}}]), &mut fake(), true), "[引用消息]");
    }

    /// JS `out.push(d.text ?? '')` and `[${seg?.type ?? '未知'}]` stringify whatever the gateway sent.
    #[test]
    fn non_string_text_and_type_print_the_value_like_js() {
        let msg = json!([
            {"type": "text", "data": {"text": 12}},
            {"type": "text", "data": {"text": false}},
            {"type": "text", "data": {"text": [1, null, "b"]}},
            {"type": "text", "data": {"text": {"a": 1}}},
            {"type": "text", "data": {"text": null}},
            {"type": 5, "data": {}},
            {"type": true},
            {"type": [1, 2]},
            {"type": {"k": 1}},
            {"type": null},
            {"data": {}},
            null
        ]);
        assert_eq!(
            segments_to_text(&msg, &mut fake(), true),
            "12false1,,b[object Object][5][true][1,2][[object Object]][未知][未知][未知]"
        );
    }

    #[test]
    fn allow_and_deny_lists_follow_the_old_rules() {
        let cfg = json!({"allow": {"groups": [1, "2"], "privates": ["3"]}, "deny": {"group": ["2"]}});
        assert!(allowed(&cfg, "group", "1"));
        assert!(!allowed(&cfg, "group", "2"), "deny beats allow");
        assert!(!allowed(&cfg, "group", "9"));
        assert!(allowed(&cfg, "private", "3"), "`privates` alias");
        assert!(!allowed(&json!({}), "group", "1"));
        assert!(allowed(&json!({"allowAllWhenEmpty": true}), "group", "1"));
        assert!(!allowed(&json!({"allowAllWhenEmpty": true, "deny": {"groups": ["1"]}}), "group", "1"));
        assert!(allowed_key(&cfg, "private:3") && !allowed_key(&cfg, "private:x") && !allowed_key(&cfg, "channel:3"));
    }

    #[test]
    fn update_commands_are_the_whole_message_forms_only() {
        for command in ["/update", "/UPDATE", "  /update  ", "/update deps", "/Update DEPS", "/update\tdeps"] {
            assert!(is_update_command(command), "{command:?}");
        }
        for ordinary in ["/update 一下进度", "/update deps now", "/update deps2", "/updates", "/update-all", "update", "请 /update", "/update\n一下", ""] {
            assert!(!is_update_command(ordinary), "{ordinary:?} is not an update command");
        }
    }

    fn facts(plain: &str, owner: bool) -> Facts<'_> {
        Facts { plain, is_owner: owner, quote_self: false, role: None, role_mode: "active".into(), session: None, allowed: true }
    }

    #[test]
    fn decisions_follow_the_old_order() {
        let exists = |n: &str| n == "小鲸鱼";
        assert_eq!(decide(&facts("", true), &exists), Decision::Ignore("empty"));
        let mut quote = facts("", false);
        quote.quote_self = true;
        assert_eq!(decide(&quote, &exists), Decision::Forward);
        let mut silent = facts("/reset", false);
        silent.role_mode = "silent".into();
        assert_eq!(decide(&silent, &exists), Decision::Ignore("silent-mode"), "silent mode comes before the member-slash drop");
        assert_eq!(decide(&facts("请切换角色", false), &exists), Decision::Forward, "role-control words are ordinary discussion");
        assert_eq!(decide(&facts("切换角色", true), &exists), Decision::Forward, "the owner may say it");
        assert_eq!(decide(&facts("/reset", true), &exists), Decision::Reset);
        assert_eq!(decide(&facts("/new", true), &exists), Decision::Reset);
        assert_eq!(decide(&facts("/role", true), &exists), Decision::SetRole { name: None, say: "已清除角色，恢复正常人格。".into() });
        assert_eq!(decide(&facts("/role clear", true), &exists), Decision::SetRole { name: None, say: "已清除角色，恢复正常人格。".into() });
        assert_eq!(decide(&facts("/role  小鲸鱼 ", true), &exists), Decision::SetRole { name: Some("小鲸鱼".into()), say: "已切换角色：小鲸鱼。".into() });
        assert_eq!(decide(&facts("/role 没有", true), &exists), Decision::Say("角色「没有」不存在。角色文件放 qq-bridge/roles/ 目录。".into()));
        assert_eq!(decide(&facts("/roles", true), &exists), Decision::Forward, "only `/role` and `/role <name>` are commands");
        assert_eq!(decide(&facts("/model", true), &exists), Decision::Forward, "other slash text goes to DSH");
        assert_eq!(decide(&facts("/update 一下进度", true), &exists), Decision::Forward, "only the whole-message update forms are commands");
        for slash in ["/reset", "/status", "/silent", "/role 小鲸鱼", "/model", "/update", "/usr/bin/env"] {
            assert_eq!(decide(&facts(slash, false), &exists), Decision::Ignore("member-slash"), "{slash}: a member's slash text is dropped, not forwarded");
        }
        let mut quoting = facts("/model", false);
        quoting.quote_self = true;
        assert_eq!(decide(&quoting, &exists), Decision::Ignore("member-slash"), "quoting the bot does not make slash text input");
        assert_eq!(decide(&facts("请问 /model 怎么用", false), &exists), Decision::Forward, "a slash inside ordinary text stays input");
        let mut s = facts("/status", true);
        s.session = Some("sess-1".into());
        s.role = Some("甲".into());
        s.role_mode = "silent".into();
        assert_eq!(decide(&s, &exists), Decision::Say("会话 sess-1；白名单 通过；角色 甲；模式 silent".into()));
        assert!(matches!(decide(&facts("/silent", true), &exists), Decision::SetRoleMode { mode: "silent", .. }));
        assert!(matches!(decide(&facts("/speak", true), &exists), Decision::SetRoleMode { mode: "active", .. }));
    }

    #[test]
    fn command_words_and_slash_text_in_ordinary_messages_reach_the_model() {
        let exists = |n: &str| n == "小鲸鱼";
        let slash = [
            "/usr/bin/env", "/resetting the router", "/status page is down", "/roles", "/model", "  /reset",
            "/reset", "/new", "/status", "/silent", "/quiet", "/active", "/speak", "/role", "/role 小鲸鱼", "/role off", "/reset now", "/status ok?",
        ];
        for text in [
            "游戏里怎么切换角色？",
            "进入角色扮演 听起来很有趣",
            "他说要退出角色扮演、关闭角色扮演，再开启角色扮演",
            "设置角色 改角色 换角色",
            "这个路径 /usr/bin/env 在哪",
            "／reset",
        ] {
            for owner in [false, true] {
                assert_eq!(decide(&facts(text, owner), &exists), Decision::Forward, "owner={owner}: {text}");
            }
        }
        for text in ["/usr/bin/env", "/resetting the router", "/status page is down", "/roles", "/model", "  /reset"] {
            assert_eq!(decide(&facts(text, true), &exists), Decision::Forward, "owner slash text goes on to DSH: {text}");
        }
        for text in slash {
            assert_eq!(decide(&facts(text, false), &exists), Decision::Ignore("member-slash"), "a member never runs or forwards {text}");
        }
        for text in ["/reset now", "/status ok?", "/silent please", "/new chat"] {
            assert_eq!(decide(&facts(text, true), &exists), Decision::Forward, "only the bare command name runs: {text}");
        }
    }

    #[test]
    fn owner_command_names_ignore_ascii_case_without_changing_role_names() {
        let exists = |name: &str| name == "MixedCase";
        assert_eq!(decide(&facts("/ReSeT", true), &exists), Decision::Reset);
        assert_eq!(decide(&facts("/NeW", true), &exists), Decision::Reset);
        assert!(matches!(decide(&facts("/StAtUs", true), &exists), Decision::Say(text) if text.starts_with("会话 ")));
        assert!(matches!(decide(&facts("/SiLeNt", true), &exists), Decision::SetRoleMode { mode: "silent", .. }));
        assert!(matches!(decide(&facts("/AcTiVe", true), &exists), Decision::SetRoleMode { mode: "active", .. }));
        assert_eq!(decide(&facts("/RoLe MixedCase", true), &exists), Decision::SetRole {
            name: Some("MixedCase".into()), say: "已切换角色：MixedCase。".into()
        });
    }

    #[test]
    fn prompts_follow_the_old_layout() {
        assert_eq!(build_prompt("", false, Some("小明"), "你好", ""), "小明：你好");
        assert_eq!(build_prompt("角色", true, Some("owner"), "你好", "\n【图片/表情】本条消息包含 2 个图片/表情（消息ID=7）。"), "角色\n\n【管理员】owner：你好\n【图片/表情】本条消息包含 2 个图片/表情（消息ID=7）。");
        assert_eq!(build_prompt("", true, None, "私聊", ""), "【管理员】私聊");
        assert_eq!(media_hint("", 2), "");
        assert_eq!(media_hint("7", 0), "");
        assert_eq!(media_count(&json!([{"type": "image"}, {"type": "face"}, {"type": "mface"}, {"type": "text"}])), 3);
    }

    #[test]
    fn replies_follow_the_turn_end_checks() {
        let none: Vec<String> = Vec::new();
        assert_eq!(reply_plan("completed", "**好**", false, &none), Reply::Send("好".into()));
        assert_eq!(reply_plan("completed", "  ", false, &none), Reply::Silent("no-text"));
        assert_eq!(reply_plan("completed", "**  **", false, &none), Reply::Silent("empty-after-markdown"));
        assert_eq!(reply_plan("completed", " [silent] ", false, &none), Reply::Silent("silent-marker"));
        assert_eq!(reply_plan("completed", "好", true, &none), Reply::Silent("tool-already-sent"), "a send tool that succeeded suppresses the auto-forward");
        assert!(matches!(reply_plan("completed", "在 C:\\Users\\x\\a.txt", false, &none), Reply::Blocked(_)));
        assert!(matches!(reply_plan("completed", "令牌 tok-12345", false, &["tok-12345".to_string()]), Reply::Blocked(_)));
        assert_eq!(reply_plan("aborted", "", false, &none), Reply::Stopped);
        assert_eq!(reply_plan("error", "x", false, &none), Reply::Silent("no-text"));
    }

    #[test]
    fn titles_and_configuration_defaults() {
        assert_eq!(session_title("group:7", "测试群"), "QQ 群 · 测试群");
        assert_eq!(session_title("group:7", ""), "QQ 群 · 7");
        assert_eq!(session_title("private:8", ""), "QQ 私聊 · 8");
        assert_eq!(ack_message(&json!({})).as_deref(), Some(DEFAULT_ACK));
        assert_eq!(ack_message(&json!({"ackMessage": ""})), None);
        assert_eq!(ack_message(&json!({"ackMessage": "收到"})).as_deref(), Some("收到"));
        assert_eq!(send_delay_ms(&json!({})), 300);
        assert_eq!(send_delay_ms(&json!({"sendDelayMs": 0})), 0);
        assert_eq!(chat_preset(&json!({})), "qq-chat");
        assert_eq!(chat_preset(&json!({"agentPreset": "x"})), "x");
        assert!(is_owner_key(&json!({"ownerQQ": 8}), "private:8") && !is_owner_key(&json!({}), "private:"));
    }
    #[test]
    fn hf123_chat_confirmation_append_failure_is_reported_and_stops_remaining_parts() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-hf123-confirm-{}", crate::turn_ledger::mint_id("")
        )));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ob = OneBot::new(&format!("http://{}", listener.local_addr().unwrap()), String::new());
        let data = root.join("data");
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut count = 0;
            while std::time::Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length: usize = headers.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                        }).unwrap();
                        if bytes.len() >= end + 4 + length { break; }
                    }
                }
                count += 1;
                if count == 1 {
                    // Fail persistence after the first attempt has become durable.
                    std::fs::rename(data.join("send-outbox.jsonl"), data.join("saved-outbox.jsonl")).unwrap();
                    std::fs::create_dir(data.join("send-outbox.jsonl")).unwrap();
                }
                let body = r#"{"status":"ok","retcode":0,"data":{"message_id":123}}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            count
        });
        let text = "x".repeat(QQ_PART_LIMIT + 10);
        let guard_calls = std::cell::Cell::new(0);
        let guard_data = root.join("data");
        let guard = || {
            let calls = guard_calls.get();
            guard_calls.set(calls + 1);
            if calls > 0 && guard_data.join("send-outbox.jsonl").is_dir() {
                // If a later part is admitted, its writes can succeed again. The failed
                // confirmation must itself stop the batch rather than rely on another I/O error.
                std::fs::remove_dir(guard_data.join("send-outbox.jsonl")).unwrap();
                std::fs::rename(guard_data.join("saved-outbox.jsonl"), guard_data.join("send-outbox.jsonl")).unwrap();
            }
            Ok(())
        };
        let report = send_recorded(&context, &ob, "group:42", "chat", "reply", &text, 0, &guard);
        let sends = worker.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(report.parts, 2);
        assert!(report.error.as_deref().is_some_and(|e| e.contains("确认") && e.contains("记录")), "{report:?}");
        assert_eq!(report.confirmed, 1, "known delivery must remain counted");
        assert_eq!(sends, 1, "stop later parts when confirmed delivery cannot be persisted");
    }

    #[test]
    fn hf124_chat_reply_audit_checks_face_normalized_paths_and_known_secrets() {
        for text in ["/ro[表情]ot/fixture.txt", "opaque-fixture-[表情[表情]]value-Q7N4"] {
            assert!(matches!(reply_plan_with_audit("completed", text, false,
                &["opaque-fixture-value-Q7N4".into()], true), Reply::Blocked(_)), "{text}");
        }
    }


    /// A gateway that loses every send reply and answers login and group history (issue #70).
    fn issue70_gateway(history: Value) -> (OneBot, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ob = OneBot::new(&format!("http://{}", listener.local_addr().unwrap()), String::new());
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut paths = Vec::new();
            while paths.len() < 4 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { std::thread::sleep(Duration::from_millis(10)); continue; }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let (mut bytes, mut buf) = (Vec::new(), [0u8; 4096]);
                let head = loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                        let length: usize = head.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                        }).unwrap_or(0);
                        if bytes.len() >= end + 4 + length { break head; }
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                paths.push(path.clone());
                let body = match path.as_str() {
                    "/send_group_msg" => continue, // receipt lost: close without a reply
                    "/get_login_info" => json!({"status":"ok","retcode":0,"data":{"user_id":10001}}),
                    "/get_group_msg_history" => json!({"status":"ok","retcode":0,"data":{"messages":history.clone()}}),
                    _ => json!({"status":"failed","retcode":1404}),
                }.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            paths
        });
        (ob, worker)
    }

    fn issue70_root(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-issue70-{tag}-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    #[test]
    fn issue70_chat_unknown_part_is_checked_against_history_and_never_resent() {
        let (root, context) = issue70_root("unproven");
        let (ob, gateway) = issue70_gateway(json!([]));
        let guard = || -> Result<(), String> { Ok(()) };
        let report = send_recorded(&context, &ob, "group:42", "chat", "reply", "未证实", 0, &guard);
        let paths = gateway.join().unwrap();
        let kinds: Vec<String> = crate::send_outbox::read_records(&context).unwrap().iter().filter_map(|r| r["k"].as_str().map(str::to_string)).collect();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(paths.iter().filter(|p| *p == "/send_group_msg").count(), 1, "an unknown part is never re-sent: {paths:?}");
        assert!(paths.iter().any(|p| p == "/get_group_msg_history"), "the unknown part was not checked against history: {paths:?}");
        assert_eq!(report.confirmed, 0);
        assert!(report.error.is_some(), "{report:?}");
        assert_eq!(kinds, ["intent", "attempt", "unknown"]);
    }

    #[test]
    fn issue70_chat_unknown_part_proven_by_history_is_confirmed_as_verified() {
        let (root, context) = issue70_root("proven");
        let at = crate::parity_clock::now_ms() / 1000 + 5;
        let (ob, gateway) = issue70_gateway(json!([{ "message_id": 4242, "time": at, "sender": {"user_id": 10001},
            "message": [{"type": "text", "data": {"text": "已送达"}}] }]));
        let guard = || -> Result<(), String> { Ok(()) };
        let report = send_recorded(&context, &ob, "group:42", "chat", "reply", "已送达", 0, &guard);
        let paths = gateway.join().unwrap();
        let records = crate::send_outbox::read_records(&context).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(paths.iter().filter(|p| *p == "/send_group_msg").count(), 1, "{paths:?}");
        assert_eq!((report.confirmed, report.error.as_deref()), (1, None), "{report:?}");
        let kinds: Vec<_> = records.iter().filter_map(|r| r["k"].as_str()).collect();
        assert_eq!(kinds, ["intent", "attempt", "unknown", "confirmed"]);
        assert_eq!(records[3]["messageId"], 4242);
        assert_eq!(records[3]["verified"], true);
    }

    /// Issue #82: a chat send waiting behind a held chain must not occupy the runtime worker.
    #[test]
    fn issue82_chat_send_waiting_on_held_chain_leaves_ingress_running() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (root, context) = issue70_root("chain-wait");
        let records = context.clone();
        let held = crate::recorded_send::send_chain();
        // Never contacted: the guard refuses before any request.
        let ob = OneBot::new("http://127.0.0.1:1", String::new());
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let send = runtime.spawn(async move {
            entered_tx.send(()).unwrap();
            let guard = || -> Result<(), String> { Err("fixture stop".into()) };
            send_recorded(&context, &ob, "group:42", "chat", "reply", "排队中", 0, &guard)
        });
        entered.recv_timeout(Duration::from_secs(5)).expect("send task never started");
        std::thread::sleep(Duration::from_millis(200)); // let it reach the chain
        let (ingress_tx, ingress) = std::sync::mpsc::channel();
        runtime.spawn(async move { let _ = ingress_tx.send(()); });
        let progressed = ingress.recv_timeout(Duration::from_secs(3)).is_ok();
        let waiting = !send.is_finished();
        let before = crate::send_outbox::read_records(&records).unwrap().len();
        drop(held);
        let report = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), send).await })
            .expect("send did not finish after the chain was released")
            .unwrap();
        let kinds: Vec<String> = crate::send_outbox::read_records(&records).unwrap().iter()
            .filter_map(|r| r["k"].as_str().map(str::to_string)).collect();
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
        assert!(progressed, "an ingress task starved while a chat send waited behind the held chain");
        assert!(waiting && before == 0, "the chat send must wait for the chain before recording its intent");
        assert!(report.error.as_deref().is_some_and(|e| e.contains("fixture stop")), "{report:?}");
        assert_eq!(kinds, ["intent", "cancelled"]);
    }

    /// Issue #82: on a current-thread runtime the send runs inline (block_in_place would panic).
    #[test]
    fn issue82_chat_send_on_current_thread_runtime_runs_inline() {
        let (root, context) = issue70_root("inline");
        let ob = OneBot::new("http://127.0.0.1:1", String::new());
        let guard = || -> Result<(), String> { Err("fixture stop".into()) };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let report = runtime.block_on(async { send_recorded(&context, &ob, "group:42", "chat", "reply", "内联", 0, &guard) });
        let kinds: Vec<String> = crate::send_outbox::read_records(&context).unwrap().iter()
            .filter_map(|r| r["k"].as_str().map(str::to_string)).collect();
        std::fs::remove_dir_all(root).unwrap();
        assert!(report.error.as_deref().is_some_and(|e| e.contains("fixture stop")), "{report:?}");
        assert_eq!(kinds, ["intent", "cancelled"]);
    }
}
