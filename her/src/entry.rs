mod dsh;
mod dsh_credential;
mod digest;
mod onebot;
mod outbound_message;
mod mcp;
mod aux_text;
mod aux_fetch;
mod aux_search;
mod aux_mcp;
mod dsh_mcp_config;
mod qq_tools;
mod model_view;
mod model_failure;
mod mcp_validate;
mod memory;
mod pipeline;
mod harness;
mod prompt;
mod initial_prompts;
mod activity;
mod feedback;
mod wake;
mod question;
#[cfg(test)]
mod wake_rate_tests;
mod session;
mod reserved2;
mod reserved;
mod sticker;
mod failover;
mod console;
mod desktop;
mod qq_login;
mod pricing;
mod ledger;
mod report;
mod role;
mod social_mem;
mod slang;
mod slang_learn;
mod wait_util;
mod backend;
mod safe_fetch;
mod media_normalize;
mod media;
mod subprocess;
mod voice;
mod forward_fmt;
mod preset_prompt;
mod runtime;
mod install_root;
mod scheduled_task;
mod release;
mod release_signature;
mod release_store;
mod update;
mod parity_clock;
mod config;
mod inbound;
mod send_outbox;
mod session_map;
mod session_context;
mod session_rotation;
mod turn_ledger;
mod turn_recovery;
mod chat;
mod inbound_text;
mod closed_agent;
mod lifecycle;
mod process_identity;
mod lifecycle_cli;
mod watchdog;
mod task_runs;
mod tasks;
mod schedule;
mod backup;
mod restore;
mod console_tasks;
mod supervisor;
mod supervisor_dependencies;
mod dependency;
mod admin_alert;
mod run_record;
mod candidate_state;
mod recovery_log;
mod dsh_health;
mod health_check;
mod gateway_health;
mod dependency_watch;
mod snowluma_heal;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const CONFIG_PATH: &str = "C:/qqbot/qq-bridge/config.json";
const SOCIAL_PATH: &str = "C:/qqbot/qq-bridge/state/social-v2.json";
const DECISION_LOG: &str = "C:/qqbot/logs/rbridge-live.jsonl";
/// 状态机真正「醒」的时刻（含合批延迟），A/B 与 P6 验收都用这份对拍。
const WAKE_LOG: &str = "C:/qqbot/logs/rbridge-live-wakes.jsonl";

#[derive(Deserialize, Debug, Default)]
struct Config {
    #[serde(default)]
    snowluma: Option<SnowLuma>,
    #[serde(default)]
    ownerqq: Option<String>,
    #[serde(default)]
    allow: Option<Allow>,
    #[serde(rename = "socialV2", default)]
    social_v2: Option<SocialV2>,
}

#[derive(Deserialize, Debug, Default)]
struct SnowLuma {
    #[serde(rename = "wsUrl", default)]
    ws_url: Option<String>,
    #[serde(rename = "accessToken", default)]
    access_token: Option<String>,
    #[serde(rename = "httpUrl", default)]
    http_url: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct Allow {
    #[serde(default)]
    groups: Vec<Value>,
    #[serde(default)]
    private: Vec<Value>,
}
#[derive(Deserialize, Debug, Default, Clone)]
struct SocialV2 {
    #[serde(default)]
    wake: WakeTuning,
    #[serde(default)]
    social: SocialTuning,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct WakeTuning {
    #[serde(rename = "batchWindowMs", default)]
    batch_window_ms: f64,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct SocialTuning {
    #[serde(rename = "skipProbability", default)]
    skip_probability: f64,
}


#[derive(Deserialize, Debug, Default, Clone)]
struct Triggers {
    #[serde(rename = "atMention", default)]
    at_mention: bool,
    #[serde(rename = "nameMention", default)]
    name_mention: bool,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    question: bool,
    #[serde(default)]
    poke: bool,
    #[serde(rename = "speakerIds", default)]
    speaker_ids: Vec<Value>,
    #[serde(rename = "anyMessage", default)]
    any_message: bool,
    #[serde(default)]
    probability: f64,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct WakeConfig {
    #[serde(default)]
    triggers: Triggers,
}

#[derive(Deserialize, Debug, Default)]
struct Conversation {
    #[serde(rename = "wakeConfig", default)]
    wake_config: Option<WakeConfig>,
}

#[derive(Deserialize, Debug, Default)]
struct SocialState {
    #[serde(default)]
    conversations: HashMap<String, Conversation>,
}

#[derive(Deserialize, Debug, Default)]
struct Event {
    #[serde(default)]
    post_type: String,
    #[serde(default)]
    message_type: String,
    #[serde(default)]
    sub_type: String,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    group_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    user_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    target_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    sender_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    self_id: Option<i64>,
    #[serde(default)]
    raw_message: String,
    #[serde(default)]
    message_id: Option<Value>,
    #[serde(default)]
    msg_id: Option<Value>,
    #[serde(default)]
    message: Value,
    #[serde(default)]
    meta_event_type: String,
    /// A heartbeat's `status` (`online`, `good`): SnowLuma's own verdict on its receive chain.
    #[serde(default)]
    status: Option<Value>,
    #[serde(default)]
    echo: Option<Value>,
    #[serde(default)]
    data: Option<Value>,
    /// OneBot 的 sender 对象（card/nickname）。只用于 `speaker:<label>` 的唤醒原因，
    /// 与 JS 的 `event.sender.card || event.sender.nickname || userId` 对齐。
    #[serde(default)]
    sender: Option<SendingUser>,
    /// 录制时的时间戳（重放输入里带 `_at`），只用于对拍输出。
    #[serde(rename = "_at", default)]
    at_recorded: Option<i64>,
}


#[derive(Deserialize, Debug, Default, Clone)]
struct SendingUser {
    #[serde(default, deserialize_with = "deserialize_account_id")]
    user_id: Option<i64>,
    #[serde(default)]
    card: String,
    #[serde(default)]
    nickname: String,
}

// OneBot accepts JSON integers and decimal strings. Malformed present identities fail closed.
fn protocol_integer(value: &Value) -> Option<i64> {
    let id = match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }?;
    (-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&id).then_some(id)
}

fn deserialize_account_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    value.map(|v| protocol_integer(&v).filter(|id| *id > 0)
        .ok_or_else(|| serde::de::Error::custom("invalid OneBot account ID"))).transpose()
}

/// 稳定文本种子：同一段文本 + 同一发送者 → 同一个数（重放可复现）。
fn text_seed(text: &str, user: Option<i64>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    user.hash(&mut h);
    h.finish()
}

fn event_message_id(event: &Event) -> Option<String> {
    let value = event.message_id.as_ref().or(event.msg_id.as_ref())?;
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// `rollout.rustGroups` 里那些**没有**出现在 `--live-groups` 的会话。
///
/// 返回 `Some(missing)` 表示两边不一致（缺的列表非空）；`None` = 一致或未启用灰度。
/// 只在 `rollout.enabled === false` 之外生效 —— 明确关掉灰度时 `rustGroups` 内容无意义。
fn rollout_mismatch(cfg: &serde_json::Value, live_groups: &[String]) -> Option<Vec<String>> {
    let rollout = cfg.get("rollout")?;
    if rollout.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let groups: Vec<String> = rollout
        .get("rustGroups")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<String> = groups
        .into_iter()
        .filter(|g| !live_groups.iter().any(|l| l == g))
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

/// Wake configurations written to the state file by a tool or the console (`confirmedAt` changed) reach the
/// running state machine of the conversation. `synced` remembers the last `session::WakeSync` seen per conversation.
fn sync_wake_configs(
    context: &runtime::RuntimeContext,
    cfg: &Value,
    state: &Value,
    drivers: &mut HashMap<String, session::Driver>,
    synced: &mut HashMap<String, session::WakeSync>,
    now: i64,
) {
    // Finite sleeps must work after restart even before the first inbound event.
    // The live reserved2 hub owns general driver creation and turn lifecycle.
    let current_cfg = if context.is_isolated() { context.load_config().ok() } else { Some(cfg.clone()) };
    if let Some(cfg) = current_cfg.as_ref().filter(|cfg| {
        cfg.get("socialV2").and_then(|s| s.get("enabled")).and_then(Value::as_bool) != Some(false)
            && state.get("paused").and_then(Value::as_bool) != Some(true)
            && console::runtime_mode(context) == "reserved2"
    }) {
        if let Some(conversations) = state.get("conversations").and_then(Value::as_object) {
            for (key, conversation) in conversations {
                let finite = conversation.get("wakeConfig").is_some_and(|wc| {
                    wc.get("infinite").and_then(Value::as_bool) == Some(false)
                        && wc.get("sleepUntil").and_then(Value::as_str).and_then(wake::parse_iso_ms).is_some()
                });
                if finite && console::key_allowed(cfg, key) {
                    drivers.entry(key.clone()).or_insert_with(|| {
                        session::Driver::new(session::scheduler_from_state(wake_cfg_of(cfg), conversation), text_seed(key, None))
                    });
                }
            }
        }
    }
    for (key, driver) in drivers.iter_mut() {
        let Some(wc) = state.get("conversations").and_then(|c| c.get(key)).and_then(|c| c.get("wakeConfig")) else {
            continue;
        };
        if let Some(next) = session::WakeSync::changed(synced.get(key), wc) {
            let announce = synced.contains_key(key);
            driver.adopt_wake_config(session::wake_config_from_json(Some(wc)), now);
            synced.insert(key.clone(), next);
            if announce {
                println!("[her] {key} 唤醒配置已更新（confirmedBy {}）", wc.get("confirmedBy").and_then(|v| v.as_str()).unwrap_or("?"));
            }
        }
    }
}

fn now_ms() -> u128 {
    crate::parity_clock::now_ms() as u128
}

fn plain_text(msg: &Value, self_id: i64) -> (String, bool, Option<i64>) {
    let segs = match msg.as_array() {
        Some(a) => a.clone(),
        None => return (msg.as_str().unwrap_or("").to_string(), false, None),
    };
    let mut text = String::new();
    let mut at_self = false;
    let mut reply_id: Option<i64> = None;
    for seg in segs {
        let ty = seg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = seg.get("data").cloned().unwrap_or(json!({}));
        match ty {
            "text" => {
                if let Some(t) = data.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            "at" => {
                let qq = data.get("qq").map(|v| match v { Value::String(s) => s.clone(), _ => protocol_integer(v).filter(|id| *id > 0).map(|id| id.to_string()).unwrap_or_default() }).unwrap_or_default();
                if qq == self_id.to_string() {
                    at_self = true;
                } else {
                    text.push_str(&format!("@{qq}"));
                }
            }
            "reply" => {
                if reply_id.is_none() {
                    reply_id = data.get("id").and_then(protocol_integer);
                }
            }
            "record" => text.push_str("[语音]"),
            "image" | "face" | "mface" | "video" | "file" => text.push_str("[媒体]"),
            _ => {}
        }
    }
    (text, at_self, reply_id)
}

/// JS 正则里的 `\w`：**只有 ASCII 字母数字和下划线**。
///
/// 这里不能用 Rust 的 `char::is_alphanumeric()` —— 它是 Unicode 语义，汉字算 alphanumeric。
/// 后果很具体：JS 里 `\bds\b` 能匹配「ds好」（汉字不是 `\w`，所以 s 后面有边界），
/// Rust 用 Unicode 判断会认定「s 与好之间没边界」→ **短英文关键词后面紧跟中文时漏判**。
fn js_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 录下来的文本里，引用是 `[引用 某人：原文]` 这种内联形式。
/// 判断「被引用的是不是我自己」用昵称/群名片比对，替代线上那次「查发送者」。
fn quote_target_is_self(body: &str, self_nick: &str, self_cards: &HashMap<i64, String>) -> bool {
    let Some(rest) = body.trim_start().strip_prefix("[引用 ") else {
        return false;
    };
    let Some((who, _)) = rest.split_once('：') else {
        return false;
    };
    let who = who.trim();
    if who.is_empty() {
        return false;
    }
    if !self_nick.is_empty() && who == self_nick {
        return true;
    }
    self_cards.values().any(|c| !c.is_empty() && c.trim() == who)
}
fn keyword_hit(text_lower: &str, kw: &str) -> bool {
    let k = kw.to_lowercase();
    if k.is_empty() {
        return false;
    }
    // 与 JS 一致：纯 ASCII 且长度 ≤4 的关键词走词边界匹配，避免 ADS/BDSM 误触发
    if !(k.is_ascii() && k.len() <= 4) {
        return text_lower.contains(&k);
    }
    let chars: Vec<char> = text_lower.chars().collect();
    let kchars: Vec<char> = k.chars().collect();
    let klen = kchars.len();
    if klen > chars.len() {
        return false;
    }
    for i in 0..=chars.len() - klen {
        if chars[i..i + klen] != kchars[..] {
            continue;
        }
        let before_ok = i == 0 || !js_word_char(chars[i - 1]);
        let after_ok = i + klen >= chars.len() || !js_word_char(chars[i + klen]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Her current nickname, or in a group her group card, appears in the lowercased text.
pub(crate) fn names_self(ev: &Event, lower: &str, self_nick: &str, self_cards: &HashMap<i64, String>) -> bool {
    let is_group = ev.message_type == "group" || ev.group_id.is_some();
    let mut aliases: Vec<String> = Vec::new();
    if !self_nick.is_empty() {
        aliases.push(self_nick.to_lowercase());
    }
    if is_group {
        if let Some(card) = ev.group_id.and_then(|g| self_cards.get(&g)) {
            if !card.is_empty() {
                aliases.push(card.to_lowercase());
            }
        }
    }
    aliases.iter().any(|a| lower.contains(a))
}

#[allow(clippy::too_many_arguments)]
fn decide(
    ev: &Event,
    text: &str,
    at_self: bool,
    self_nick: &str,
    self_cards: &HashMap<i64, String>,
    tr: &Triggers,
    question_directed: bool,
) -> (bool, String) {
    let lower = text.to_lowercase();
    // 群/私聊的判据不能只看 message_type：**拍一拍这类 notice 事件里 message_type 是空的**，
    // 只有 group_id。只看 message_type 的话，群里的拍一拍会被当成"私聊的非消息事件"，
    // 在下面那条 `post_type != "message"` 就返回 no-trigger，永远走不到 poke 分支。
    // 实测漏判过一条：`[07:59:15] group:7003 拍一拍事件：MR1FC 戳了戳 7001`，
    // JS 醒了（reason=poke），Rust 判 no-trigger —— 红线就是这么被破掉的。
    let is_group = ev.message_type == "group" || ev.group_id.is_some();

    // 私聊一律唤醒，而且原因就是 `private`（JS 的 evaluateWakeTriggerV2 第一行）。
    // 原来 Rust 把它放在**最后**、还分了 `private:owner`，后果有两个：
    //   1. 私聊里命中关键词/名字时，原因串变成了 keyword:xxx，和 JS 不一样；
    //   2. `private:owner` 这个后缀 JS 根本没有，A/B 匹配键也对不上。
    // 另外必须挡掉 notice/meta 这类空正文事件 —— 它们也会走到这里。
    // 拍一拍：**群聊和私聊都算**，而且要排在"私聊/群聊分叉"之前 ——
    // notice 事件没有 message_type，先分叉的话永远走不到这里（红线漏判就是这么来的）。
    // 与 JS handlePokeNotice 对齐：判据是 sub_type === "poke"；自己拍出去的不算；
    // 唤醒原因统一是 `poke`（不带 self/other 后缀，prompt 的 tail() 按精确串匹配）。
    if ev.post_type == "notice" && ev.sub_type == "poke" {
        if !tr.poke {
            return (false, "no-trigger".into());
        }
        let self_id = ev.self_id.unwrap_or(0);
        let sender = ev.sender_id.or(ev.user_id).unwrap_or(0);
        if sender != 0 && sender == self_id {
            return (false, "poke:selfskip".into());
        }
        return (true, "poke".into());
    }

    if !is_group {
        if ev.post_type != "message" {
            return (false, "no-trigger".into());
        }
        return (true, "private".into());
    }
    // JS 的 notice 事件根本不会进 evaluateWakeTriggerV2 ——
    // 它只走 handlePokeNotice 那条路（要么唤醒 poke，要么什么都不做）。
    // 之前 Rust 把 notice 也丢进后面的触发器判断，于是「配置了指定成员」的会话
    // 会因为一条拍一拍通知而按 speaker 唤醒（JS 不会）。
    if ev.post_type != "message" {
        if ev.post_type == "notice" && ev.sub_type == "poke" && tr.poke {
            let self_id = ev.self_id.unwrap_or(0);
            let sender = ev.sender_id.or(ev.user_id).unwrap_or(0);
            if sender != 0 && sender == self_id {
                return (false, "poke:selfskip".into());
            }
            return (true, "poke".into());
        }
        return (false, "no-trigger".into());
    }
    if tr.any_message {
        return (true, "anyMessage".into());
    }

    if tr.poke {
        // 与 JS handlePokeNotice 对齐：判据是 sub_type === "poke"（不是 notice_type），
        // 且自己拍别人不算（否则机器人会回应自己的动作）。
        if ev.post_type == "notice" && ev.sub_type == "poke" {
            let self_id = ev.self_id.unwrap_or(0);
            let sender = ev.sender_id.or(ev.user_id).unwrap_or(0);
            if sender != 0 && sender == self_id {
                return (false, "poke:selfskip".into());
            }
            // JS 的唤醒原因统一是 `poke`（scheduleWakeV2(key, 'poke')），不区分拍谁；
            // 之前 Rust 用了 poke:self / poke:other，后果不只是报表对不上 ——
            // prompt 的 `tail()` 是按 string 精确匹配 `poke` 的，带后缀会掉进通用分支，
            // 拍一拍的专属提示词就整段丢了。这里对齐成同一个字符串。
            let _ = ev.target_id;
            return (true, "poke".into());
        }
    }
    if tr.at_mention && at_self {
        return (true, "atMention".into());
    }
    if tr.name_mention && names_self(ev, &lower, self_nick, self_cards) {
        return (true, "nameMention".into());
    }
    for kw in &tr.keywords {
        if keyword_hit(&lower, kw) {
            return (true, format!("keyword:{kw}"));
        }
    }
    // Direction is established from conversation evidence before this decision.
    if tr.question && question_directed {
        return (true, "question".into());
    }
    if !tr.speaker_ids.is_empty() {
        let uid = ev.user_id.unwrap_or(0).to_string();
        if tr.speaker_ids.iter().any(|v| v.to_string().trim_matches('"') == uid) {
            // 与 JS 一致：`speaker:<群名片或昵称或QQ号>`，不是干巴巴一个 speakerId。
            // 这个字符串会进提示词的「原因：…」那一行，也会进 A/B 的匹配键。
            let label = ev
                .sender
                .as_ref()
                .map(|s| {
                    if !s.card.trim().is_empty() {
                        s.card.trim().to_string()
                    } else if !s.nickname.trim().is_empty() {
                        s.nickname.trim().to_string()
                    } else {
                        uid.clone()
                    }
                })
                .unwrap_or_else(|| uid.clone());
            return (true, format!("speaker:{label}"));
        }
    }
    if tr.probability > 0.0 {
        // 用「事件内容 + 时间」哈希做随机，而不是 now_ms() % N：
        // 时钟在同一秒内几乎不变，会让概率触发成簇（离线重放时实测 58% 全命中，
        // 配置是 5%）。哈希保证同一批输入下分布均匀。
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // 只用「内容 + 发送者」做种子：同一批输入重放结果可复现（回归测试需要）。
        // 不同消息的文本不同，等价于每条独立抽签；同一句话重复出现时结果一致，
        // 这在群里是可接受的（真要每次都不同，得引入 rand 依赖）。
        text.hash(&mut h);
        ev.user_id.hash(&mut h);
        let r = (h.finish() % 10_000) as f64 / 10_000.0;
        if r < tr.probability {
            return (true, "probability".into());
        }
    }

    (false, "no-trigger".into())
}

fn discover_ws_token() -> Option<String> {
    let dir = std::path::Path::new("C:/qqbot/SnowLuma/config");
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("onebot_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else { continue };
        let tok = v
            .get("networks")
            .and_then(|n| n.get("wsServers"))
            .and_then(|w| w.get(0))
            .and_then(|s| s.get("accessToken"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string());
        if tok.is_some() {
            println!("[her] ws token from {name}");
            return tok;
        }
    }
    None
}

fn is_loopback_ws_url(url: &str) -> bool {
    let Some(authority_and_path) = url.strip_prefix("ws://") else {
        return false;
    };
    let authority = authority_and_path.split('/').next().unwrap_or("");
    if authority.contains('@') || authority.is_empty() {
        return false;
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map(|(host, _)| host).unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Extract the authority from the explicit OneBot HTTP dependency. Isolated runtimes only accept
/// `http://<loopback-ip>:<non-zero-port>` endpoints; keep brackets around IPv6 so both the socket
/// address and HTTP Host header remain valid.
fn loopback_http_endpoint(url: &str) -> Option<(String, u16)> {
    let authority = url.strip_prefix("http://")?;
    if authority.is_empty() || authority.chars().any(|ch| matches!(ch, '/' | '?' | '#' | '@')) {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, rest) = bracketed.split_once(']')?;
        let port = rest.strip_prefix(':')?;
        (host, port)
    } else {
        authority.rsplit_once(':')?
    };
    let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
    let address = host.parse::<std::net::IpAddr>().ok()?;
    if !address.is_loopback() {
        return None;
    }
    let host = if address.is_ipv6() {
        format!("[{address}]")
    } else {
        address.to_string()
    };
    Some((host, port))
}

fn parse_live_groups(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|groups| groups.split(','))
        .map(str::trim)
        .filter(|group| !group.is_empty())
        .map(|group| {
            if group.bytes().all(|byte| byte.is_ascii_digit()) {
                format!("group:{group}")
            } else {
                group.to_string()
            }
        })
        .collect()
}

fn read_json(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

fn append_runtime_json_line(
    context: &runtime::RuntimeContext,
    relative: &str,
    legacy_path: &str,
    record: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let safe = activity::mask_json_session_tokens(record, &activity::known_session_tokens(context));
    let mut line = serde_json::to_vec(&safe)?;
    line.push(b'\n');
    if context.is_isolated() {
        context
            .append_bytes(runtime::RuntimeArea::Logs, relative, &line)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
    } else {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(legacy_path)?;
        file.write_all(&line)?;
    }
    Ok(())
}

fn runtime_json(
    context: &runtime::RuntimeContext,
    area: runtime::RuntimeArea,
    relative: &str,
    legacy_path: &str,
) -> Option<Value> {
    let text = if context.is_isolated() {
        context.read_text(area, relative).ok()?
    } else {
        std::fs::read_to_string(legacy_path).ok()?
    };
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// 从 config.json 提取唤醒调度参数（在线主循环与离线重放共用一份口径）。
fn wake_cfg_of(cfg_raw: &Value) -> wake::WakeCfg {
    let sv2 = cfg_raw.get("socialV2").cloned().unwrap_or_default();
    let mut wcfg = wake::WakeCfg::default();
    wcfg.turn_hard_cap_ms = wake::turn_budget(cfg_raw).hard_cap_ms;
    wcfg.turn_ceiling_ms = wake::turn_ceiling_ms(cfg_raw);
    wcfg.enabled = sv2.get("enabled").and_then(Value::as_bool) != Some(false);
    wcfg.reply_wake_quiet_ms = Some(sv2.pointer("/reply/wakeQuietMs").and_then(Value::as_i64)
        .filter(|ms| (1..=30_000).contains(ms)).unwrap_or(3000));
    if let Some(ms) = sv2.pointer("/wake/preemptAfterMs").and_then(Value::as_i64) { wcfg.preempt_after_ms = ms.max(0); }
    if let Some(n) = sv2
        .get("wake")
        .and_then(|w| w.get("batchWindowMs"))
        .and_then(|v| v.as_i64())
    {
        wcfg.batch_window_ms = n;
    }
    if let Some(n) = sv2.get("autoReplyCheckMs").and_then(|v| v.as_i64()) {
        wcfg.auto_reply_check_ms = n;
    }
    // 限频参数：JS 默认 1/分钟、12/小时；填 0 表示不限。
    if let Some(n) = sv2
        .get("wake")
        .and_then(|w| w.get("maxWakePerMinute"))
        .and_then(|v| v.as_i64())
    {
        wcfg.max_wake_per_minute = n;
    } else {
        wcfg.max_wake_per_minute = 1;
    }
    if let Some(n) = sv2
        .get("wake")
        .and_then(|w| w.get("maxWakePerHour"))
        .and_then(|v| v.as_i64())
    {
        wcfg.max_wake_per_hour = n;
    } else {
        wcfg.max_wake_per_hour = 12;
    }
    let proactive = sv2.get("proactive").cloned().unwrap_or_default();
    wcfg.proactive_enabled = proactive
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if let Some(n) = proactive.get("checkIntervalMinMs").and_then(|v| v.as_i64()) {
        wcfg.proactive_min_ms = n;
    }
    if let Some(n) = proactive.get("checkIntervalMaxMs").and_then(|v| v.as_i64()) {
        wcfg.proactive_max_ms = n;
    }
    if let Some(n) = proactive.get("idleThresholdMs").and_then(|v| v.as_i64()) {
        wcfg.proactive_idle_ms = n;
    }
    if let Some(p) = proactive.get("probability").and_then(|v| v.as_f64()) {
        wcfg.proactive_probability = p;
    }
    wcfg
}
/// The OS-held owner gate, not the age or contents of a lock file, decides ownership.
fn acquire_single_instance(
    context: &runtime::RuntimeContext,
) -> Option<lifecycle::OwnerGuard> {
    let relative = "logs/rbridge-live.lock";
    let path = if context.is_isolated() {
        context.resolve_path(runtime::RuntimeArea::Data, &relative, runtime::PathIntent::WriteFile)
            .map_err(|e| println!("[her] owner path unavailable: {e}")).ok()?
    } else {
        std::path::PathBuf::from(format!("C:/qqbot/{relative}"))
    };
    let identity = lifecycle::current_identity().map_err(|e| println!("[her] identity unavailable: {e}")).ok()?;
    let groups = {
        let args: Vec<String> = std::env::args().collect();
        Some(parse_live_groups(args.iter().position(|a| a == "--live-groups").and_then(|i| args.get(i + 1)).map(String::as_str)))
    };
    match lifecycle::OwnerGate::new(path).acquire("live", identity, groups, now_ms() as i64, std::time::Duration::from_millis(500)) {
        lifecycle::Acquire::Acquired(owner) => Some(owner),
        other => { println!("[her] owner unavailable: {other:?}"); None }
    }
}

/// Initialize the live pipeline before publishing a heartbeat. In isolated mode, a missing or
/// corrupt candidate session map, selected DSH credential, or dependency is a startup failure;
/// the process must not claim that it has taken over a session when it cannot answer it. Legacy
/// mode retains its historical behavior of logging the failure and remaining in observation mode.
fn prepare_live_pipeline(
    context: &runtime::RuntimeContext,
    cfg: &Value,
    live_mode: bool,
    live_groups: &[String],
    live_all: bool,
) -> Result<Option<pipeline::Pipeline>, String> {
    if !live_mode || (live_groups.is_empty() && !live_all) {
        return Ok(None);
    }
    match pipeline::Pipeline::new_with_runtime(context, cfg, true) {
        Ok(pipeline) => {
            println!("[her] LIVE 模式：会真的建会话并发送 QQ 消息");
            Ok(Some(pipeline))
        }
        Err(error) if context.is_isolated() => Err(format!(
            "isolated live pipeline initialization failed; refusing to publish a live heartbeat: {error}"
        )),
        Err(error) => {
            println!("[her] ⚠️ 无法初始化 pipeline（退回只判定）：{error}");
            Ok(None)
        }
    }
}

/// Feed one group message to the slang window and, when due, start an extraction in the background.
fn feed_slang(learning: &slang_learn::SlangLearning, auto: bool, cfg_raw: &Value, key: &str, sender: &str, body: &str) {
    let feed = learning.try_feed_window(key, sender, body);
    if let Err(error) = &feed {
        crate::safe_println!("[slang] 候选黑话窗口读写失败：{error}");
    }
    let trigger = matches!(feed, Ok(true))
        && auto
        && match learning.try_maybe_trigger(cfg_raw, key) {
            Ok(trigger) => trigger,
            Err(error) => {
                crate::safe_println!("[slang] 候选黑话窗口读取失败：{error}");
                false
            }
        };
    if trigger {
        println!("[slang] 触发黑话抽取：{key}");
        if let Err(error) = learning.spawn_auto(cfg_raw.clone(), key.to_string()) {
            crate::safe_println!("[slang] 无法排队抽取：{error}");
        }
    }
}

/// 极简 HTTP/1.1 POST（只用于本机 OneBot HTTP API，不需要 TLS）。
fn http_post_json(host: &str, port: u16, path: &str, token: &str, body: &str) -> Option<String> {
    use std::io::{Read, Write};
    let addr = format!("{host}:{port}");
    let mut s = std::net::TcpStream::connect(addr).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(8))).ok()?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    let (_, body) = raw.split_once("\r\n\r\n")?;
    Some(body.to_string())
}

/// 查某条消息是不是自己发的（引用回复的判定）。JS 用 resolveReplyInfo 做同样的事。
fn fetch_msg_sender_is_self(host: &str, port: u16, token: &str, msg_id: i64, self_id: i64) -> bool {
    let body = format!(r#"{{"message_id":{msg_id}}}"#);
    let Some(text) = http_post_json(host, port, "/get_msg", token, &body) else {
        return false;
    };
    let Ok((v, _)) = aux_text::parse_json_replacing_lone_surrogates::<serde_json::Value>(&text) else {
        return false;
    };
    let sender = v
        .get("data")
        .and_then(|d| d.get("sender"))
        .and_then(|s| s.get("user_id"))
        .and_then(protocol_integer).filter(|id| *id > 0)
        .or_else(|| v.get("data").and_then(|d| d.get("user_id")).and_then(protocol_integer).filter(|id| *id > 0));
    sender == Some(self_id)
}

/// 查某个群里自己的群名片。
fn fetch_group_card(host: &str, port: u16, token: &str, gid: i64, self_id: i64) -> Option<String> {
    let body = format!(r#"{{"group_id":{gid},"user_id":{self_id},"no_cache":false}}"#);
    let text = http_post_json(host, port, "/get_group_member_info", token, &body)?;
    let (v, _) = aux_text::parse_json_replacing_lone_surrogates::<serde_json::Value>(&text).ok()?;
    v.get("data")?
        .get("card")?
        .as_str()
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
}


/// DSH 基址：优先环境变量，其次 bridge 的 config.json。
fn dsh_probe_base() -> String {
    if let Ok(v) = std::env::var("DSH_BASE_URL") {
        return v;
    }
    read_json("C:/qqbot/qq-bridge/config.json")
        .and_then(|v| v.get("dsh").and_then(|d| d.get("baseUrl")).and_then(|b| b.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "http://127.0.0.1:3080".to_string())
}



/// 解析 `private:123` / `group:456` 形式的发送目标。
fn parse_target(spec: &str, cfg: &serde_json::Value) -> Result<onebot::Target, Box<dyn std::error::Error>> {
    if let Some(id) = spec.strip_prefix("private:") {
        return Ok(onebot::Target::Private(id.parse()?));
    }
    if let Some(id) = spec.strip_prefix("group:") {
        return Ok(onebot::Target::Group(id.parse()?));
    }
    let owner = cfg.get("ownerQQ").and_then(|o| o.as_i64()).unwrap_or(0);
    if owner == 0 {
        return Err("target 格式错误且读不到 ownerQQ".into());
    }
    Ok(onebot::Target::Private(owner))
}



fn main() -> Result<(), Box<dyn std::error::Error>> {
    subprocess::prevent_stdio_inheritance();
    let mut args: Vec<String> = std::env::args().collect();
    let install = match install_root::InstallRoot::select(&mut args) {
        Ok(install) => install,
        Err(error) => { eprintln!("[her] {error}"); std::process::exit(2); }
    };
    // Set once, before Tokio starts threads, so selected children keep the same production boundary.
    std::env::set_var("HER_INSTALL_ROOT", subprocess::external_path(install.root())?);
    if args.get(1).map(String::as_str) == Some("install-root") {
        if args.get(2).map(String::as_str) == Some("layout") && args.len() == 3 {
            let layout = install.layout().into_iter().map(|(key, path)| {
                subprocess::external_path(path).map(|value| (key.to_string(), serde_json::Value::String(value)))
            }).collect::<Result<serde_json::Map<String, Value>, String>>()?;
            println!("{}", Value::Object(layout));
            return Ok(());
        }
        if args.get(2).map(String::as_str) == Some("check-development") && args.len() == 5
            && args[3] == "--runtime-root" {
            if let Err(error) = install.check_development(std::path::Path::new(&args[4])) {
                eprintln!("[her] {error}"); std::process::exit(2);
            }
            println!("{{\"ok\":true}}");
            return Ok(());
        }
        if args.get(2).map(String::as_str) == Some("check-supervisor") && args.len() == 7
            && args[3] == "--runtime-root" && args[5] == "--executable" {
            let check = || -> Result<(), String> {
                let image = std::fs::canonicalize(&args[6]).map_err(|_| "resident executable is unavailable")?;
                if !image.is_file() { return Err("resident executable is unavailable".into()); }
                install.check_runtime(std::path::Path::new(&args[4]), &image, true)
            };
            if let Err(error) = check() { eprintln!("[her] {error}"); std::process::exit(2); }
            println!("{{\"ok\":true}}");
            return Ok(());
        }
        eprintln!("usage: her-cli install-root layout [--install-root <absolute path>]\n       her-cli install-root check-development --runtime-root <absolute path>\n       her-cli install-root check-supervisor --runtime-root <absolute path> --executable <resident image>");
        std::process::exit(2);
    }
    run_entry(args)
}

#[tokio::main]
async fn run_entry(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if args.get(1).map(String::as_str) == Some("release") {
        std::process::exit(release::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("install") {
        std::process::exit(scheduled_task::install_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("scheduled-task") {
        std::process::exit(scheduled_task::run_cli(&args[2..]));
    }
    if args.len() == 1 || args.get(1).map(String::as_str) == Some("app") {
        std::process::exit(desktop::run_cli(args.get(2..).unwrap_or(&[])));
    }
    if args.get(1).map(String::as_str) == Some("dsh-mcp-config") {
        std::process::exit(dsh_mcp_config::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("report") {
        std::process::exit(report::run_cli(&args[2..]));
    }
    // Ticket 27: desired state and task history of a selected root (explicit root, works with a broken config).
    if args.get(1).map(String::as_str) == Some("process") {
        std::process::exit(process_identity::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("backup") {
        std::process::exit(restore::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("lifecycle") {
        std::process::exit(lifecycle_cli::run_lifecycle(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("stop") {
        std::process::exit(lifecycle_cli::run_lifecycle(&args[1..]));
    }
    if args.get(1).map(String::as_str) == Some("tasks") {
        std::process::exit(lifecycle_cli::run_tasks(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("supervise") {
        std::process::exit(supervisor::run_cli(&args[2..]));
    }
    if args.get(1).is_some_and(|arg| !arg.starts_with("--")) {
        eprintln!("[her] unknown command");
        std::process::exit(2);
    }
    // Removed startup options must not fall through to implicit runtime paths.
    if args.iter().skip(1).any(|arg| matches!(arg.as_str(),
        "--ab-full" | "--ab-probe" | "--shadow-turn" | "--shadow-state-root"
        | "--replay" | "--wake-replay" | "--replay-jsonl" | "--replay-triggers"
    )) {
        eprintln!("[her] unknown option");
        std::process::exit(2);
    }
    // These diagnostic/maintenance modes use legacy paths rather than a selected runtime.
    // They remain development-only even when launched through an installed image.
    let development_mode = args.iter().skip(1).any(|arg| matches!(arg.as_str(),
        "--send-probe" | "--ask" | "--memory-probe" | "--memory-write-probe"
        | "--hot-render" | "--index-hint" | "--price-probe" | "--role-probe"
        | "--prompt-wrapped" | "--rpc-probe" | "--slang-parse-test" | "--selftest-401"
        | "--failover-probe" | "--sticker-sync" | "--build-prompt" | "--dsh-dump"
        | "--dsh-watch" | "--dsh-probe" | "--dsh-demo" | "--question-probe"
        | "--split-probe" | "--sensitive-probe" | "--backend-ask" | "--backends"
    ));
    if development_mode {
        let legacy = runtime::RuntimeContext::legacy();
        let db = std::env::var("HER_MEMORY_DB").unwrap_or_else(|_| memory::DB_PATH.into());
        let runtime_db = memory::db_path_for_runtime(&legacy)?;
        for path in [legacy.config_file(), legacy.data_dir(), legacy.resources_dir(), std::path::Path::new(&db), runtime_db.as_path()] {
            if let Err(error) = install_root::check_development_path(path) {
                eprintln!("[her] {error}"); std::process::exit(2);
            }
        }
    }
    let runtime_context = match runtime::RuntimeContext::from_process_args(&args) {
        Ok(context) => context,
        Err(message) => {
            eprintln!("[her] {message}");
            std::process::exit(2);
        }
    };
    activity::set_log_context(&runtime_context);
    // P2-A 探针：--dsh-probe 只验鉴权；--dsh-demo 建会话并投 prompt
    // P2-B 探针：发一条私聊测试消息，验证 OneBot 发送链路
    if std::env::args().any(|a| a == "--send-probe") {
        let cfgv = read_json("C:/qqbot/qq-bridge/config.json").unwrap_or_default();
        let http_url = cfgv
            .get("snowluma").and_then(|s| s.get("httpUrl")).and_then(|u| u.as_str())
            .unwrap_or("http://127.0.0.1:3000").to_string();
        let token = cfgv
            .get("snowluma").and_then(|s| s.get("accessToken")).and_then(|t| t.as_str())
            .unwrap_or("").to_string();
        let owner: i64 = cfgv.get("ownerQQ").and_then(|o| o.as_i64()).unwrap_or(0);
        if owner == 0 { println!("[send] ❌ 读不到 ownerQQ"); return Ok(()); }
        // 可选参数：group:123 / private:456，默认给 owner 发私聊
        let target = match std::env::args().position(|a| a == "--send-probe").and_then(|i| std::env::args().nth(i + 1)) {
            Some(s) if s.starts_with("group:") => {
                onebot::Target::Group(s.trim_start_matches("group:").parse().unwrap_or(0))
            }
            Some(s) if s.starts_with("private:") => {
                onebot::Target::Private(s.trim_start_matches("private:").parse().unwrap_or(owner))
            }
            _ => onebot::Target::Private(owner),
        };
        let ob = onebot::OneBot::new(&http_url, token);
        let tune = onebot::SendTuning::from_config(&cfgv);
        let mut limiter = onebot::RateLimiter::new(
            onebot::SendTuning::max_per_minute(&cfgv),
            onebot::SendTuning::max_per_hour(&cfgv),
        );
        // 故意写长一点，验证分条与条间延迟
        let text = format!(
            "Rust 发送链路测试 {}。这条消息用来验证分条、CQ 转义和条间延迟；收到请忽略。",
            now_ms() % 100000
        );
        match ob.send_text(&target, &text, &tune, &mut limiter) {
            Ok(list) => println!("[send] ✅ 发送成功 {} 条：{list:?}", list.len()),
            Err(e) => println!("[send] ❌ {e}"),
        }
        return Ok(());
    }

    // P2-C 探针：完整一轮（投 prompt -> 等回复 -> 可选发送）
    //   --ask private:7001 "你好"            默认 dry-run，只打印回复
    //   --ask private:7001 "你好" --live     真的发出去
    if let Some(pos) = std::env::args().position(|a| a == "--ask") {
        let target_arg = std::env::args().nth(pos + 1).ok_or("--ask needs target")?;
        let text = std::env::args().nth(pos + 2).ok_or("--ask needs text")?;
        let live = std::env::args().any(|a| a == "--live");
        let cfgv = read_json("C:/qqbot/qq-bridge/config.json").unwrap_or_default();
        let target = parse_target(&target_arg, &cfgv)?;
        match pipeline::Pipeline::new(&cfgv, live) {
            Ok(mut p) => {
                println!("[ask] target={} live={}", target.key(), live);
                // 走**带 failover** 的那条：诊断/探针也应该是生产路径，
                // 否则"探针能通"不代表"主路径能通"（候选链、失败分类、重投都没被走到）。
                let result = p.handle_with_failover(&target, &text, &cfgv).await;
                match result {
                    Ok(Some(reply)) => println!("[ask] 回复：{reply}"),
                    Ok(None) => println!("[ask] 无回复"),
                    Err(e) => println!("[ask] ❌ {e}"),
                }
            }
            Err(e) => println!("[ask] ❌ 初始化失败：{e}"),
        }
        return Ok(());
    }

    // P3 探针：直接在 Rust 里查记忆库，结果与 JS 对拍
    if std::env::args().any(|a| a == "--memory-probe") {
        let reader = std::env::args()
            .position(|a| a == "--memory-probe")
            .and_then(|i| std::env::args().nth(i + 1))
            .unwrap_or_else(|| "private:7001".to_string());
        let query = std::env::args()
            .position(|a| a == "--query")
            .and_then(|i| std::env::args().nth(i + 1))
            .unwrap_or_else(|| "管理员偏好".to_string());
        let is_owner = reader == "private:7001";
        let json_mode = std::env::args().any(|a| a == "--json");
        let db_path = std::env::var("HER_MEMORY_DB").unwrap_or_else(|_| memory::DB_PATH.to_string());
        let conn = rusqlite::Connection::open(&db_path)?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))?;
        if !json_mode {
            println!("[memory] 库 {} 条，reader={reader} isOwner={is_owner}", total);
        }
        let scope = memory::resolve_reader_scope(&conn, &reader, is_owner)?;
        if !json_mode {
            println!("[memory] 可见范围 allowAll={} ids={:?}", scope.allow_all, scope.ids);
        }
        let hits = memory::search(&conn, &reader, is_owner, &query, &[], 5)?;
        if std::env::args().any(|a| a == "--json") {
            // 给对拍脚本用：只输出纯 JSON，不要别的行
            let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
            let payload = json!({
                "reader": reader,
                "isOwner": is_owner,
                "query": query,
                "total": total,
                "allowAll": scope.allow_all,
                "ids": ids,
                "hits": hits,
            });
            println!("{}", serde_json::to_string(&payload)?);
            return Ok(());
        }
        println!("[memory] 查询 {query:?} 命中 {} 条", hits.len());
        for h in &hits {
            println!("  #{} [{}·{}] {:.3} {}", h.id, h.scope, h.kind, h.score, activity::preview_chars(&h.content, 40));
        }
        return Ok(());
    }

    // P3 写路径验证：upsert 去重 + 检索命中（默认用临时副本，不动真库）
    if std::env::args().any(|a| a == "--memory-write-probe") {
        let content = std::env::args()
            .position(|a| a == "--content")
            .and_then(|i| std::env::args().nth(i + 1))
            .unwrap_or_else(|| "Rust 写入验证：这条由 her 写入".to_string());
        let db_path = std::env::var("HER_MEMORY_DB").unwrap_or_else(|_| memory::DB_PATH.to_string());
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        let mk = |imp: f64| memory::MemoryInput {
            owner_type: "session",
            owner_id: "private:7001",
            scope: "private",
            kind: "fact",
            subject: Some("rust-write-probe"),
            content: &content,
            tags: None,
            source: "rust-probe",
            importance: imp,
        };
        let first = memory::upsert_memory(&conn, &mk(0.6))?;
        let second = memory::upsert_memory(&conn, &mk(0.7))?;
        let hits = memory::search(&conn, "private:7001", true, &content, &[], 5)?;
        println!("{}", serde_json::to_string(&json!({
            "db": db_path,
            "first": { "id": first.id, "deduped": first.deduped },
            "second": { "id": second.id, "deduped": second.deduped },
            "hitIds": hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        }))?);
        return Ok(());
    }

    // P3 热缓存/索引提示探针：--hot-render 输出渲染结果，--index-hint 输出词表
    if std::env::args().any(|a| a == "--hot-render" || a == "--index-hint") {
        let db_path = std::env::var("HER_MEMORY_DB").unwrap_or_else(|_| memory::DB_PATH.to_string());
        let conn = rusqlite::Connection::open(&db_path)?;
        if std::env::args().any(|a| a == "--hot-render") {
            let items = memory::pick_hot(&conn, 40)?;
            let (text, count, chars) = memory::render_hot(&items);
            println!("{}", serde_json::to_string(&json!({
                "candidates": items.len(),
                "count": count,
                "chars": chars,
                "text": text,
            }))?);
        } else {
            let reader = std::env::args()
                .position(|a| a == "--index-hint")
                .and_then(|i| std::env::args().nth(i + 1))
                .unwrap_or_else(|| "private:7001".to_string());
            let is_owner = reader == "private:7001";
            let (total, terms) = memory::index_hint(&conn, &reader, is_owner, &[], 12)?;
            println!("{}", serde_json::to_string(&json!({
                "reader": reader, "total": total, "terms": terms,
            }))?);
        }
        return Ok(());
    }

    // P4：MCP 服务器（stdio）
    if std::env::args().any(|a| a == "--mcp-serve") {
        if runtime_context.is_isolated() {
            return Ok(mcp::serve_with_context(&runtime_context)?);
        }
        return Ok(mcp::serve()?);
    }

    // Ticket 26: the auxiliary MCP servers DSH launches next to `--mcp-serve`.
    if std::env::args().any(|a| a == "--mcp-web-search-serve") {
        return Ok(aux_mcp::serve_web_search(&runtime_context)?);
    }
    if std::env::args().any(|a| a == "--mcp-host-serve") {
        return Ok(aux_mcp::serve_host(&runtime_context)?);
    }

    // R3：计费对拍探针 —— 给定 model/时间/桶，输出单价与金额，供 JS 侧逐字段比较
    if std::env::args().any(|a| a == "--price-probe") {
        let cfg = read_json(CONFIG_PATH).unwrap_or_default();
        let table = pricing::resolve(cfg.get("pricing"));
        // 用例从 stdin 读（JSON 数组）：对拍脚本要能自由换时间段，
        // 尤其是**高峰**和**节假日** —— 写死一串「全是空闲时段」的用例，
        // 峰谷逻辑等于没测。
        let mut raw_in = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw_in);
        let cases: Vec<serde_json::Value> = serde_json::from_str(&raw_in).unwrap_or_default();
        let mut out = Vec::new();
        for case in &cases {
            let model = case.get("model").and_then(|v| v.as_str()).unwrap_or("");
            let at = case.get("at").and_then(|v| v.as_i64()).unwrap_or(0);
            let b = case.get("b").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let g = |i: usize| b.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let buckets = pricing::Buckets {
                cache_miss: g(0), cache_hit: g(1), cache_write: g(2), output: g(3), reasoning: 0.0,
            };
            let (rates, cost) = pricing::price_usage(&buckets, model, at, &table);
            out.push(json!({
                "model": model, "at": at,
                "tier": rates.tier, "estimated": rates.estimated, "label": rates.label,
                "cacheHit": rates.cache_hit, "cacheMiss": rates.cache_miss,
                "cacheWrite": rates.cache_write, "output": rates.output,
                "cost": { "cacheHit": cost.cache_hit, "cacheMiss": cost.cache_miss,
                          "cacheWrite": cost.cache_write, "output": cost.output, "total": cost.total }
            }));
        }
        println!("{}", serde_json::to_string(&out).unwrap());
        return Ok(());
    }
    // 人格卡解析探针：给定若干卡片文本，输出解析、统计与告警。
    if std::env::args().any(|a| a == "--role-probe") {
        let cfg = read_json(CONFIG_PATH).unwrap_or_default();
        let inject_max = role::apply_inject_limit(cfg.get("role").and_then(|r| r.get("maxInjectChars")));
        let mut raw_in = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw_in);
        let cases: Vec<serde_json::Value> = serde_json::from_str(&raw_in).unwrap_or_default();
        let mut out = Vec::new();
        for case in &cases {
            let text = case.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let sections: Vec<serde_json::Value> = role::parse_role_sections(text)
                .into_iter()
                .map(|s| {
                    json!({
                        "mode": s.mode,
                        "heading": s.heading,
                        "lines": s.lines,
                    })
                })
                .collect();
            let report = |mode: &str| -> Vec<serde_json::Value> {
                role::role_section_report(text, mode).iter().map(|r| r.to_json()).collect()
            };
            out.push(json!({
                "stats": role::role_char_stats(text).to_json(),
                "sections": sections,
                "v1Text": role::select_role_text(text, role::MODE_V1),
                "v2Text": role::select_role_text(text, role::MODE_V2),
                "reportV1": report(role::MODE_V1),
                "reportV2": report(role::MODE_V2),
                "excerpt": role::first_meaningful_line(text),
                "warningsV2": role::role_warnings(text, role::MODE_V2, inject_max),
                "warningsV1": role::role_warnings(text, role::MODE_V1, inject_max),
                "injectMaxChars": inject_max,
            }));
        }
        println!("{}", serde_json::to_string(&out).unwrap());
        return Ok(());
    }

    // 排障：打出「真正投递给模型」的完整文本（时间行 + 黑话表 + 正文）。
    // `--build-prompt` 只出正文，因为 JS 的 /api/debug/wake-prompt 就回正文；
    // 包装只在交付那一刻发生，所以要看包装后的样子得单独一个开关。
    if let Some(pos) = std::env::args().position(|a| a == "--prompt-wrapped") {
        let key = std::env::args().nth(pos + 1).unwrap_or_else(|| "private:7001".to_string());
        let reason = std::env::args().nth(pos + 2).unwrap_or_else(|| "atMention".to_string());
        let cfg = read_json(CONFIG_PATH).unwrap_or_default();
        let nick = std::env::var("HER_SELF_NICK").unwrap_or_else(|_| "Fixture Bot".to_string());
        let qq = std::env::var("HER_SELF_QQ").unwrap_or_else(|_| "7004".to_string());
        let body = prompt::build(&key, &reason, &nick, &qq);
        print!("{}", prompt::wrap_for_delivery(&body, &cfg, now_ms() as i64));
        return Ok(());
    }

    // 排障：直接打一个 DSH RPC 方法，看是 cookie 问题还是端点问题
    if let Some(pos) = std::env::args().position(|a| a == "--rpc-probe") {
        let method = std::env::args().nth(pos + 1).unwrap_or_else(|| "session/list".to_string());
        let cfg = read_json(CONFIG_PATH).unwrap_or_default();
        let base = cfg.get("dsh").and_then(|d| d.get("baseUrl")).and_then(|b| b.as_str())
            .unwrap_or("http://127.0.0.1:3080");
        println!("[rpc-probe] base={base} method={method}");
        let rpc = dsh::Rpc::connect(base)?;
        match rpc.call(&method, json!({})) {
            Ok(v) => println!("[rpc-probe] ok: {}", activity::preview_chars(&v.to_string(), 300)),
            Err(e) => println!("[rpc-probe] err: {e}"),
        }
        return Ok(());
    }

    // 回归/排障：只跑一遍抽取解析器，证明「模型输出不是 JSON」时不会崩。
    // 从 stdin 读原文（正文里可能有换行/引号，放命令行参数会被 shell 转义弄坏）。
    if std::env::args().any(|a| a == "--slang-parse-test") {
        use std::io::Read as _;
        let mut raw = String::new();
        let _ = std::io::stdin().read_to_string(&mut raw);
        println!("{}", slang_learn::parse_debug(&raw));
        return Ok(());
    }

    // P10：Rust 版控制台（默认 3101，避免和 JS 的 3100 抢端口）
    if std::env::args().any(|a| a == "--console-serve") {
        let _console_owner = if runtime_context.is_isolated() {
            let gate_path = runtime_context.resolve_path(runtime::RuntimeArea::Data, "lifecycle/lifecycle.lock", runtime::PathIntent::WriteFile)?;
            let _transition = lifecycle::lock_with_wait(&gate_path, std::time::Duration::from_secs(5))?;
            let desired = lifecycle::DesiredStore::new(runtime_context.data_dir().join("lifecycle"));
            match lifecycle::effective(&desired.read(), now_ms() as i64) {
                lifecycle::Effective::Act { wants, .. } if wants.console => {}
                _ => { eprintln!("[her] desired state does not permit console startup"); std::process::exit(3); }
            }
            let path = runtime_context.resolve_path(runtime::RuntimeArea::Data, "logs/rbridge-console.lock", runtime::PathIntent::WriteFile)?;
            let gate = lifecycle::OwnerGate::new(path);
            let identity = lifecycle::current_identity()?;
            match gate.acquire("console", identity, None, now_ms() as i64, std::time::Duration::from_millis(500)) {
                lifecycle::Acquire::Acquired(owner) => Some(owner),
                other => { eprintln!("[her] console owner unavailable: {other:?}"); std::process::exit(3); }
            }
        } else { None };
        let requested_port = args
            .iter()
            .position(|arg| arg == "--console-port")
            .and_then(|index| args.get(index + 1));
        let port = if runtime_context.is_isolated() {
            match requested_port
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|port| *port != 0)
            {
                Some(port) => port,
                None => {
                    eprintln!("[her] isolated runtime requires an explicit non-zero console port");
                    std::process::exit(2);
                }
            }
        } else {
            requested_port
                .and_then(|value| value.parse().ok())
                .unwrap_or(3101)
        };
        if runtime_context.is_isolated() {
            let cfg = match runtime_context.load_config() {
                Ok(config) => config,
                Err(message) => {
                    eprintln!("[her] {message}");
                    std::process::exit(2);
                }
            };
            console::serve_with_context(&cfg, port, runtime_context)?;
        } else {
            let cfg = console::load_config();
            console::serve(&cfg, port)?;
        }
        return Ok(());
    }

    // 401 自愈演练：故意把 cookie 弄坏 → 确认 harness 能重铸并成功。
    //
    // 为什么单独做：真实 401 只有"DSH 重启换密钥"才会出现，那是线上操作。
    // 但**弄坏自己手里的 cookie** 等价于同一个状态（服务端认不出这张票），
    // 所以可以在不影响 DSH 的前提下把这条路走一遍。
    if std::env::args().any(|a| a == "--selftest-401") {
        let cfg_raw = read_json(CONFIG_PATH).ok_or("no config.json")?;
        let base = cfg_raw
            .get("dsh")
            .and_then(|d| d.get("baseUrl"))
            .and_then(|b| b.as_str())
            .unwrap_or("http://127.0.0.1:3080")
            .to_string();
        let mut rpc = dsh::Rpc::connect(&base)?;
        // 伪造成"服务端换了密钥"的效果：票本身格式合法但签名对不上。
        // 直接在末尾加一个字符，破坏签名。
        rpc.credential().disable_refresh();
        rpc.credential().set_cookie(format!("{}x", rpc.cookie()));

        let args = json!({ "workspaceId": "probe", "agentPreset": "default" });
        let first = rpc.call("session/create", args.clone());
        println!("[selftest-401] 坏票调用结果：{}", match &first {
            Ok(_) => "成功（说明 DSH 没校验？）".to_string(),
            Err(e) => format!("失败（预期）：{}", activity::preview_chars(&e, 80)),
        });
        let hit_401 = matches!(&first, Err(e) if e.contains("401"));
        if !hit_401 {
            println!("[selftest-401] ❌ 没拿到 401，这条演练不成立（可能 DSH 未运行）");
            std::process::exit(1);
        }
        rpc.relogin()?;
        // 重铸后用一个**无副作用**的鉴权调用证明票好了。
        // 不用 session/create 收尾：它会真的建会话，而这里只想验"票能用"。
        match rpc.call("agentPresets/list", json!({})) {
            Ok(_) => {
                println!("[selftest-401] ✅ 重铸票后鉴权调用成功（agentPresets/list）");
                std::process::exit(0);
            }
            Err(e) => {
                println!("[selftest-401] ❌ 重铸后仍失败：{e}");
                std::process::exit(1);
            }
        }
    }

    // P8 验收：故意把主 provider 写坏，看是否能自动切到 fallback 并拿到回复
    if std::env::args().any(|a| a == "--failover-probe") {
        let mut cfg_raw = read_json(CONFIG_PATH).ok_or("no config.json")?;
        // 只改主 provider；fallbacks 保持配置原样
        if let Some(dsh) = cfg_raw.get_mut("dsh") {
            dsh["provider"] = serde_json::json!("definitely-not-a-provider");
            dsh["model"] = serde_json::json!("no-such-model");
        }
        let mut pipe = pipeline::Pipeline::new(&cfg_raw, false)?;
        let chain = failover::model_chain(&cfg_raw);
        println!("[failover-probe] 候选链 {} 个：{:?}", chain.len(), chain.iter().map(|c| c.label()).collect::<Vec<_>>());
        let key = "private:7001";
        match pipe
            .ask_with_failover(key, "只回复两个字：收到", 180, &cfg_raw)
            .await
        {
            Ok(text) => println!("[failover-probe] ✅ 拿到回复（{} 字）：{}", text.chars().count(), activity::preview_chars(&text, 40)),
            Err(e) => println!("[failover-probe] ❌ 失败：{e}"),
        }
        return Ok(());
    }

    // P7：同步 QQ 收藏表情（拉取 → 合并 → 落盘）
    if std::env::args().any(|a| a == "--sticker-sync") {
        let cfg_raw = read_json(CONFIG_PATH).ok_or("no config.json")?;
        let http = cfg_raw
            .get("snowluma")
            .and_then(|s| s.get("httpUrl"))
            .and_then(|u| u.as_str())
            .unwrap_or("http://127.0.0.1:3000");
        let token = cfg_raw
            .get("snowluma")
            .and_then(|s| s.get("accessToken"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let ob = onebot::OneBot::new(http, token.to_string());
        match sticker::sync(&ob, &cfg_raw) {
            Ok((fetched, total)) => println!("[sticker] 拉到 {fetched} 条，合并后本地 {total} 条"),
            Err(e) => println!("[sticker] 同步失败：{e}"),
        }
        return Ok(());
    }

    // P2-C：输出 Rust 版的唤醒提示词（用于与 JS 逐字对拍）
    if let Some(pos) = std::env::args().position(|a| a == "--build-prompt") {
        let key = std::env::args().nth(pos + 1).ok_or("--build-prompt needs key")?;
        let reason = std::env::args().nth(pos + 2).unwrap_or_else(|| "atMention".to_string());
        let name = std::env::var("HER_SELF_NICK").unwrap_or_else(|_| "Fixture Bot".to_string());
        let qq = std::env::var("HER_SELF_QQ").unwrap_or_else(|_| "7004".to_string());
        print!("{}", prompt::build(&key, &reason, &name, &qq));
        return Ok(());
    }

    // 诊断：拉 snapshot 并列出事件
    if let Some(pos) = std::env::args().position(|a| a == "--dsh-dump") {
        let sid = std::env::args().nth(pos + 1).ok_or("--dsh-dump needs a sessionId")?;
        let base = dsh_probe_base();
        match dsh::Rpc::connect(&base) {
            Ok(rpc) => {
                if let Err(e) = dsh::dump_snapshot(&base, &rpc.cookie(), &sid).await {
                    println!("[dump] ❌ {e}");
                }
            }
            Err(e) => println!("[dump] ❌ {e}"),
        }
        return Ok(());
    }

    // 诊断：订阅并打印原始帧
    if let Some(pos) = std::env::args().position(|a| a == "--dsh-watch") {
        let sid = std::env::args().nth(pos + 1).ok_or("--dsh-watch needs a sessionId")?;
        let base = dsh_probe_base();
        match dsh::Rpc::connect(&base) {
            Ok(rpc) => {
                if let Err(e) = dsh::watch_raw(&base, &rpc.cookie(), &sid, 30).await {
                    println!("[watch] ❌ {e}");
                }
            }
            Err(e) => println!("[watch] ❌ {e}"),
        }
        return Ok(());
    }

    if std::env::args().any(|a| a == "--dsh-probe" || a == "--dsh-demo") {
        let base = dsh_probe_base();
        if std::env::args().any(|a| a == "--dsh-demo") {
            let preset = std::env::var("DSH_DEMO_PRESET").unwrap_or_else(|_| "qq-chat-v2".to_string());
            match dsh::demo_create_and_prompt(&base, &preset).await {
                Ok(()) => {}
                Err(e) => println!("[dsh] ❌ {e}"),
            }
        } else {
            match dsh::Rpc::connect(&base) {
                Ok(rpc) => match rpc.call("session/list", serde_json::json!({})) {
                    Ok(v) => {
                        let n = v.get("items").and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0);
                        println!("[dsh] 鉴权探针：✅ 通过（看到 {n} 个会话）");
                    }
                    Err(e) => println!("[dsh] 鉴权探针：❌ {e}"),
                },
                Err(e) => println!("[dsh] ❌ {e}"),
            }
        }
        return Ok(());
    }

    // Debug the question text shape only; live direction additionally requires conversation evidence.
    if let Some(pos) = std::env::args().position(|a| a == "--question-probe") {
        let text = std::env::args().nth(pos + 1).unwrap_or_default();
        println!("{}", question::is_question_text(&text));
        return Ok(());
    }
    // 长消息切分探针：从 stdin 读文本，按最大长度切分，段间用 --SPLIT-- 分隔。
    if let Some(pos) = std::env::args().position(|a| a == "--split-probe") {
        use std::io::Read as _;
        let max: usize = std::env::args()
            .nth(pos + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(4000);
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).ok();
        let parts = onebot::split_message(&text, max);
        // 输出 JSON 数组而不是用分隔符拼接：空数组和「一个空串」必须能区分开，
        // 用 `join` 的话两者打印出来是一样的（都是空行），对拍就失去意义了。
        println!("{}", serde_json::json!(parts));
        return Ok(());
    }
    // 出站哨兵探针：判一段文本会不会被拦（红线「零漏放」的对拍入口）。
    if let Some(pos) = std::env::args().position(|a| a == "--sensitive-probe") {
        let text = std::env::args().nth(pos + 1).unwrap_or_default();
        match pipeline::outbound_violation(&text, &[]) {
            Some(why) => println!("BLOCK {why}"),
            None => println!("allow"),
        }
        return Ok(());
    }
    // R4：用指定后端真跑一轮（验证 CliBackend 端到端可用）。
    // prompt 走 stdin；Windows 上可以用 `cmd /c more` 当回显桩来验证链路。
    if let Some(pos) = std::env::args().position(|a| a == "--backend-ask") {
        let id = std::env::args().nth(pos + 1).ok_or("--backend-ask needs backend id")?;
        let text = std::env::args().nth(pos + 2).ok_or("--backend-ask needs text")?;
        let cfg_raw = read_json(CONFIG_PATH).ok_or("no config.json")?;
        let reg = backend::Registry::from_config(&cfg_raw).map_err(|e| e.message().to_string())?;
        let Some(b) = reg.chain.iter().find(|b| b.id() == id) else {
            return Err(format!("没有这个后端：{id}（候选：{:?}）", reg.ids()).into());
        };
        let req = backend::BackendRequest { key: "probe".into(), prompt: text, timeout_s: 60 };
        match b.ask(&req) {
            Ok(r) => println!("{} => {}", r.backend, r.text.trim()),
            Err(e) => println!("{} 失败（可重试={}）：{}", id, e.retryable(), e.message()),
        }
        return Ok(());
    }
    // R4：列出后端候选链与可用性（不真的跑一轮）。
    if std::env::args().any(|a| a == "--backends") {
        let cfg_raw = read_json(CONFIG_PATH).ok_or("no config.json")?;
        match backend::Registry::from_config(&cfg_raw) {
            Ok(reg) => {
                println!("后端候选链（按顺序）：");
                for b in &reg.describe().as_array().cloned().unwrap_or_default() {
                    let avail = b.get("available").and_then(|v| v.as_bool()).unwrap_or(false);
                    println!(
                        "  {} {} {}",
                        if avail { "✅" } else { "❌" },
                        b.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                        b.get("kind").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                }
            }
            Err(e) => println!("配置有误：{}", e.message()),
        }
        return Ok(());
    }
    let live_mode = std::env::args().any(|a| a == "--live");
    let legacy_source = !runtime_context.is_isolated();
    let _live_transition = if live_mode && runtime_context.is_isolated() {
        let gate_path = runtime_context.resolve_path(runtime::RuntimeArea::Data, "lifecycle/lifecycle.lock", runtime::PathIntent::WriteFile)?;
        let gate = lifecycle::lock_with_wait(&gate_path, std::time::Duration::from_secs(5))?;
        let desired = lifecycle::DesiredStore::new(runtime_context.data_dir().join("lifecycle"));
        match lifecycle::effective(&desired.read(), now_ms() as i64) {
            lifecycle::Effective::Act { wants, .. } if wants.live => {}
            _ => { eprintln!("[her] desired state does not permit live startup"); std::process::exit(3); }
        }
        Some(gate)
    } else { None };
    if !live_mode {
        eprintln!("[her] specify --live for the live service");
        std::process::exit(2);
    }
    let Some(mut lock) = acquire_single_instance(&runtime_context) else {
        recovery_log::record(
            &runtime_context,
            run_record::Trigger::Startup,
            run_record::Component::Process,
            "single-instance",
            run_record::Outcome::Skipped,
            "another bridge process owns this runtime; this one exits",
        );
        return Ok(());
    };
    let _source_owner = if runtime_context.is_isolated() {
        session_map::verify_cutover(&runtime_context)?;
        let endpoint = runtime_context.dependency_url("onebot_ws").ok_or("isolated runtime requires a loopback onebot_ws dependency")?;
        let selected = args.iter().position(|arg| arg == "--live-groups").and_then(|index| args.get(index + 1));
        let groups = parse_live_groups(selected.map(String::as_str));
        Some(lifecycle::acquire_message_source(endpoint, &groups)?)
    } else { None };
    drop(_live_transition);
    recovery_log::bind_default(&runtime_context);
    if live_mode {
        recovery_log::record(
            &runtime_context,
            run_record::Trigger::Startup,
            run_record::Component::Process,
            "start",
            run_record::Outcome::Ok,
            "live bridge process started",
        );
        dependency_watch::install_crash_marker(&runtime_context);
    }
    // The marker is diagnostic; only the held OS gate decides ownership.
    let mut lock_touched = std::time::Instant::now();

    let mut cfg_raw = if runtime_context.is_isolated() {
        runtime_context.load_config().map_err(|e| e.to_string())?
    } else {
        read_json(CONFIG_PATH).ok_or("no config.json")?
    };
    let cfg: Config = serde_json::from_value(cfg_raw.clone())?;
    let ws = if runtime_context.is_isolated() {
        runtime_context
            .dependency_url("onebot_ws")
            .filter(|url| is_loopback_ws_url(url))
            .ok_or("isolated runtime requires a loopback onebot_ws dependency")?
            .to_string()
    } else {
        cfg.snowluma
            .as_ref()
            .and_then(|s| s.ws_url.clone())
            .unwrap_or_else(|| "ws://127.0.0.1:3001".to_string())
    };
    let token = if !legacy_source {
        cfg.snowluma.as_ref().and_then(|s| s.access_token.clone()).filter(|t| !t.is_empty())
    } else {
        discover_ws_token().or_else(|| {
            cfg.snowluma.as_ref().and_then(|s| s.access_token.clone()).filter(|t| !t.is_empty())
        })
    };
    let owner_qq = cfg.ownerqq.clone().unwrap_or_default();
    let allow_groups: Vec<String> = cfg.allow.as_ref().map(|a| a.groups.iter().map(|v| v.to_string().trim_matches('"').to_string()).collect()).unwrap_or_default();
    let allow_private: Vec<String> = cfg.allow.as_ref().map(|a| a.private.iter().map(|v| v.to_string().trim_matches('"').to_string()).collect()).unwrap_or_default();

    println!("[her] mode: --live 已指定（是否真发消息还要看 --live-groups）");
    println!("[her] connect {ws} (token {})", if token.is_some() { "ok" } else { "missing" });
    println!("[her] allow groups {allow_groups:?} private {allow_private:?}");

    // ── 重连外层循环 ──
    //
    // 为什么必须有：WS 一旦被对端中止（WSAECONNABORTED / error 10053，SnowLuma 重启、
    // 桥接重启、网络抖动都会触发），原来的 `let msg = msg?;` 会把 IO 错误抛到 main，
    // **整个进程直接退出**。
    // 实测 2026-10-23 13:28 就是这么死的：心跳停在 13:28:22、进程消失，全量切换
    // 静默回落到 JS 桥，而看门狗要等最多 15 分钟才把它拉起来 —— 这段时间群里没人回。
    if std::env::args().any(|a| a == "--no-reconnect") {
        println!("[her] --no-reconnect：连接断开即退出（调试用）");
    }
    // 入站持久化（ticket 07）：打不开就不启动，否则无法声称已确认接收。
    let journal = inbound::Journal::open(&runtime_context)?;
    journal.recover_queued_gap_notices()?;
    let journal = inbound::SharedJournal::new(journal);
    let interrupted = journal.recover_interrupted(now_ms() as i64).await?;
    if !interrupted.is_empty() {
        println!("[her] 上次运行中断的入站事件 {} 条：状态未知，不自动重提，已记入管理端", interrupted.len());
        let notices = journal.run(move |j| j.interrupted_notice_details(&interrupted)).await?;
        let (notice_context, notice_cfg) = (runtime_context.clone(), cfg_raw.clone());
        tokio::task::spawn_blocking(move || {
            dependency_watch::report_interrupted(&notice_context, &notice_cfg, &notices);
        }).await?;
    }
    if live_mode && runtime_context.is_isolated() {
        dependency_watch::report_last_crash(&runtime_context, &cfg_raw);
    }
    let mut reserved2_hub = if runtime_context.is_isolated() && live_mode {
        let args: Vec<String> = std::env::args().collect();
        let groups = parse_live_groups(args.iter().position(|a| a == "--live-groups").and_then(|i| args.get(i+1)).map(String::as_str));
        Some(reserved2::Hub::new(&runtime_context, journal.clone(), groups, args.iter().any(|a| a == "--live-all")))
    } else { None };
    let mut reserved_hub = if runtime_context.is_isolated() && live_mode {
        let args:Vec<String>=std::env::args().collect();
        let groups=parse_live_groups(args.iter().position(|a|a=="--live-groups").and_then(|i|args.get(i+1)).map(String::as_str));
        Some(reserved::Hub::new(&runtime_context,journal.clone(),groups,args.iter().any(|a|a=="--live-all"))?)
    }else{None};
    let mut interaction_hub = None;
    if live_mode && runtime_context.is_isolated() {
        // DSH probe, health snapshot for the console and the SnowLuma status poll: once per process, not per connection.
        tokio::spawn(dependency_watch::run(runtime_context.clone()));
    }
    // Reconnect pacing: doubling waits up to 30 s, reset only by a connection that delivered frames (no storm
    // during a long outage); a warning at the third failure in a row.
    let mut gateway_backoff = dependency::ReconnectBackoff::gateway();
    let mut heal_throttle = snowluma_heal::Throttle::default();
    let mut heal_http_token: Option<String> = None;
    let mut ws = ws;
    let mut token = token;
    'reconnect: loop {
        // Isolated startup validates all selected-root state before contacting its WebSocket.
        // Keep this inside the reconnect loop: a failed handshake drops this candidate pipeline,
        // and the next attempt repeats local validation before dialing again. The selected-root
        // gate is already held, so pipeline setup may create only its selected workspace folder.
        let isolated_live_startup = if runtime_context.is_isolated() {
            let live_groups = std::env::args()
                .position(|arg| arg == "--live-groups")
                .and_then(|index| std::env::args().nth(index + 1));
            let live_groups = parse_live_groups(live_groups.as_deref());
            if let Some(missing) = if live_mode { rollout_mismatch(&cfg_raw, &live_groups) } else { None } {
                println!("[her] ❌ 灰度配置不一致，拒绝启动：");
                println!("           config.json 的 rollout.rustGroups 里有 {missing:?}");
                println!("           命令行 --live-groups = {live_groups:?}");
                println!("           这些会话会被 JS 让出、Rust 又不服务 —— 等于静默失联。");
                println!("           要么把它们加到 --live-groups，要么从 rollout.rustGroups 里去掉。");
                std::process::exit(2);
            }
            let live_all = std::env::args().any(|arg| arg == "--live-all");
            let pipeline = prepare_live_pipeline(
                &runtime_context,
                &cfg_raw,
                live_mode,
                &live_groups,
                live_all,
            )
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            Some((live_groups, live_all, pipeline))
        } else {
            None
        };

        let mut request = ws.as_str().into_client_request()?;
        if let Some(t) = &token {
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {t}").parse()?);
        }
        let (mut stream, _resp) = match connect_async(request).await {
            Ok(x) => x,
            Err(e) => {
                let mut wait = gateway_backoff.on_failure(dependency::jitter_unit(now_ms() as i64, u64::from(gateway_backoff.streak())));
                println!("[her] 连接失败：{e}；{} 秒后重试（连续第 {} 次）", wait / 1000, gateway_backoff.streak());
                gateway_health::with(|h| h.connected = false);
                recovery_log::record(
                    &runtime_context,
                    run_record::Trigger::Timer,
                    run_record::Component::Onebot,
                    "connect-failed",
                    run_record::Outcome::Failed,
                    &format!("gateway connection failed: {e}"),
                );
                if gateway_backoff.should_warn() {
                    println!("[her] ⚠️ 与网关的连接已连续失败 {} 次，继续按退避重试", gateway_backoff.streak());
                    recovery_log::record(
                        &runtime_context,
                        run_record::Trigger::Timer,
                        run_record::Component::Onebot,
                        "connect-streak",
                        run_record::Outcome::Failed,
                        &format!("{} failed connection attempts in a row; retrying with backoff", gateway_backoff.streak()),
                    );
                }
                if dependency_watch::adopt_heal(
                    &runtime_context, &mut heal_throttle, &format!("connect failed {e}"),
                    &mut ws, &mut token, &mut heal_http_token, &mut cfg_raw,
                ).await {
                    wait = 500;
                }
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                continue 'reconnect;
            }
        };
        println!("[her] connected");
        gateway_health::with(|h| h.connected = true);
        if gateway_backoff.streak() > 0 {
            recovery_log::record(
                &runtime_context,
                run_record::Trigger::Recovery,
                run_record::Component::Onebot,
                "reconnected",
                run_record::Outcome::Ok,
                &format!("gateway connected again after {} failed attempts", gateway_backoff.streak()),
            );
        }
        let mut got_traffic = false;

        if let Err(e) = stream
            .send(Message::Text(
                json!({"action":"get_login_info","params":{},"echo":"login"}).to_string(),
            ))
            .await
        {
            println!("[her] 登录信息发送失败：{e}（继续，等下一轮重连）");
        }


    let mut self_id: i64 = 0;
    let mut self_nick = String::new();
    let mut self_cards: HashMap<i64, String> = HashMap::new();

    // 合批窗口与闲聊沉默概率（与 JS 版对齐）
    let skip_probability = cfg
        .social_v2
        .as_ref()
        .map(|s| s.social.skip_probability)
        .unwrap_or(0.0);
    // OneBot HTTP 端点与令牌（群名片查询、引用判定都要用）
    let http_base = if runtime_context.is_isolated() {
        runtime_context
            .dependency_url("onebot")
            .ok_or("isolated runtime requires a OneBot HTTP endpoint")?
            .to_string()
    } else {
        cfg.snowluma
            .as_ref()
            .and_then(|s| s.http_url.clone())
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
    };
    let (http_host, http_port) = if runtime_context.is_isolated() {
        loopback_http_endpoint(&http_base)
            .ok_or("isolated runtime requires a valid loopback OneBot HTTP endpoint")?
    } else {
        loopback_http_endpoint(&http_base).unwrap_or_else(|| ("127.0.0.1".to_string(), 3000))
    };
    let http_token = heal_http_token.clone().unwrap_or_else(|| {
        cfg.snowluma
            .as_ref()
            .and_then(|s| s.access_token.clone())
            .unwrap_or_default()
    });

    // 断线缺口与重放（ticket 07）。每次（重）连接：先按网关历史能力补收未解决的缺口，
    // 再把已保存但尚未处理完的实时事件按原顺序重放，然后才读取新帧。
    let mut awaiting: HashMap<String, Vec<String>> = HashMap::new();
    let mut in_flight: Vec<String> = Vec::new();
    // 缺口先同步记下（快，只写库），再把含阻塞 HTTP 的补收丢到后台线程：
    // 慢网关不能让 WebSocket 在重连后停止读帧。
    {
        let (groups, privates) = (allow_groups.clone(), allow_private.clone());
        let opened = journal
            .run(move |j| inbound::open_reconnect_gaps(j, &groups, &privates, now_ms() as i64))
            .await;
        if let Err(error) = opened {
            println!("[her] ❌ 断线缺口记录失败：{error}");
        }
        // Private history is unavailable: announce its new window without waiting for group HTTP.
        let (notice_context, notice_cfg) = (runtime_context.clone(), cfg_raw.clone());
        tokio::task::spawn_blocking(move || {
            dependency_watch::report_unresolved_gaps(&notice_context, &notice_cfg, "inbound-gap-opened");
        }).await?;
        let (host, port, token) = (http_host.clone(), http_port, http_token.clone());
        let recovery_context = runtime_context.clone();
        let spawned = inbound::spawn_gap_recovery(
            move || inbound::Journal::open(&recovery_context),
            move |group_id, seq| inbound::fetch_group_history(&host, port, &token, group_id, seq),
            now_ms() as i64,
            {
                let (gap_context, gap_cfg) = (runtime_context.clone(), cfg_raw.clone());
                move |result| match result {
                    Ok(report) => {
                        if !report.is_empty() {
                            println!("[her] 断线缺口补收结果：{}", report.join("；"));
                        }
                        dependency_watch::report_unresolved_gaps(&gap_context, &gap_cfg, "inbound-gap");
                    }
                    Err(error) => println!("[her] ❌ 断线缺口补收失败：{error}"),
                }
            },
        );
        if spawned.is_none() {
            println!("[her] 上一次缺口补收仍在进行，本次只记录缺口");
        }
    }
    let mut seen = inbound::SeenTracker::default();
    let mut last_prune = std::time::Instant::now();
    let mut replay_queue: std::collections::VecDeque<inbound::Replayable> = std::collections::VecDeque::new();
    for item in journal.replayable().await? {
        let allowed = if item.conversation.starts_with("group:") {
            allow_groups.iter().any(|g| item.conversation == format!("group:{g}"))
        } else {
            allow_private.iter().any(|p| item.conversation == format!("private:{p}"))
        };
        if allowed {
            replay_queue.push_back(item);
        } else {
            journal.mark_handled(vec![item.event_key.clone()], now_ms() as i64).await?;
        }
    }
    if !replay_queue.is_empty() {
        println!("[her] 重放已保存但未处理完的入站事件 {} 条", replay_queue.len());
    }

    // 灰度：只接管列出来的会话（"group:123,private:456"）。空 = 不接管任何会话。
    let (live_groups, _live_all, mut pipeline) = match isolated_live_startup {
        Some((live_groups, live_all, pipeline)) => {
            if !live_groups.is_empty() {
                println!("[her] 灰度接管：{live_groups:?}");
            }
            (live_groups, live_all, pipeline)
        }
        None => {
    let live_groups = std::env::args()
        .position(|arg| arg == "--live-groups")
        .and_then(|index| std::env::args().nth(index + 1));
    let live_groups = parse_live_groups(live_groups.as_deref());
    if !live_groups.is_empty() {
        println!("[her] 灰度接管：{live_groups:?}");
    }
    // 交接一致性：JS 的闸门是 `rollout.rustGroups`（配置文件），Rust 实际服务的是
    // 命令行 `--live-groups`。两者不一致的后果是**静默失联**：
    // JS 看到 rustGroups 就把这些会话的唤醒让给 Rust，而 Rust 没打算服务它们 ——
    // 两边都不回，群里就是"机器人突然哑了"，而且没有任何报错。
    //
    // 所以发现不一致就**直接退出**：Rust 退出后心跳停更，JS 在 TTL 内自动收回这些会话，
    // 最坏是短暂停顿。比起"永久哑掉直到有人发现"，这个失败方式好得多。
    if let Some(missing) = if live_mode { rollout_mismatch(&cfg_raw, &live_groups) } else { None } {
        println!("[her] ❌ 灰度配置不一致，拒绝启动：");
        println!("           config.json 的 rollout.rustGroups 里有 {missing:?}");
        println!("           命令行 --live-groups = {live_groups:?}");
        println!("           这些会话会被 JS 让出、Rust 又不服务 —— 等于静默失联。");
        println!("           要么把它们加到 --live-groups，要么从 rollout.rustGroups 里去掉。");
        std::process::exit(2);
    }
    let live_all = std::env::args().any(|a| a == "--live-all");
    // Do this before the heartbeat task starts. An isolated candidate that cannot load its own
    // session/credentials/dependencies must exit without claiming the selected groups.
    let pipeline = prepare_live_pipeline(
        &runtime_context,
        &cfg_raw,
        live_mode,
        &live_groups,
        live_all,
    )
    .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            (live_groups, live_all, pipeline)
        }
    };
    // 回合恢复（ticket 10）：先向 DSH 与发送账本核对上次中断的回合，再决定重放、等待还是暂停。
    if pipeline.is_some() {
        turn_recovery::spawn(runtime_context.clone());
    }
    // Chat mode (ticket 12): a live selected runtime serves chat whenever its stored mode is `chat`.
    let mut chat_hub: Option<chat::Hub> = if pipeline.is_some() && runtime_context.is_isolated() {
        if interaction_hub.is_none() {
            interaction_hub = Some(closed_agent::Hub::start(&runtime_context)?);
        }
        match chat::Hub::new(&runtime_context, journal.clone(), &cfg_raw, interaction_hub.clone()) {
            Ok(hub) => Some(hub),
            Err(error) => {
                println!("[her] ❌ chat 模式不可用：{error}");
                None
            }
        }
    } else {
        None
    };
    // Publish the live heartbeat consumed by the Rust supervisor.
    {
        let groups = live_groups.clone();
        let heartbeat_context = runtime_context.clone();
        let name = if live_mode {
            "rust-heartbeat-live.json"
        } else {
            "rust-heartbeat.json"
        };
        tokio::spawn(async move {
            loop {
                let payload = json!({
                    "ts": now_ms(),
                    "pid": std::process::id(),
                    "live": !groups.is_empty(),
                    "groups": groups,
                });
                if heartbeat_context.is_isolated() {
                    let _ = heartbeat_context.write_atomic(
                        runtime::RuntimeArea::Data,
                        name,
                        payload.to_string().as_bytes(),
                    );
                } else {
                    let _ = std::fs::write(
                        format!("C:/qqbot/qq-bridge/state/{name}"),
                        payload.to_string(),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
    }
    // 每个会话最近几句，用来构造唤醒提示（完整版应该按 JS 的 buildWakePromptV2 来）
    let mut recent_by_key: HashMap<String, Vec<String>> = HashMap::new();
    // DSH 的完整提示也要看当前 Rust 进程收到的消息。与字符串版 CLI 窗口分开保存，
    // 这样既不改 CLI prompt 合同，也能按 OneBot message_id 与持久化 recentMessages 合并去重。
    let mut runtime_recent_by_key: HashMap<String, Vec<prompt::RuntimeRecentMessage>> =
        HashMap::new();
    // S1 黑话抽取：窗口本身在 `slang_learn` 里（跨进程落盘），这里只决定
    // 「要不要喂」和「要不要自动排一次抽取」。
    // 自动抽取只在 live 进程开：影子和 live 会同时存在，两边都抽 = 同一批消息花两份额度。
    let slang_enabled = cfg_raw
        .get("slang")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let slang_auto = live_mode && slang_enabled;
    let slang_learning = slang_learn::SlangLearning::new(&runtime_context);
    if slang_auto {
        slang_learning.start_dsh_watch();
    }

    let mut events = 0u64;
    let mut wakes = 0u64;
    let mut last_heartbeat_ms: u128 = 0;

    // Live decision and wake diagnostics retain their existing record format.
    let decision_legacy_path = DECISION_LOG;
    let wake_legacy_path = WAKE_LOG;
    let decision_name = "logs/rbridge-live.jsonl";
    let wake_name = "logs/rbridge-live-wakes.jsonl";
    let shadow_flag = live_groups.is_empty();
    append_runtime_json_line(
        &runtime_context,
        decision_name,
        decision_legacy_path,
        &json!({
            "ts": now_ms(),
            "type": "start",
            "pid": std::process::id(),
            "shadow": shadow_flag,
            "groups": live_groups,
        }),
    )?;

    // P6：每会话一个状态机。消息喂进去，唤醒从 select 的定时分支吐出来。
    let mut drivers: HashMap<String, session::Driver> = HashMap::new();
    let mut wake_synced: HashMap<String, session::WakeSync> = HashMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // ticker 分支里**不能**直接 await live pipeline（它在 select! 里，借用冲突），
    // 所以先把「该由 live 处理的唤醒」攒在这里，下一轮循环开头执行。
    // 延迟最多一个 tick（500ms），对聊天无所谓。
    let mut pending_live: Vec<(String, String)> = Vec::new();

    loop {
        let tick_now = now_ms() as i64;

        // ── 先补跑上一轮 ticker 攒下的 live 唤醒 ──
        //
        // 修的是灰度的核心 bug：live 的回复逻辑原本只挂在「消息路径」上，
        // 而**绝大多数唤醒是 ticker 点火的**（消息只是 schedule，合批窗口到点由 tick 点火）。
        // 结果：live 进程天天打「定时唤醒」日志，但一次都不回复。
        // 实测 2026-10-23 09:28 香草群那条 probability 唤醒就是这么白醒的。
        //
        // 两条后端两种打法：
        //   · CLI 纯文本后端 → `build_text_only`（自包含、不让它调工具）+ Rust 代发
        //   · DSH           → `prompt::build`（完整提示词）+ 模型自己调工具发
        // 都由 `run_turn` 内部按 `backends.sessions` 决定，这里只把两份 prompt 都备好。
        for (key, reason) in std::mem::take(&mut pending_live) {
            if let Some(pipe) = pipeline.as_mut() {
                let Ok(target) = parse_target(&key, &cfg_raw) else {
                    println!("[her] {key} 的 target 解析失败，跳过");
                    continue;
                };
                let runtime_recent = runtime_recent_by_key
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                let recent = recent_by_key.get(&key).cloned().unwrap_or_default();
                let dsh_prompt = prompt::build_with_runtime_and_recent(
                    &runtime_context,
                    &key,
                    &reason,
                    &self_nick,
                    &self_id.to_string(),
                    &runtime_recent,
                );
                let cli_prompt = prompt::build_text_only_with_runtime(&runtime_context, &key, &recent, &self_nick, &self_id.to_string());
                println!("[her] 定时唤醒 {key}（{reason}）：执行 live 回合");
                pipe.note_turn(&reason, &in_flight);
                match pipe.run_turn(&target, &dsh_prompt, &cli_prompt, &cfg_raw).await {
                    Ok(n) => println!("[her] 回合完成（{n} 字输出）"),
                    Err(e) => {
                        println!("[her] ❌ 回合失败：{e}");
                        // 被暂缓的唤醒没有起回合：不能让「刚派发」的忙标记把之后交回的唤醒压住 60 秒。
                        if e.starts_with(pipeline::HELD_ERROR_PREFIX) {
                            if let Some(d) = drivers.get_mut(&key) {
                                d.sched.pending_wake_key_at = None;
                            }
                        }
                    }
                }
            }
        }
        // 每 10 秒把锁文件重写一遍（pid + 新鲜时间戳）。
        // 不做这件事的话，30 秒后锁就"过期"了，第二个影子能起来跟它抢着写日志。
        if lock_touched.elapsed().as_secs() >= 10 {
            let _ = lock.refresh(tick_now);
            lock_touched = std::time::Instant::now();
        }
        // 长连接不会触发重连补收，保留清理每小时在阻塞线程里跑一次（策略见 inbound.rs）。
        if last_prune.elapsed().as_secs() >= 3600 {
            last_prune = std::time::Instant::now();
            let pruning = journal.clone();
            tokio::spawn(async move {
                if let Err(error) = pruning.run(|j| j.prune(now_ms() as i64)).await {
                    println!("[her] ❌ 入站库保留清理失败：{error}");
                }
            });
        }
        // 上一轮的回合（若有）已在上面执行完毕：处理中 → 已处理。
        if !in_flight.is_empty() {
            let keys = std::mem::take(&mut in_flight);
            journal.mark_handled(keys, now_ms() as i64).await?;
        }
        // 回合恢复核对后明确未执行且仍有效的事件（ticket 10）排到重放队列末尾。
        replay_queue.extend(turn_recovery::take_resumable());
        // 重放队列优先于新帧；重放事件已经由上一次连接保存过，不再写入。
        let (text, replayed) = if let Some(item) = replay_queue.pop_front() {
            (item.raw, true)
        } else {
        let msg = tokio::select! {
            m = stream.next() => match m {
                Some(m) => m,
                None => break,
            },
            _ = ticker.tick() => {
                if let Some(hub)=reserved_hub.as_mut(){ if let Err(error)=hub.tick(&self_nick).await {println!("[reserved] scheduler error: {error}");} }
                if let Some(hub) = reserved2_hub.as_mut() {
                    if let Err(error) = hub.tick(&self_nick, self_id).await { println!("[reserved2] scheduler error: {error}"); }
                    continue;
                }
                // 没有新消息也要推进定时器：合批窗口、回复检查、有限潜水全靠它。
                if let Some(state) = runtime_json(&runtime_context, runtime::RuntimeArea::Data, "social-v2.json", SOCIAL_PATH) {
                    sync_wake_configs(&runtime_context, &cfg_raw, &state, &mut drivers, &mut wake_synced, tick_now);
                }
                for (k, e) in session::pump_all(&mut drivers, tick_now) {
                    inbound::start_processing(&journal, &mut awaiting, &mut in_flight, &k, now_ms() as i64).await?;
                    wakes += 1;
                    println!("[her] 定时唤醒 {k} ({})", e.reason);
                    append_runtime_json_line(
                        &runtime_context,
                        wake_name,
                        wake_legacy_path,
                        &json!({"ts": e.at_ms, "key": k, "reason": e.reason, "pid": std::process::id(), "shadow": shadow_flag}),
                    )?;

                    // 灰度里的会话：排队给 live pipeline，下一轮循环开头执行。
                    // 不在这里直接跑，是因为 ticker 在 select! 分支里，拿不到 &mut pipeline。
                    if live_mode && live_groups.iter().any(|g| g == &k) {
                        pending_live.push((k.clone(), e.reason.clone()));
                    }

                }
                continue;
            }
        };
        // 连接断了不要 `?`（那会让 main 返回 Err、进程退出）。跳出内层循环去重连。
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                println!("[her] WS 断开：{e}；准备重连");
                break;
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        (text, false)
        };
        // 未配对的 \uD800-\uDFFF 转义（被截断的 emoji）：JS 原样保留，Rust 的 String 装不下，
        // 换成 U+FFFD 后照常处理并记日志（已接受偏差 18），不能静默丢掉整条消息。
        let (ev, text) = match aux_text::parse_json_replacing_lone_surrogates::<Event>(&text) {
            Ok((ev, None)) => (ev, text),
            Ok((ev, Some(repaired))) => {
                println!("[her] 入站帧含孤立代理项转义，已替换为 U+FFFD 后照常处理");
                (ev, repaired)
            }
            Err(_) => continue,
        };
        // 心跳等元事件只记内存，断线时补写一次；其余帧才落盘（不在异步线程里 fsync）。
        if !replayed {
            got_traffic = true;
            dependency_watch::note_frame(&ev.post_type, &ev.meta_event_type, ev.status.as_ref());
            if let Some(at) = seen.observe(now_ms() as i64, &ev.post_type) {
                journal.touch_seen(at).await?;
            }
        }

        if ev.echo.is_some() {
            // 群名片响应
            if let Some(echo) = ev.echo.as_ref().and_then(|e| e.as_str()) {
                if let Some(gid) = echo.strip_prefix("card:").and_then(|s| s.parse::<i64>().ok()) {
                    let card = ev
                        .data
                        .as_ref()
                        .and_then(|d| d.get("card"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !card.is_empty() {
                        println!("[her] 群 {gid} 的群名片：{card}");
                        self_cards.insert(gid, card);
                    }
                }
            }
            if let Some(d) = &ev.data {
                if self_nick.is_empty() {
                    if let Some(n) = d.get("nickname").and_then(|v| v.as_str()) {
                        self_nick = n.to_string();
                        self_id = d.get("user_id").and_then(protocol_integer).filter(|id| *id > 0).unwrap_or(self_id);
                        println!("[her] identity {self_nick} ({self_id})");
                        // The console is a separate process: it learns the bot identity (prompt `self.*`) from this file.
                        if runtime_context.is_isolated() {
                            let identity = serde_json::json!({ "userId": self_id.to_string(), "nickname": self_nick, "updatedAt": now_ms() });
                            if let Err(error) = runtime_context.write_atomic(runtime::RuntimeArea::Data, "identity.json", identity.to_string().as_bytes()) {
                                println!("[her] identity.json 未能写入：{error}");
                            }
                        }

        // 群名片：不同群的别名不一样（香草群叫「大肥鱼」）。不查这个的话 nameMention 整类失效。
        // 用 HTTP（JS 版也是走 HTTP 的 onebotCall；WS 不保证支持这个 action）。
        {
            if !http_token.is_empty() && self_id != 0 {
                for g in &allow_groups {
                    if let Ok(gid) = g.parse::<i64>() {
                        match fetch_group_card(&http_host, http_port, &http_token, gid, self_id) {
                            Some(card) => {
                                println!("[her] 群 {gid} 的群名片：{card}");
                                self_cards.insert(gid, card);
                            }
                            None => println!("[her] 群 {gid} 无群名片（或查询失败）"),
                        }
                    }
                }
            }
        }
                    }
                }
            }
            continue;
        }
        if ev.post_type == "meta_event" {
            if ev.meta_event_type == "heartbeat" {
                // 降噪：每 60 秒最多一条（heartbeat 默认 10 秒一次）
                let t = now_ms();
                if t - last_heartbeat_ms >= 60_000 {
                    last_heartbeat_ms = t;
                    println!("[her] heartbeat (events={events} wakes={wakes})");
                }
            }
            continue;
        }
        let is_message = ev.post_type == "message";
        let is_notice = ev.post_type == "notice";
        if !is_message && !is_notice {
            continue;
        }
        if self_id == 0 {
            self_id = ev.self_id.unwrap_or(0);
            // The numeric identity is usable before the optional login nickname arrives.
            if self_id > 0 && runtime_context.is_isolated() {
                let saved: Value = runtime_context.read_optional_text(runtime::RuntimeArea::Data, "identity.json")
                    .ok().flatten().and_then(|text|serde_json::from_str(&text).ok()).unwrap_or(Value::Null);
                let id = self_id.to_string();
                let nickname = if !self_nick.is_empty() {self_nick.as_str()}
                    else if saved["userId"] == id {saved["nickname"].as_str().unwrap_or("")} else {""};
                let identity = json!({"userId":id,"nickname":nickname,"updatedAt":now_ms()});
                if let Err(error) = runtime_context.write_atomic(runtime::RuntimeArea::Data,"identity.json",identity.to_string().as_bytes()) {
                    println!("[her] identity.json 未能写入：{error}");
                }
            }
        }
        let sender = if is_notice { ev.sender_id.or(ev.user_id) } else { ev.user_id }
            .or_else(|| ev.sender.as_ref().and_then(|s| s.user_id));
        if self_id > 0 && (sender == Some(self_id)
            || (is_message && ev.sender.as_ref().and_then(|s| s.user_id) == Some(self_id))) {
            continue;
        }

        // Administrative release approval is independent of participation mode.
        if is_message && live_mode && pipeline.is_some() && runtime_context.is_isolated() {
            let (plain, _, _) = plain_text(&ev.message, self_id);
            if chat::is_update_command(&plain) {
                let (kind, id) = if ev.message_type == "private" {
                    ("private", sender.unwrap_or(0))
                } else {
                    ("group", ev.group_id.unwrap_or(0))
                };
                let key = format!("{kind}:{id}");
                if id == 0 || !(live_groups.is_empty() || live_groups.contains(&key)) {
                    continue;
                }
                let inbound_key =
                    inbound::event_key(&key, &ev.post_type, event_message_id(&ev).as_deref(), &text);
                if !replayed
                    && journal
                        .record_received(
                            inbound_key.clone(),
                            key.clone(),
                            text.clone(),
                            now_ms() as i64,
                        )
                        .await?
                        == inbound::Receipt::Duplicate
                {
                    continue;
                }
                // Keep replacement behind this command's reply, approval and durable
                // settlement. A replacement must never replay a partly executed /update.
                let _admission = release::turn_guard(&runtime_context).await?;
                let context = runtime_context.clone();
                let user = sender.unwrap_or(0).to_string();
                let result = tokio::task::spawn_blocking(move || {
                    chat::handle_update_command(&context, &key, &user, &plain)
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    crate::safe_eprintln!("[release] update command refused or reply not confirmed");
                }
                journal
                    .mark_handled(vec![inbound_key], now_ms() as i64)
                    .await?;
                continue;
            }
        }

        if live_mode && turn_recovery::owner_command(&runtime_context, &journal, &ev, &text, replayed).await? {
            continue;
        }

        // Chat mode (ticket 12): the runtime's stored mode picks the pipeline. Everything below this
        // block is the reserved2 wake machinery, which chat never touches (pokes are not chat input).
        if let Some(hub) = chat_hub.as_mut() {
            if matches!(console::runtime_mode(&runtime_context).as_str(), "chat" | "reserved" | "closed-agent") {
                if !is_message {
                    continue;
                }
                let (kind, id) = if ev.message_type == "private" {
                    ("private", ev.user_id.unwrap_or(0))
                } else {
                    ("group", ev.group_id.unwrap_or(0))
                };
                let me = ev.self_id.unwrap_or(self_id);
                let own = ev.user_id == Some(me) || ev.sender.as_ref().and_then(|s| s.user_id) == Some(me);
                let key = format!("{kind}:{id}");
                if id == 0 || own || !(live_groups.is_empty() || live_groups.iter().any(|g| g == &key)) {
                    continue;
                }
                let chat_cfg = match runtime_context.load_config() {
                    Ok(chat_cfg) => chat_cfg,
                    Err(error) => {
                        println!("[her] ❌ chat 读取配置失败：{error}");
                        continue;
                    }
                };
                if !chat::allowed_in_mode(&chat_cfg, &console::runtime_mode(&runtime_context), &key) {
                    continue;
                }
                let inbound_key = inbound::event_key(&key, &ev.post_type, event_message_id(&ev).as_deref(), &text);
                if !replayed
                    && journal
                        .record_received(inbound_key.clone(), key.clone(), text.clone(), now_ms() as i64)
                        .await?
                        == inbound::Receipt::Duplicate
                {
                    continue;
                }
                events += 1;
                let inb = chat::Inbound {
                    kind,
                    id,
                    user_id: ev.user_id.unwrap_or(0),
                    self_id: me,
                    card: ev.sender.as_ref().map(|s| s.card.trim().to_string()).unwrap_or_default(),
                    nickname: ev.sender.as_ref().map(|s| s.nickname.trim().to_string()).unwrap_or_default(),
                    message_id: event_message_id(&ev),
                    message: ev.message.clone(),
                };
                if console::runtime_mode(&runtime_context)=="reserved" {
                    if let Some(reserved)=reserved_hub.as_mut(){if let Err(error)=reserved.handle(&inb,&inbound_key,&self_nick).await{println!("[reserved] ingress error: {error}");}}
                    continue;
                }
                match hub.handle(&chat_cfg, &inb, &inbound_key).await {
                    Ok(chat::Outcome::Dispatched { plain, sender }) => {
                        if kind == "group" && slang_learn::enabled(&chat_cfg) {
                            feed_slang(&slang_learning, live_mode, &chat_cfg, &key, &sender, &plain);
                        }
                    }
                    Ok(chat::Outcome::Done) => {}
                    Err(error) => {
                        println!("[her] ❌ chat 消息处理失败 {key}：{error}");
                        journal.mark_handled(vec![inbound_key], now_ms() as i64).await?;
                    }
                }
                continue;
            }
        }

        if let Some(hub) = reserved2_hub.as_mut() {
            if console::runtime_mode(&runtime_context) == "reserved2" {
                if let Err(error) = hub.handle(&ev, &text, replayed, self_id, &self_nick, &self_cards).await { println!("[reserved2] ingress error: {error}"); }
                continue;
            }
        }
        let key = if is_message && ev.message_type == "private" {
            format!("private:{}", ev.user_id.unwrap_or(0))
        } else if is_notice && ev.sub_type == "poke" && ev.group_id.is_none() {
            let Some(peer) = ev.user_id.or(ev.sender_id).or_else(|| ev.sender.as_ref().and_then(|s| s.user_id)) else { continue };
            format!("private:{peer}")
        } else if ev.message_type == "group" || (is_notice && ev.sub_type == "poke" && ev.group_id.is_some()) {
            format!("group:{}", ev.group_id.unwrap_or(0))
        } else {
            continue;
        };

        let allowed = if key.starts_with("group:") {
            allow_groups.iter().any(|g| key == format!("group:{g}"))
        } else {
            allow_private.iter().any(|p| key == format!("private:{p}"))
        };
        if !allowed {
            continue;
        }

        let (plain, at_self_direct, reply_id) = plain_text(&ev.message, self_id);
        let body = if plain.is_empty() { ev.raw_message.clone() } else { plain };
        // 引用了我的消息 = 在对我说话（对齐 JS 的 isQuoteTargetSelf）
        let quote_self = match (reply_id, self_id != 0) {
            (Some(mid), true) => fetch_msg_sender_is_self(&http_host, http_port, &http_token, mid, self_id),
            _ => false,
        };
        let at_self = at_self_direct || quote_self;
        // 与 JS 一致：**状态里没有这个会话就直接跳过**。
        // 之前这里 unwrap_or_default() 会给未知会话套上默认触发器（@/概率 0.05），
        // 于是重放出来的唤醒数比 JS 多一截 —— 数据对不上的第一来源就是这个。
        let Some(tr) = runtime_json(&runtime_context, runtime::RuntimeArea::Data, "social-v2.json", SOCIAL_PATH)
            .and_then(|v| serde_json::from_value::<SocialState>(v).ok())
            .and_then(|s| s.conversations.get(&key).and_then(|c| c.wake_config.clone()))
            .map(|w| w.triggers)
        else {
            continue;
        };

        // 入站持久化（ticket 07）：保存成功后才进入唤醒；同一事件身份的重复到达不产生第二次工作。
        let inbound_key = inbound::event_key(&key, &ev.post_type, event_message_id(&ev).as_deref(), &text);
        if !replayed
            && journal
                .record_received(inbound_key.clone(), key.clone(), text.clone(), now_ms() as i64)
                .await?
                == inbound::Receipt::Duplicate
        {
            continue;
        }
        events += 1;
        let conv = runtime_json(&runtime_context, runtime::RuntimeArea::Data, "social-v2.json", SOCIAL_PATH)
            .map(|s|s["conversations"][&key].clone()).unwrap_or(Value::Null);
        let directed = question::directed(&ev, &body, quote_self, &conv, &cfg_raw, now_ms() as i64);
        let (mut wake, mut reason) = decide(&ev, &body, at_self_direct || quote_self, &self_nick, &self_cards, &tr, directed);

        // First contact with a reserved2 conversation: its first wake is the bootstrap wake that
        // carries the operator's initial prompt (JS `bootstrapSent`), whatever the triggers say.
        if pipeline.is_some()
            && (live_groups.is_empty() || live_groups.iter().any(|g| g == &key))
            && (is_message || ev.sub_type == "poke")
        {
            match console::claim_first_wake(&runtime_context, &cfg_raw, &key) {
                Ok(true) => {
                    println!("[her] {key} 首次唤醒：升级为 bootstrap");
                    wake = true;
                    reason = "bootstrap".to_string();
                }
                Ok(false) => {}
                Err(error) => println!("[her] ❌ {key} 首次唤醒标记失败（本次不注入初始提示词）：{error}"),
            }
        }

        // ① 合批窗口：同一会话 1.5 秒内的连发只算一次唤醒（JS 的 batchWindowMs）。
        //    不做这步的话，一段连发会被记成 N 次唤醒 → 误判暴涨。
        // The driver owns the batch deadline and priority upgrade.
        if wake && reason == "probability" && skip_probability > 0.0 {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text_seed(&body, ev.user_id).hash(&mut h);
            let roll = (h.finish() % 10_000) as f64 / 10_000.0;
            if roll < skip_probability {
                reason = "skip:probability".into();
                wake = false;
            }
        }

        if wake {
            wakes += 1;
        }
        // 需要唤醒的事件保持 received，唤醒真正触发后才转为 processing；其余事件不产生工作，立即 handled。
        if wake {
            awaiting.entry(key.clone()).or_default().push(inbound_key.clone());
        } else {
            journal.mark_handled(vec![inbound_key.clone()], now_ms() as i64).await?;
        }

        // 维护最近消息（无论是否唤醒）
        {
            let who = ev.user_id.map(|u| u.to_string()).unwrap_or_else(|| "?".to_string());
            let line = format!("{who}: {body}");
            let buf = recent_by_key.entry(key.clone()).or_default();
            buf.push(line);
            if buf.len() > 12 {
                buf.remove(0);
            }
            let sender = ev
                .sender
                .as_ref()
                .map(|value| {
                    if !value.card.trim().is_empty() {
                        value.card.trim().to_string()
                    } else if !value.nickname.trim().is_empty() {
                        value.nickname.trim().to_string()
                    } else {
                        ev.user_id
                            .map(|user| user.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    }
                })
                .unwrap_or_else(|| {
                    ev.user_id
                        .map(|user| user.to_string())
                        .unwrap_or_else(|| "?".to_string())
                });
            let live_message = prompt::RuntimeRecentMessage {
                message_id: event_message_id(&ev),
                sender,
                text: body.clone(),
                is_self: ev.user_id == Some(self_id),
                time_ms: now_ms().min(i64::MAX as u128) as i64,
            };
            let live_messages = runtime_recent_by_key.entry(key.clone()).or_default();
            live_messages.push(live_message);
            if live_messages.len() > 12 {
                live_messages.remove(0);
            }
        }

        // S1：喂黑话学习窗口（只喂普通消息；命令/角色控制语由 feed_window 自己滤掉）。
        // 触发判断在这里只做「计数 + 冷却」，真正的模型调用是 tokio::spawn 出去的：
        // opencode CLI 一次可能要十几秒，绝不能卡住消息处理。
        if is_message && slang_enabled {
            let sender = ev
                .sender
                .as_ref()
                .map(|s| {
                    if !s.card.trim().is_empty() {
                        s.card.trim().to_string()
                    } else if !s.nickname.trim().is_empty() {
                        s.nickname.trim().to_string()
                    } else {
                        ev.user_id.map(|u| u.to_string()).unwrap_or_else(|| "未知".to_string())
                    }
                })
                .or_else(|| ev.user_id.map(|u| u.to_string()))
                .unwrap_or_else(|| "未知".to_string());
            let feed = slang_learning.try_feed_window(&key, &sender, &body);
            if let Err(error) = &feed {
                crate::safe_println!("[slang] 候选黑话窗口读写失败：{error}");
            }
            let trigger = matches!(feed, Ok(true))
                && slang_auto
                && match slang_learning.try_maybe_trigger(&cfg_raw, &key) {
                    Ok(trigger) => trigger,
                    Err(error) => {
                        crate::safe_println!("[slang] 候选黑话窗口读取失败：{error}");
                        false
                    }
                };
            if trigger {
                println!("[slang] 触发黑话抽取：{key}");
                if let Err(error) = slang_learning.spawn_auto(cfg_raw.clone(), key.clone()) {
                    crate::safe_println!("[slang] 无法排队抽取：{error}");
                }
            }
        }

        // 决策喂给状态机；真正的唤醒由它按合批窗口/优先级吐出来。
        let now_i = now_ms() as i64;
        let _created = drivers.entry(key.clone()).or_insert_with(|| {
            let sess = runtime_json(&runtime_context, runtime::RuntimeArea::Data, "social-v2.json", SOCIAL_PATH)
                .and_then(|s| s.get("conversations").and_then(|c| c.get(&key)).cloned())
                .unwrap_or_else(|| serde_json::json!({}));
            let mut sched = session::scheduler_from_state(wake_cfg_of(&cfg_raw), &sess);
            // 与 JS 一致：加载时兜一次「配置里一个触发条件都没有」→ 重置默认，避免永眠。
            // （JS 在加载、主动检查、回合收尾、mark_read 四处都会兜；原来 Rust 只有 mark_read。）
            if session::ensure_wakeable_at_load(&mut sched, &cfg_raw, now_i, 0.5) {
                println!("[her] {key} 的唤醒配置没有任何触发条件，已按默认配置兜底");
            }
            let mut d = session::Driver::new(sched, text_seed(&key, None));
            d.arm_proactive_now(now_i);
            d
        });

        if let Some(state) = runtime_json(&runtime_context, runtime::RuntimeArea::Data, "social-v2.json", SOCIAL_PATH) {
            sync_wake_configs(&runtime_context, &cfg_raw, &state, &mut drivers, &mut wake_synced, now_i);
        }
        let driver = drivers.get_mut(&key).expect("driver exists");
        let rec = json!({
            "ts": now_i,
            "key": key,
            "shouldWake": wake,
            "reason": reason,
            "textChars": body.chars().count(),
            "postType": ev.post_type,
            "busy": driver.sched.is_busy(now_i),
            "pending": driver.sched.pending.len(),
            "pid": std::process::id(),
            "shadow": true
        });
        append_runtime_json_line(
            &runtime_context,
            decision_name,
            decision_legacy_path,
            &rec,
        )?;

        let fired = if wake {
            driver.apply(session::Input::Message {
                reason: reason.clone(),
                seq: now_i,
                at_ms: now_i,
            })
        } else {
            driver.apply(session::Input::Tick { at_ms: now_i })
        };

        // 打断：状态机判定「这个回合该让位」时，先取消 DSH 里正在跑的那一轮。
        if let Some(why) = driver.preempted.take() {
            if let Some(pipe) = pipeline.as_mut() {
                println!("[her] 抢占 {key}（{why}）：取消当前回合");
                if let Err(e) = pipe.cancel(&key) {
                    println!("[her] ❌ 取消回合失败：{e}");
                }
            }
        }

        if !fired.is_empty() {
            inbound::start_processing(&journal, &mut awaiting, &mut in_flight, &key, now_ms() as i64).await?;
        }
        for e in fired {
            wakes += 1;
            println!("[her] wake {key} ({})", e.reason);
            append_runtime_json_line(
                &runtime_context,
                wake_name,
                wake_legacy_path,
                &json!({"ts": e.at_ms, "key": key, "reason": e.reason, "pid": std::process::id(), "shadow": shadow_flag}),
            )?;

            // P2-C：--live 才真的走完一轮（建会话 -> 完整提示词 -> 模型自己决定发什么）
            //
            // 和 ticker 那条路径用同一套：`prompt::build` + `run_turn`（不代发）。
            // P2-C：--live 才真的走完一轮
            //
            // 和 ticker 那条路径同一套：CLI 后端用自包含提示词 + 代发，DSH 用完整提示词 + 自调工具。
            if let Some(pipe) = pipeline.as_mut() {
                let taken = live_groups.is_empty() || live_groups.iter().any(|g| g == &key);
                if !taken {
                    println!("[her] {key} 不在灰度名单，交给 JS");
                } else if let Ok(target) = parse_target(&key, &cfg_raw) {
                    // 状态机需要知道「回合开始 / 结束 / 自己说过话」——这三件事以前
                    // **一个都没喂**（main.rs 里 `Input::TurnStart/TurnEnd/ReplySent` 零调用），
                    // 后果是 `last_ai_reply_at` 永不更新、`schedule_reply_check` 永不触发，
                    // 于是「说完一句、对方接着说」它就不再理人（实测 19:46 回了"这得看你给多少"，
                    // 之后「看图」直接没接，因为那条消息没有 @/名字/问句，追听又没排上）。
                    let turn_at = now_ms() as i64;
                    if let Some(d) = drivers.get_mut(&key) {
                        let _ = d.apply(session::Input::TurnStart { at_ms: turn_at });
                    }
                    let runtime_recent = runtime_recent_by_key
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    let recent = recent_by_key.get(&key).cloned().unwrap_or_default();
                    let dsh_prompt = prompt::build_with_runtime_and_recent(
                        &runtime_context,
                        &key,
                        &reason,
                        &self_nick,
                        &self_id.to_string(),
                        &runtime_recent,
                    );
                    let cli_prompt = prompt::build_text_only_with_runtime(&runtime_context, &key, &recent, &self_nick, &self_id.to_string());
                    pipe.note_turn(&reason, &in_flight);
                    let ok = match pipe.run_turn(&target, &dsh_prompt, &cli_prompt, &cfg_raw).await {
                        Ok(n) => {
                            println!("[her] 回合完成（{n} 字输出）");
                            n > 0
                        }
                        Err(err) => {
                            println!("[her] ❌ 回合失败：{err}");
                            false
                        }
                    };
                    let end_at = now_ms() as i64;
                    if let Some(d) = drivers.get_mut(&key) {
                        // 模型自己发过话（输出非空）→ 安排追听；没说话就只解锁忙碌。
                        if ok {
                            let _ = d.apply(session::Input::ReplySent {
                                at_ms: end_at,
                                text: String::new(),
                            });
                        }
                        let _ = d.apply(session::Input::TurnEnd { at_ms: end_at });
                    }
                }
            }
        }
    }

        // 内层事件循环结束 = 连接断了（或心跳流被关）。等 2 秒重连，别退出进程。
        if let Some(at) = seen.pending() {
            if let Err(error) = journal.touch_seen(at).await {
                println!("[her] ❌ 记录最后存活时刻失败：{error}");
            }
        }
        if std::env::args().any(|a| a == "--no-reconnect") {
            println!("[her] --no-reconnect：不重连，直接退出");
            break 'reconnect;
        }
        gateway_health::with(|h| h.connected = false);
        if got_traffic {
            gateway_backoff.on_traffic();
        }
        let mut wait = gateway_backoff.on_failure(dependency::jitter_unit(now_ms() as i64, u64::from(gateway_backoff.streak())));
        println!("[her] 连接已断开，{} 秒后重连", wait / 1000);
        recovery_log::record(
            &runtime_context,
            run_record::Trigger::Timer,
            run_record::Component::Onebot,
            "disconnected",
            run_record::Outcome::Failed,
            "gateway connection closed; reconnecting with backoff",
        );
        if dependency_watch::adopt_heal(
            &runtime_context, &mut heal_throttle, "close",
            &mut ws, &mut token, &mut heal_http_token, &mut cfg_raw,
        ).await {
            wait = 500;
        }
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    } // 'reconnect

    // 走到这里只有两种情况：`--no-reconnect` 调试模式，或者不可恢复的错误。
    // 计数器（events/wakes）在重连循环内部，这里不再引用。
    println!("[her] 退出（重连循环已结束）");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reserved2_trigger_precedence_matches_frozen_js() {
        let event=ev(true,"needle");
        let cards=HashMap::new();
        let mut tr:Triggers=serde_json::from_value(json!({"anyMessage":true,"atMention":true})).unwrap();
        assert_eq!(decide(&event,"needle",true,"",&cards,&tr,false).1,"anyMessage");
        tr.any_message=false;
        tr.at_mention=false;
        tr.speaker_ids=vec![json!(event.user_id.unwrap())];
        tr.keywords=vec!["needle".into()];
        assert_eq!(decide(&event,"needle",false,"",&cards,&tr,false).1,"keyword:needle");
    }
    #[test]
    fn ingress_segments_accept_numeric_mentions_and_preserve_signed_reply_ids() {
        for id in [json!(-1234), json!("-1234")] {
            let (text, at, reply) = plain_text(&json!([
                {"type":"at","data":{"qq":9001}},
                {"type":"reply","data":{"id":id}},
                {"type":"text","data":{"text":"hello"}}
            ]), 9001);
            assert_eq!((text.as_str(), at, reply), ("hello", true, Some(-1234)));
        }
    }
    #[test]
    fn ingress_voice_segment_reaches_the_model_as_the_baseline_voice_label() {
        // JS segmentsToText: a record segment is "[语音]" (a voice-only message is still a message).
        let (text, at, reply) = plain_text(&json!([{"type":"record","data":{"file":"x.silk"}}]), 9001);
        assert_eq!((text.as_str(), at, reply), ("[语音]", false, None));
        let (text, _, _) = plain_text(&json!([{"type":"text","data":{"text":"听 "}},{"type":"record","data":{}}]), 9001);
        assert_eq!(text, "听 [语音]");
    }
    #[test]
    fn ingress_protocol_identities_accept_strings_and_reject_malformed_present_values() {
        for id in [serde_json::json!(9001), serde_json::json!("9001")] {
            let event: super::Event = serde_json::from_value(serde_json::json!({
                "user_id": id, "self_id": "9002", "group_id": "420042",
                "sender": {"user_id": "9001"}
            })).expect("valid protocol identity");
            assert_eq!(event.user_id, Some(9001));
            assert_eq!(event.sender.unwrap().user_id, Some(9001));
        }
        for field in ["user_id", "self_id", "group_id", "sender_id", "target_id"] {
            for bad in [serde_json::json!(0), serde_json::json!(-1), serde_json::json!("oops"), serde_json::json!(1.5), serde_json::json!(true), serde_json::json!(9007199254740992_i64)] {
                assert!(serde_json::from_value::<super::Event>(serde_json::json!({field: bad})).is_err(), "{field}");
            }
        }
    }
    use super::*;

    #[test]
    fn live_groups_accept_legacy_numeric_and_canonical_session_keys() {
        assert_eq!(
            parse_live_groups(Some("123, group:456, private:789,,")),
            vec![
                "group:123".to_string(),
                "group:456".to_string(),
                "private:789".to_string(),
            ]
        );
        let cfg = json!({"rollout": {"enabled": true, "rustGroups": ["group:123"]}});
        assert!(rollout_mismatch(&cfg, &parse_live_groups(Some("123"))).is_none());
    }

    #[test]
    fn isolated_onebot_http_endpoint_keeps_its_own_loopback_port() {
        assert_eq!(
            loopback_http_endpoint("http://127.0.0.1:43103"),
            Some(("127.0.0.1".to_string(), 43103))
        );
        assert_eq!(
            loopback_http_endpoint("http://[::1]:43104"),
            Some(("[::1]".to_string(), 43104))
        );
        assert!(loopback_http_endpoint("http://192.0.2.1:43103").is_none());
        assert!(loopback_http_endpoint("http://127.0.0.1:43103/path").is_none());
    }

    #[test]
    fn isolated_live_initialization_failure_is_rejected_before_heartbeat() {
        let root = isolated_runtime_fixture("bad-live-session");
        let session_file = root.join("data/sessions.json");
        let corrupt = b"{bad candidate session map";
        std::fs::write(&session_file, corrupt).unwrap();
        let context = runtime::RuntimeContext::from_root(&root)
            .unwrap()
            .with_console_port(43111);
        let cfg = context.load_config().unwrap();
        let result = prepare_live_pipeline(
            &context,
            &cfg,
            true,
            &["group:42".to_string()],
            false,
        );

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("corrupt selected sessions must prevent live pipeline initialization"),
        };
        assert!(error.contains("refusing to publish a live heartbeat"));
        assert_eq!(std::fs::read(&session_file).unwrap(), corrupt);
        assert!(
            !root.join("data/rust-heartbeat-live.json").exists(),
            "candidate initialization failure must not create a live owner heartbeat"
        );
        assert!(!root.join("data/logs/rbridge-live.jsonl").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hotfix128_wrapped_fix1_structured_strings_are_masked_before_serialization() {
        let root = isolated_runtime_fixture("wrapped-log-128");
        std::fs::write(root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        for newline in ["\n", "\r\n"] {
            let record = json!({"message":format!("prefix known-structured- {newline} token-128 suffix"),
                "nested":[{"detail":format!("known-structured-{newline}token-128"),"ok":true}],"count":2});
            append_runtime_json_line(&context, "logs/wrapped128.jsonl", "", &record).unwrap();
        }
        let raw = std::fs::read_to_string(root.join("data/logs/wrapped128.jsonl" )).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let values: Vec<Value> = raw.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values, vec![json!({"message":"prefix *** suffix","nested":[{"detail":"***","ok":true}],"count":2}); 2]);
    }

    #[test]
    fn hotfix128_structured_decision_logs_mask_tokens_with_the_guard_off() {
        let root = isolated_runtime_fixture("token-log-128");
        std::fs::write(root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#).unwrap();
        std::fs::write(root.join("config/config.json"),
            r#"{"consoleToken":"fixture-console-token-128","security":{"outboundGuard":false}}"#).unwrap();
        let context = runtime::RuntimeContext::from_root(&root).unwrap();
        let shape = "0123456789abcdef0123456789abcdef0123456789abcdef";
        append_runtime_json_line(&context, "logs/decision128.jsonl", "", &json!({
            "message": format!("ordinary known-structured-token-128 {shape}"), "count": 2
        })).unwrap();
        let raw = std::fs::read_to_string(root.join("data/logs/decision128.jsonl")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(value, json!({"message":"ordinary *** ***", "count":2}), "{raw}");
    }

    #[test]
    fn hotfix128_fix2_snapshot_debug_and_json_diagnostics_mask_escaped_breaks() {
        const CHILD_ROOT: &str = "HER_FIX2_SNAPSHOT_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let context = runtime::RuntimeContext::from_root(std::path::Path::new(&root)).unwrap();
            activity::set_log_context(&context);
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
                dsh::dump_snapshot(&context.dependency_url("dsh").unwrap(), "synthetic-cookie", "snapshot-fix2")
            ).unwrap();
            let wrapped = context.read_text(runtime::RuntimeArea::Data, "snapshot-text").unwrap();
            let diagnostic = format!("snapshot text={wrapped:?}");
            activity::append(&context, &diagnostic);
            append_runtime_json_line(&context, "logs/snapshot128.jsonl", "", &json!({"snapshot":diagnostic,"count":2})).unwrap();
            return;
        }
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        for separator in ["\n", "\r\n"] {
            let fixture = Fixture(isolated_runtime_fixture("snapshot-fix2"));
            let root = &fixture.0;
            let wrapped = format!("snapshot K128session{separator}VALUExyz end");
            std::fs::write(root.join("data/social-v2.json"), json!({"conversations":{
                "group:42":{"agentToken":"K128sessionVALUExyz"}
            }}).to_string()).unwrap();
            std::fs::write(root.join("data/snapshot-text"), &wrapped).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            std::fs::write(root.join("dependencies.json"), json!({"dsh":base,"model":base,"onebot":base}).to_string()).unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "snapshot client did not connect");
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("snapshot listener: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut socket = tokio_tungstenite::tungstenite::accept(stream).unwrap();
                let open: Value = serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(open["endpoint"], "session/follow");
                let frame = json!({"type":"item","streamId":open["streamId"],"value":{"type":"snapshot","records":[
                    {"event":{"type":"assistant/message","seq":1,"data":{"turn":1,"message":{"content":[{"type":"text","text":wrapped}]}}}},
                    {"event":{"type":"error","seq":2,"data":{"message":wrapped}}}
                ]}});
                socket.send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string())).unwrap();
            });
            // Exercise the real diagnostic function in a fresh process with an explicit context;
            // the legacy --dsh-dump CLI intentionally refuses selected-runtime arguments.
            let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
                .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
                .env(CHILD_ROOT, root).output().unwrap();
            server.join().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("[dump] snapshot") && text.contains("assistant/message") && text.contains("error"), "{text}");
            assert!(!text.contains("K128session") && !text.contains("VALUExyz"), "snapshot leaked: {text}");
            assert_eq!(text.matches("***").count(), 2, "one debug and one JSON mask: {text}");
            for name in ["data/qq-activity.log", "data/logs/snapshot128.jsonl"] {
                let text = std::fs::read_to_string(root.join(name)).unwrap();
                assert!(!text.contains("K128session") && !text.contains("VALUExyz") && text.contains("***"), "{name}: {text}");
            }
            let record: Value = serde_json::from_str(&std::fs::read_to_string(root.join("data/logs/snapshot128.jsonl")).unwrap()).unwrap();
            assert_eq!(record["count"], 2);
        }
    }

    fn isolated_runtime_fixture(tag: &str) -> std::path::PathBuf {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her ticket04 live fixture 中文 {tag} {} {}",
            std::process::id(),
            now_ms()
        )));
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources")).unwrap();
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"fixture-token"}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#)
            .unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43301","model":"http://127.0.0.1:43302","onebot":"http://127.0.0.1:43303"}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn isolated_live_lock_recovers_crashed_marker_without_unlinking_gate() {
        let root = isolated_runtime_fixture("lock-recovery");
        let context = runtime::RuntimeContext::from_root(&root).unwrap();
        let gate = root.join("data/logs/rbridge-live.lock");
        std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
        std::fs::write(&gate, "crashed-owner\n1").unwrap();

        let mut first = acquire_single_instance(&context)
            .expect("the OS-released gate can be recovered immediately");
        assert_eq!(first.marker().identity.pid, std::process::id());
        // Windows byte-range locks prohibit reading through another handle while
        // held. The sidecar is the public ownership witness; check
        // the gate's unchanged bytes after releasing ownership.
        assert!(gate.is_file());
        first.refresh(123).unwrap();
        assert_eq!(first.marker().refreshed_at, 123);
        let marker_path = root.join("data/logs/rbridge-live.lock.owner.json");
        let marker: Value = serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
        assert_eq!(marker["identity"]["pid"], std::process::id());
        assert_eq!(marker["refreshedAt"], 123);
        drop(first);
        assert_eq!(std::fs::read_to_string(&gate).unwrap(), "crashed-owner\n1");

        assert!(gate.is_file(), "the persistent gate is never unlinked on normal drop");
        let second = acquire_single_instance(&context)
            .expect("dropping the old handle releases ownership immediately");
        drop(second);
        assert!(gate.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollout_mismatch_catches_silent_dead_air() {
        // 关键场景：JS 让出、Rust 不服务 = 静默失联。必须被拦住。
        let cfg = json!({"rollout": {"enabled": true, "rustGroups": ["group:1", "group:2"]}});
        let miss = rollout_mismatch(&cfg, &["group:1".to_string()]).expect("应该报缺");
        assert_eq!(miss, vec!["group:2".to_string()]);

        // 两边一致：放行
        let ok = rollout_mismatch(&cfg, &["group:1".to_string(), "group:2".to_string()]);
        assert!(ok.is_none());

        // 没配 rollout：不启用灰度，不该拦
        assert!(rollout_mismatch(&json!({}), &[]).is_none());

        // 明确关掉灰度时，rustGroups 里残留内容不算问题
        let off = json!({"rollout": {"enabled": false, "rustGroups": ["group:9"]}});
        assert!(rollout_mismatch(&off, &[]).is_none());

        // rustGroups 为空 = 还没灰度，谁都不该拦
        let empty = json!({"rollout": {"enabled": true, "rustGroups": []}});
        assert!(rollout_mismatch(&empty, &[]).is_none());
    }

    fn ev(group: bool, text: &str) -> Event {
        Event {
            post_type: "message".into(),
            message_type: if group { "group".into() } else { "private".into() },
            // 私聊事件**没有** group_id —— 这里以前一律给 7002，
            // 于是"私聊一律唤醒"那条用例其实在测一个不存在的形状。
            group_id: if group { Some(7002) } else { None },
            user_id: Some(7001),
            self_id: Some(7004),
            raw_message: text.into(),
            message: serde_json::json!([{ "type": "text", "data": { "text": text } }]),
            ..Default::default()
        }
    }

    fn tr() -> Triggers {
        Triggers {
            at_mention: true,
            name_mention: true,
            keywords: vec!["鲸鱼".into(), "DS".into()],
            question: true,
            ..Default::default()
        }
    }

    #[test]
    fn keyword_short_ascii_needs_boundary() {
        assert!(keyword_hit("ds 说的", "DS"));
        assert!(keyword_hit("ds", "DS"));
        assert!(!keyword_hit("ads", "DS"));
        assert!(!keyword_hit("bdsm", "DS"));
        // 关键：JS 的 \b 只认 ASCII word 字符，汉字算「非 word」→ 有边界
        assert!(keyword_hit("ds好", "DS"), "汉字紧跟短英文关键词时，JS 会命中");
        assert!(keyword_hit("用ds画图", "DS"), "两侧都是汉字时同样命中");
        // 下划线在 JS 里是 word 字符
        assert!(!keyword_hit("ads_", "DS"));
        assert!(!keyword_hit("_ds", "DS"));
    }

    #[test]
    fn keyword_cjk_substring() {
        assert!(keyword_hit("大鲸鱼来了", "鲸鱼"));
        assert!(!keyword_hit("大肥鱼来了", "鲸鱼"));
    }

    /// 事件形状矩阵：把 JS 侧路由分支全列一遍，逐条固定"醒还是不醒"。
    ///
    /// 为什么需要这张表：拍一拍的漏判不是"某一行写错了"，而是**事件形状枚举不全**。
    /// JS 的路由只有四种去处（群消息 / 私聊消息 / 拍一拍 / 其余忽略），
    /// 所以把四种形状 × 常见字段组合都钉住，比读代码更可靠。
    #[test]
    fn event_shape_matrix() {
        let mut t = tr();
        t.poke = true;
        let nicks = HashMap::new();

        let shape = |post: &str, mtype: &str, sub: &str, gid: Option<i64>| Event {
            post_type: post.into(),
            message_type: mtype.into(),
            sub_type: sub.into(),
            group_id: gid,
            user_id: Some(111),
            sender_id: Some(111),
            self_id: Some(7004),
            ..Default::default()
        };

        // ① 群消息（普通聊天）：由触发器决定，这里用无触发的正文
        let (w, why) = decide(&shape("message", "group", "", Some(1)), "嗯", false, "Fixture Bot", &nicks, &t, false);
        assert!(!w, "群消息无触发应不醒");
        assert_eq!(why, "no-trigger");

        // ② 群消息 + 名字：醒
        let (w2, why2) = decide(&shape("message", "group", "", Some(1)), "Fixture Bot在吗", false, "Fixture Bot", &nicks, &t, false);
        assert!(w2);
        assert_eq!(why2, "nameMention");

        // ③ 私聊消息：一律醒，reason=private
        let (w3, why3) = decide(&shape("message", "private", "", None), "随便说点", false, "Fixture Bot", &nicks, &t, false);
        assert!(w3);
        assert_eq!(why3, "private");

        // ④ 群拍一拍（无 message_type）：醒，reason=poke
        let (w4, why4) = decide(&shape("notice", "", "poke", Some(1)), "", false, "Fixture Bot", &nicks, &t, false);
        assert!(w4, "群拍一拍必须醒");
        assert_eq!(why4, "poke");

        // ⑤ 私聊拍一拍（无 group_id）：同样醒（JS 也走同一条 handlePokeNotice）
        let (w5, why5) = decide(&shape("notice", "", "poke", None), "", false, "Fixture Bot", &nicks, &t, false);
        assert!(w5, "私聊拍一拍也应醒");
        assert_eq!(why5, "poke");

        // ⑥ 其它 notice（群撤回/入群等）：忽略
        let (w6, why6) = decide(&shape("notice", "", "group_recall", Some(1)), "", false, "Fixture Bot", &nicks, &t, false);
        assert!(!w6, "非拍一拍的 notice 不该醒");
        assert_eq!(why6, "no-trigger");

        // ⑦ meta 心跳：忽略
        let (w7, why7) = decide(&shape("meta_event", "", "", None), "", false, "Fixture Bot", &nicks, &t, false);
        assert!(!w7);
        assert_eq!(why7, "no-trigger");
    }

    #[test]
    fn group_poke_notice_wakes() {
        // 群里别人拍别人（含拍 owner）也算一次唤醒 —— JS 的 handlePokeNotice 不区分拍谁，
        // 只要 triggers.poke 打开、且不是自己拍的。
        //
        // 回归的正是红线漏判：notice 事件里 message_type 是**空的**，只有 group_id。
        // 早先 `is_group` 只看 message_type，于是群里拍一拍被判成"私聊的非消息事件"，
        // 直接 no-trigger（实测 07:59:15 group:7003 漏了一条 poke）。
        let e = Event {
            post_type: "notice".into(),
            message_type: String::new(),
            sub_type: "poke".into(),
            group_id: Some(7003),
            sender_id: Some(111),
            target_id: Some(7001),
            self_id: Some(7004),
            ..Default::default()
        };
        let mut t = tr();
        t.poke = true;
        let (wake, why) = decide(&e, "", false, "Fixture Bot", &HashMap::new(), &t, false);
        assert!(wake, "群里拍一拍应该唤醒");
        assert_eq!(why, "poke");

        // 自己拍别人不算（否则机器人会回应自己的动作）
        let mut me = Event::default();
        me.post_type = "notice".into();
        me.sub_type = "poke".into();
        me.group_id = Some(7003);
        me.sender_id = Some(7004);
        me.target_id = Some(7001);
        me.self_id = Some(7004);
        let (wake2, why2) = decide(&me, "", false, "Fixture Bot", &HashMap::new(), &t, false);
        assert!(!wake2);
        assert_eq!(why2, "poke:selfskip");

        // 没开 poke 触发器时不醒
        let mut off = tr();
        off.poke = false;
        let (wake3, _) = decide(&e, "", false, "Fixture Bot", &HashMap::new(), &off, false);
        assert!(!wake3);
    }

    #[test]
    fn name_mention_hits() {
        let e = ev(true, "Fixture Bot你在吗");
        let (wake, why) = decide(&e, "Fixture Bot你在吗", false, "Fixture Bot", &HashMap::new(), &tr(), false);
        assert!(wake);
        assert_eq!(why, "nameMention");
    }

    #[test]
    fn question_requires_direction_and_the_existing_switch() {
        let e = ev(true, "你有空吗？");
        assert!(!decide(&e,"你有空吗？",false,"Fixture Bot",&HashMap::new(),&tr(),false).0);
        let (wake,why)=decide(&e,"你有空吗？",false,"Fixture Bot",&HashMap::new(),&tr(),true);
        assert!(wake);
        assert_eq!(why,"question");
        let mut off=tr(); off.question=false;
        assert!(!decide(&e,"你有空吗？",false,"Fixture Bot",&HashMap::new(),&off,true).0);
    }

    #[test]
    fn plain_group_message_does_not_wake() {
        let e = ev(true, "今天天气不错");
        let (wake, why) = decide(&e, "今天天气不错", false, "Fixture Bot", &HashMap::new(), &tr(), false);
        assert!(!wake);
        assert_eq!(why, "no-trigger");
    }

    #[test]
    fn private_always_wakes() {
        let e = ev(false, "嗯");
        let (wake, why) = decide(&e, "嗯", false, "Fixture Bot", &HashMap::new(), &tr(), false);
        assert!(wake);
        assert_eq!(why, "private", "JS 的私聊唤醒原因就是 private，没有 owner 后缀");
    }

    #[test]
    fn group_card_counts_as_name() {
        let e = ev(true, "大肥鱼出来");
        let mut cards = HashMap::new();
        cards.insert(7002i64, "大肥鱼".to_string());
        let (wake, why) = decide(&e, "大肥鱼出来", false, "Fixture Bot", &cards, &tr(), false);
        assert!(wake);
        assert_eq!(why, "nameMention");
    }

    #[test]
    fn at_self_wins() {
        let e = ev(true, "随便说说");
        let (wake, why) = decide(&e, "随便说说", true, "Fixture Bot", &HashMap::new(), &tr(), false);
        assert!(wake);
        assert_eq!(why, "atMention");
    }
}

