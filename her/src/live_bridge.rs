//! Live QQ gateway bridge: startup/lock/heartbeat, WebSocket reconnect loop, inbound replay
//! and held-event gating, OneBot event/wake trigger evaluation, and unified Second-generation
//! simulation (`reserved2`) wake orchestration (`reserved2::Hub` for isolated runtimes and
//! fallback drivers for legacy/observation runs).

#[path = "reserved2.rs"]
pub(crate) mod reserved2;

use crate::{
    activity, chat, closed_agent, conversation_state, dependency, dependency_watch, dsh_profile,
    gateway_health, group_mute, harness, history_log, inbound, ledger, lifecycle, local_time,
    onebot, parity_clock, persona_command, pipeline, pricing, prompt, question, recovery_log,
    release, reserved, run_record, runtime, session, session_map, slang_learn, snowluma_heal,
    social_state, turn_recovery, wake, wake_words, watchdog,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const CONFIG_PATH: &str = crate::legacy_path!("qq-bridge/config.json");
const DECISION_NAME: &str = "logs/rbridge-live.jsonl";
const DECISION_LOG: &str = crate::legacy_path!("logs/rbridge-live.jsonl");
/// Wake firing timestamps (after batching/scheduling delays) for live/replay diagnostics.
const WAKE_NAME: &str = "logs/rbridge-live-wakes.jsonl";
const WAKE_LOG: &str = crate::legacy_path!("logs/rbridge-live-wakes.jsonl");

#[derive(Deserialize, Debug, Default)]
struct Config {
    #[serde(default)]
    snowluma: Option<SnowLuma>,
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

#[derive(Deserialize, Debug, Default, Clone)]
struct SocialV2 {
    #[serde(default)]
    social: SocialTuning,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct SocialTuning {
    #[serde(rename = "skipProbability", default)]
    skip_probability: f64,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub(crate) struct Triggers {
    #[serde(rename = "atMention", default)]
    pub(crate) at_mention: bool,
    #[serde(rename = "nameMention", default)]
    pub(crate) name_mention: bool,
    #[serde(default)]
    pub(crate) keywords: Vec<String>,
    /// Persona names she turned off (`wake_words.rs`).
    #[serde(rename = "dropNames", default)]
    pub(crate) drop_names: Vec<String>,
    #[serde(default)]
    pub(crate) question: bool,
    #[serde(default)]
    pub(crate) poke: bool,
    #[serde(rename = "speakerIds", default)]
    pub(crate) speaker_ids: Vec<Value>,
    #[serde(rename = "anyMessage", default)]
    pub(crate) any_message: bool,
    #[serde(default)]
    pub(crate) probability: f64,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct WakeConfig {
    #[serde(default)]
    triggers: Triggers,
}

#[derive(Deserialize, Debug, Default)]
pub(crate) struct Event {
    #[serde(default)]
    pub(crate) post_type: String,
    #[serde(default)]
    pub(crate) message_type: String,
    #[serde(default)]
    pub(crate) sub_type: String,
    /// Lenient like the other mute fields: a null or non-string value must not drop the whole frame.
    #[serde(default)]
    pub(crate) notice_type: Option<Value>,
    /// `group_ban` operator. Lenient: a malformed value only loses the name in the owner notice.
    #[serde(default)]
    pub(crate) operator_id: Option<Value>,
    /// `group_ban` duration in seconds. Lenient: an unreadable value means the end is unknown.
    #[serde(default)]
    pub(crate) duration: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) group_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) user_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) target_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) sender_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) self_id: Option<i64>,
    #[serde(default)]
    pub(crate) raw_message: String,
    #[serde(default)]
    pub(crate) message_id: Option<Value>,
    #[serde(default)]
    pub(crate) msg_id: Option<Value>,
    #[serde(default)]
    pub(crate) message: Value,
    #[serde(default)]
    pub(crate) meta_event_type: String,
    /// A heartbeat's `status` (`online`, `good`): SnowLuma's own verdict on its receive chain.
    #[serde(default)]
    pub(crate) status: Option<Value>,
    #[serde(default)]
    pub(crate) echo: Option<Value>,
    #[serde(default)]
    pub(crate) data: Option<Value>,
    /// OneBot sender metadata (`card`, `nickname`, fallback `user_id`) for `speaker:<label>` wake reasons.
    #[serde(default)]
    pub(crate) sender: Option<SendingUser>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub(crate) struct SendingUser {
    #[serde(default, deserialize_with = "deserialize_account_id")]
    pub(crate) user_id: Option<i64>,
    #[serde(default)]
    pub(crate) card: String,
    #[serde(default)]
    pub(crate) nickname: String,
}

// OneBot accepts JSON integers and decimal strings. Malformed present identities fail closed.
pub(crate) fn protocol_integer(value: &Value) -> Option<i64> {
    let id = match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }?;
    (-9_007_199_254_740_991..=9_007_199_254_740_991)
        .contains(&id)
        .then_some(id)
}

fn deserialize_account_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    value
        .map(|v| {
            protocol_integer(&v)
                .filter(|id| *id > 0)
                .ok_or_else(|| serde::de::Error::custom("invalid OneBot account ID"))
        })
        .transpose()
}

/// Deterministic per-message/sender seed so probability rolls remain reproducible across replays.
pub(crate) fn text_seed(text: &str, user: Option<i64>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    user.hash(&mut h);
    h.finish()
}

pub(crate) fn event_message_id(event: &Event) -> Option<String> {
    let value = event.message_id.as_ref().or(event.msg_id.as_ref())?;
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// Conversations listed in `rollout.rustGroups` that are missing from `--live-groups`.
///
/// Returns `Some(missing)` when rollout is active and the two disagree; `None` when aligned or
/// when `rollout.enabled === false`.
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

/// #186: say at live startup where `rollout.rustGroups` (fixed into `--live-groups` at launch) and the allow/deny
/// lists disagree, one line per kind of gap, on stdout and in the run record. Returns the lines it reported.
fn report_rollout_scope(context: &runtime::RuntimeContext, cfg: &Value) -> Vec<String> {
    let lines = watchdog::rollout_scope_gaps(cfg).lines();
    for line in &lines {
        println!("[her] ⚠️ {line}");
        recovery_log::record(
            context,
            run_record::Trigger::Startup,
            run_record::Component::Process,
            "rollout-scope",
            run_record::Outcome::Failed,
            line,
        );
    }
    lines
}

/// #186: the saved inbound events replayed at a (re)connect, split by the configuration read for that connection:
/// admitted conversations replay, the other events are settled. Without a readable configuration nothing is decided
/// and every event stays saved for the next connection.
fn split_replayable(
    cfg: Option<&Value>,
    items: Vec<inbound::Replayable>,
) -> (Vec<inbound::Replayable>, Vec<String>) {
    let Some(cfg) = cfg else {
        return (Vec::new(), Vec::new());
    };
    let mut replay = Vec::new();
    let mut settle = Vec::new();
    for item in items {
        if social_state::inbound_admitted(cfg, &item.conversation) {
            replay.push(item);
        } else {
            settle.push(item.event_key);
        }
    }
    (replay, settle)
}

/// #186: the conversations whose disconnect gaps a (re)connect records (`(groups, privates)` ids): those the
/// configuration read now admits or, when it cannot be read, those of the last readable configuration.
fn gap_lists(
    read: Result<Value, &'static str>,
    last: &(Vec<String>, Vec<String>),
) -> (Vec<String>, Vec<String>) {
    match read {
        Ok(cfg) => (
            social_state::admitted_ids(&cfg, "group"),
            social_state::admitted_ids(&cfg, "private"),
        ),
        Err(error) => {
            println!("[her] ❌ 读取配置失败，本次连接按上次读到的允许列表记录断线缺口：{error}");
            last.clone()
        }
    }
}

/// #192 (F3): how many held events (saved while the configuration could not be read) are kept in memory for replay
/// once it reads again; a later one stays saved and replays at the next connection that reads the configuration.
const HELD_LIMIT: usize = 10_000;

/// #192 (F3): the conversation a message or poke belongs to, named as the reserved2 hub names it (`group:<id>` or
/// `private:<id>`); `None` for every other frame and for a frame without that id.
fn held_conversation(ev: &Event) -> Option<String> {
    match (ev.post_type.as_str(), ev.message_type.as_str()) {
        ("message", "group") => ev.group_id.map(|id| format!("group:{id}")),
        ("message", "private") => ev.user_id.map(|id| format!("private:{id}")),
        ("notice", _) if ev.sub_type == "poke" => {
            ev.group_id.map(|id| format!("group:{id}")).or_else(|| {
                ev.user_id
                    .or(ev.sender_id)
                    .or_else(|| ev.sender.as_ref().and_then(|s| s.user_id))
                    .map(|id| format!("private:{id}"))
            })
        }
        _ => None,
    }
}

/// #192 (F3): remember a saved event (its journal key) for replay once the configuration reads again. False when
/// `HELD_LIMIT` events are already held: that one stays saved for the next connection's replay.
fn hold(held: &mut Vec<String>, event_key: String) -> bool {
    if held.contains(&event_key) {
        return true;
    }
    if held.len() >= HELD_LIMIT {
        return false;
    }
    held.push(event_key);
    true
}

/// #192 (F3): the held events among the saved ones, in saved order, split by the configuration that reads again:
/// admitted conversations replay, the others are settled.
fn take_held(
    cfg: &Value,
    held: &[String],
    saved: Vec<inbound::Replayable>,
) -> (Vec<inbound::Replayable>, Vec<String>) {
    let held: std::collections::HashSet<&str> = held.iter().map(String::as_str).collect();
    let items = saved
        .into_iter()
        .filter(|item| held.contains(item.event_key.as_str()))
        .collect();
    split_replayable(Some(cfg), items)
}

/// #192 (F3): what the ingress loop does with one event. `readable`: the configuration read for it succeeded; `holds`:
/// reserved2 serves its conversation here (`--live-groups`); `held_any`: events are held already; `refusable`: another
/// mode would serve it. A served event is held while the configuration cannot be read, and also while earlier events
/// are still held, so it never overtakes them once the configuration reads again.
#[derive(Debug, PartialEq, Eq)]
enum Ingress {
    Handle,
    Hold,
    Refuse,
    Skip,
}

fn ingress_for(readable: bool, holds: bool, held_any: bool, refusable: bool) -> Ingress {
    if holds && (!readable || held_any) {
        Ingress::Hold
    } else if readable {
        Ingress::Handle
    } else if refusable {
        Ingress::Refuse
    } else {
        Ingress::Skip
    }
}

/// #192 (F3): whether this process's reserved2 hub serves `key`, by the rule of `Hub::taken` (reserved2.rs): every
/// conversation with `--live-all` (the flag the hub is built with), otherwise those listed in `--live-groups`.
fn reserved2_serves(live_all: bool, live_groups: &[String], key: &str) -> bool {
    !key.is_empty() && (live_all || live_groups.iter().any(|g| g == key))
}

/// #192 (F3): events refused while the configuration cannot be read (outside reserved2, which holds them) go to the
/// run record at most once a minute, also across a recovery, with the number refused since the previous record.
#[derive(Default)]
struct UnreadableRefusals {
    count: u64,
    recorded_at: Option<std::time::Instant>,
}

impl UnreadableRefusals {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(60);

    /// One more refused event; the number to record now, when a record is due.
    fn note(&mut self, now: std::time::Instant) -> Option<u64> {
        self.count += 1;
        self.due(now)
    }

    /// The configuration reads again: the number refused since the last record, once a record is due (the ingress
    /// loop asks again until it is recorded).
    fn flush(&mut self, now: std::time::Instant) -> Option<u64> {
        self.due(now)
    }

    fn due(&mut self, now: std::time::Instant) -> Option<u64> {
        if self.count == 0
            || self
                .recorded_at
                .is_some_and(|at| now.saturating_duration_since(at) < Self::EVERY)
        {
            return None;
        }
        self.recorded_at = Some(now);
        Some(std::mem::take(&mut self.count))
    }
}

/// #192 (F3): one run-record line for `count` events refused while the configuration could not be read.
fn record_unreadable_refusals(context: &runtime::RuntimeContext, count: u64) {
    recovery_log::record(
        context,
        run_record::Trigger::Turn,
        run_record::Component::Queue,
        "config-unreadable",
        run_record::Outcome::Failed,
        &format!("{count} inbound event(s) not processed: the configuration could not be read"),
    );
}

/// Wake configurations written to the state file by a tool or the console (`confirmedAt` changed) reach the
/// running state machine of the conversation. `synced` remembers the last `session::WakeSync` seen per conversation.
pub(crate) fn sync_wake_configs(
    context: &runtime::RuntimeContext,
    cfg: &Value,
    state: &Value,
    drivers: &mut HashMap<String, session::Driver>,
    synced: &mut HashMap<String, session::WakeSync>,
    now: i64,
) {
    // Finite sleeps must work after restart even before the first inbound event.
    // The live reserved2 hub owns general driver creation and turn lifecycle.
    let current_cfg = if context.is_isolated() {
        context.load_config().ok()
    } else {
        Some(cfg.clone())
    };
    if let Some(cfg) = current_cfg.as_ref().filter(|cfg| {
        cfg.get("socialV2")
            .and_then(|s| s.get("enabled"))
            .and_then(Value::as_bool)
            != Some(false)
            && state.get("paused").and_then(Value::as_bool) != Some(true)
            && social_state::mode(context) == "reserved2"
    }) {
        if let Some(conversations) = state.get("conversations").and_then(Value::as_object) {
            for (key, conversation) in conversations {
                let finite = conversation.get("wakeConfig").is_some_and(|wc| {
                    wc.get("infinite").and_then(Value::as_bool) == Some(false)
                        && wc
                            .get("sleepUntil")
                            .and_then(Value::as_str)
                            .and_then(wake::parse_iso_ms)
                            .is_some()
                });
                if finite && social_state::inbound_admitted(cfg, key) {
                    drivers.entry(key.clone()).or_insert_with(|| {
                        session::Driver::new(
                            session::scheduler_from_state(wake_cfg_of(cfg), conversation),
                            text_seed(key, None),
                        )
                    });
                }
            }
        }
    }
    for (key, driver) in drivers.iter_mut() {
        let Some(wc) = state
            .get("conversations")
            .and_then(|c| c.get(key))
            .and_then(|c| c.get("wakeConfig"))
        else {
            continue;
        };
        if let Some(next) = session::WakeSync::changed(synced.get(key), wc) {
            let announce = synced.contains_key(key);
            driver.adopt_wake_config(session::wake_config_from_json(Some(wc)), now);
            synced.insert(key.clone(), next);
            if announce {
                println!(
                    "[her] {key} 唤醒配置已更新（confirmedBy {}）",
                    wc.get("confirmedBy").and_then(|v| v.as_str()).unwrap_or("?")
                );
            }
        }
    }
}

pub(crate) fn now_ms() -> u128 {
    crate::parity_clock::now_ms() as u128
}

pub(crate) fn plain_text(msg: &Value, self_id: i64) -> (String, bool, Option<i64>) {
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
                let qq = data
                    .get("qq")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => protocol_integer(v)
                            .filter(|id| *id > 0)
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                    })
                    .unwrap_or_default();
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

/// ASCII word character (`[A-Za-z0-9_]`) used for short-ASCII keyword boundaries so Han characters
/// adjacent to short ASCII tokens (e.g. `ds好`) still count as word boundaries (pinned by
/// `keyword_short_ascii_needs_boundary`).
fn js_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn keyword_hit(text_lower: &str, kw: &str) -> bool {
    let k = kw.to_lowercase();
    if k.is_empty() {
        return false;
    }
    // Pure-ASCII keywords of length <= 4 require ASCII word boundaries so substrings like
    // `ads`/`bdsm` do not fire (pinned by `keyword_short_ascii_needs_boundary`).
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
pub(crate) fn names_self(
    ev: &Event,
    lower: &str,
    self_nick: &str,
    self_cards: &HashMap<i64, String>,
) -> bool {
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

/// Evaluate whether an inbound OneBot frame wakes a reserved2 conversation and with which reason.
///
/// Precedence and shapes (pinned by `event_shape_matrix`, `group_poke_notice_wakes`,
/// `private_always_wakes`, and `reserved2_trigger_precedence_matches_frozen_js`):
/// 1. `notice` with `sub_type == "poke"` (group or private, since pokes lack `message_type`)
///    wakes as `"poke"` when `tr.poke` is enabled and the sender is not `self_id` (else `"poke:selfskip"`).
/// 2. Non-group `message` events always wake as `"private"`; other non-message frames do not wake.
/// 3. Group `message` events check `anyMessage`, `atMention`, `nameMention`, `keywords`,
///    directed `question`, `speaker:<card||nickname||userId>`, and deterministic `probability`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide(
    ev: &Event,
    text: &str,
    at_self: bool,
    self_nick: &str,
    self_cards: &HashMap<i64, String>,
    tr: &Triggers,
    question_directed: bool,
) -> (bool, String) {
    let lower = text.to_lowercase();
    // Group pokes carry `group_id` with an empty `message_type`, so check both fields.
    let is_group = ev.message_type == "group" || ev.group_id.is_some();

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
    // Non-message events other than pokes never enter group trigger evaluation.
    if ev.post_type != "message" {
        return (false, "no-trigger".into());
    }
    if tr.any_message {
        return (true, "anyMessage".into());
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
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Deterministic per `(text, user_id)` so replay and offline regression remain reproducible.
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
    let dir = std::path::Path::new(crate::legacy_path!("SnowLuma/config"));
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("onebot_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
            continue;
        };
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
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
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

pub(crate) fn append_runtime_json_line(
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
            .map_err(std::io::Error::other)?;
    } else {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(legacy_path)?;
        file.write_all(&line)?;
    }
    Ok(())
}

/// Extract wake scheduling parameters from `config.json` (shared across live bridge and tests).
pub(crate) fn wake_cfg_of(cfg_raw: &Value) -> wake::WakeCfg {
    use crate::settings::{reply, wake_timing as timing};
    wake::WakeCfg {
        zone: local_time::configured_zone(cfg_raw),
        turn_hard_cap_ms: wake::turn_budget(cfg_raw).hard_cap_ms,
        turn_ceiling_ms: wake::turn_ceiling_ms(cfg_raw),
        enabled: cfg_raw.pointer("/socialV2/enabled") != Some(&Value::Bool(false)),
        reply_wake_quiet_ms: Some(reply::WAKE_QUIET.integer(cfg_raw)),
        preempt_after_ms: timing::PREEMPT.integer(cfg_raw),
        batch_window_ms: timing::BATCH.integer(cfg_raw),
        auto_reply_check_ms: cfg_raw
            .pointer("/socialV2/autoReplyCheckMs")
            .and_then(Value::as_i64)
            .unwrap_or(30_000),
        max_wake_per_minute: timing::RATE_MIN.integer(cfg_raw),
        max_wake_per_hour: timing::RATE_HOUR.integer(cfg_raw),
        proactive_enabled: timing::PROACTIVE_ENABLED.switch(cfg_raw),
        proactive_min_ms: timing::PROACTIVE_MIN.integer(cfg_raw),
        proactive_max_ms: timing::PROACTIVE_MAX.integer(cfg_raw),
        proactive_idle_ms: timing::PROACTIVE_IDLE.integer(cfg_raw),
        proactive_probability: timing::PROACTIVE_PROBABILITY.probability(cfg_raw),
        ..wake::WakeCfg::default()
    }
}

/// The OS-held owner gate, not the age or contents of a lock file, decides ownership.
fn acquire_single_instance(context: &runtime::RuntimeContext) -> Option<lifecycle::OwnerGuard> {
    let relative = "logs/rbridge-live.lock";
    let path = if context.is_isolated() {
        context
            .resolve_path(
                runtime::RuntimeArea::Data,
                relative,
                runtime::PathIntent::WriteFile,
            )
            .map_err(|e| println!("[her] owner path unavailable: {e}"))
            .ok()?
    } else {
        std::path::PathBuf::from(format!("{}/{relative}", crate::legacy_path!()))
    };
    let identity = lifecycle::current_identity()
        .map_err(|e| println!("[her] identity unavailable: {e}"))
        .ok()?;
    let groups = {
        let args: Vec<String> = std::env::args().collect();
        Some(parse_live_groups(
            args.iter()
                .position(|a| a == "--live-groups")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str),
        ))
    };
    match lifecycle::OwnerGate::new(path).acquire(
        "live",
        identity,
        groups,
        now_ms() as i64,
        std::time::Duration::from_millis(500),
    ) {
        lifecycle::Acquire::Acquired(owner) => Some(owner),
        other => {
            println!("[her] owner unavailable: {other:?}");
            None
        }
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

type LiveStartup = (Vec<String>, bool, Option<pipeline::Pipeline>);

fn prepare_live_startup(
    context: &runtime::RuntimeContext,
    cfg_raw: &Value,
    args: &[String],
) -> Result<LiveStartup, Box<dyn std::error::Error>> {
    let live_groups = args
        .iter()
        .position(|arg| arg == "--live-groups")
        .and_then(|index| args.get(index + 1))
        .map(String::as_str);
    let live_groups = parse_live_groups(live_groups);
    if let Some(missing) = rollout_mismatch(cfg_raw, &live_groups) {
        println!("[her] ❌ 灰度配置不一致，拒绝启动：");
        println!("           config.json 的 rollout.rustGroups 里有 {missing:?}");
        println!("           命令行 --live-groups = {live_groups:?}");
        println!("           这些会话会被 JS 让出、Rust 又不服务 —— 等于静默失联。");
        println!("           要么把它们加到 --live-groups，要么从 rollout.rustGroups 里去掉。");
        std::process::exit(2);
    }
    let live_all = args.iter().any(|a| a == "--live-all");
    let pipeline = prepare_live_pipeline(context, cfg_raw, true, &live_groups, live_all)
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    Ok((live_groups, live_all, pipeline))
}

fn publish_live_heartbeat(
    context: &runtime::RuntimeContext,
    name: &str,
    groups: &[String],
    gateway_connected: bool,
) {
    let state = if gateway_connected {
        "ready"
    } else {
        "waiting-for-gateway"
    };
    let payload = json!({
        "ts": now_ms(),
        "pid": std::process::id(),
        "live": !groups.is_empty(),
        "groups": groups,
        "state": state,
        "gatewayConnected": gateway_connected,
    });
    if context.is_isolated() {
        let _ = context.write_atomic(
            runtime::RuntimeArea::Data,
            name,
            payload.to_string().as_bytes(),
        );
    } else {
        let _ = std::fs::write(
            format!("{}/qq-bridge/state/{name}", crate::legacy_path!()),
            payload.to_string(),
        );
    }
}

fn set_gateway_connected(tx: &tokio::sync::watch::Sender<bool>, ready: bool) {
    tx.send_if_modified(|connected| {
        if *connected == ready {
            false
        } else {
            *connected = ready;
            true
        }
    });
}

/// Feed one group message to the slang window and, when due, start an extraction in the background.
pub(crate) fn feed_slang(
    learning: &slang_learn::SlangLearning,
    auto: bool,
    cfg_raw: &Value,
    key: &str,
    sender: &str,
    body: &str,
) {
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

/// Query via OneBot `get_msg` whether the replied-to message was sent by `self_id`.
fn fetch_msg_sender_is_self(ob: &onebot::OneBot, msg_id: i64, self_id: i64) -> bool {
    let Some(v) = ob.request_with_timeout(
        "get_msg",
        json!({ "message_id": msg_id }),
        std::time::Duration::from_secs(8),
    ) else {
        return false;
    };
    let sender = v
        .get("data")
        .and_then(|d| d.get("sender"))
        .and_then(|s| s.get("user_id"))
        .and_then(protocol_integer)
        .filter(|id| *id > 0)
        .or_else(|| {
            v.get("data")
                .and_then(|d| d.get("user_id"))
                .and_then(protocol_integer)
                .filter(|id| *id > 0)
        });
    sender == Some(self_id)
}

/// Query our group card in one group over OneBot HTTP (WS may not implement this action).
fn fetch_group_card(ob: &onebot::OneBot, gid: i64, self_id: i64) -> Option<String> {
    let v = ob.request_with_timeout(
        "get_group_member_info",
        json!({ "group_id": gid, "user_id": self_id, "no_cache": false }),
        std::time::Duration::from_secs(8),
    )?;
    v.get("data")?
        .get("card")?
        .as_str()
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
}

/// Parse `private:123` / `group:456` target strings into `onebot::Target`.
pub(crate) fn parse_target(
    spec: &str,
    cfg: &serde_json::Value,
) -> Result<onebot::Target, Box<dyn std::error::Error>> {
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

/// Unified Second-generation simulation (`reserved2`) wake orchestration inside the live bridge:
/// `Isolated` delegates to `reserved2::Hub` for selected runtimes, while `Fallback` owns the
/// legacy/observation wake drivers and per-connection turn scheduling.
enum SimulationBridge {
    Isolated {
        hub: Box<reserved2::Hub>,
        events: u64,
    },
    Fallback(Box<FallbackWakeDrivers>),
}

struct FallbackWakeDrivers {
    journal: inbound::SharedJournal,
    live_groups: Vec<String>,
    shadow_flag: bool,
    drivers: HashMap<String, session::Driver>,
    wake_synced: HashMap<String, session::WakeSync>,
    awaiting: HashMap<String, Vec<String>>,
    in_flight: Vec<String>,
    pending_live: Vec<(String, String)>,
    recent_by_key: HashMap<String, Vec<String>>,
    runtime_recent_by_key: HashMap<String, Vec<prompt::RuntimeRecentMessage>>,
    events: u64,
    wakes: u64,
}

impl FallbackWakeDrivers {
    fn new(journal: inbound::SharedJournal, live_groups: Vec<String>) -> Self {
        let shadow_flag = live_groups.is_empty();
        Self {
            journal,
            live_groups,
            shadow_flag,
            drivers: HashMap::new(),
            wake_synced: HashMap::new(),
            awaiting: HashMap::new(),
            in_flight: Vec::new(),
            pending_live: Vec::new(),
            recent_by_key: HashMap::new(),
            runtime_recent_by_key: HashMap::new(),
            events: 0,
            wakes: 0,
        }
    }

    fn reset_connection(&mut self) {
        self.drivers.clear();
        self.wake_synced.clear();
        self.awaiting.clear();
        self.in_flight.clear();
        self.pending_live.clear();
        self.recent_by_key.clear();
        self.runtime_recent_by_key.clear();
        self.events = 0;
        self.wakes = 0;
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_turn(
        &mut self,
        pipe: &mut pipeline::Pipeline,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        key: &str,
        reason: &str,
        self_nick: &str,
        self_id: i64,
        track_driver_lifecycle: bool,
    ) {
        let Ok(target) = parse_target(key, cfg_raw) else {
            println!("[her] {key} 的 target 解析失败，跳过");
            return;
        };
        if track_driver_lifecycle {
            let turn_at = now_ms() as i64;
            if let Some(d) = self.drivers.get_mut(key) {
                let _ = d.apply(session::Input::TurnStart { at_ms: turn_at });
            }
        }
        let runtime_recent = self
            .runtime_recent_by_key
            .get(key)
            .cloned()
            .unwrap_or_default();
        let recent = self.recent_by_key.get(key).cloned().unwrap_or_default();
        let dsh_prompt = prompt::build_with_runtime_and_recent(
            context,
            key,
            reason,
            self_nick,
            &self_id.to_string(),
            &runtime_recent,
        );
        let cli_prompt = prompt::build_text_only_with_runtime(
            context,
            key,
            &recent,
            self_nick,
            &self_id.to_string(),
        );
        pipe.note_turn(reason, &self.in_flight);
        let outcome = pipe.run_turn(&target, &dsh_prompt, &cli_prompt, cfg_raw).await;
        if track_driver_lifecycle {
            let spoke = match outcome {
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
            if let Some(d) = self.drivers.get_mut(key) {
                if spoke {
                    let _ = d.apply(session::Input::ReplySent {
                        at_ms: end_at,
                        text: String::new(),
                    });
                }
                let _ = d.apply(session::Input::TurnEnd { at_ms: end_at });
            }
        } else {
            match outcome {
                Ok(n) => println!("[her] 回合完成（{n} 字输出）"),
                Err(e) => {
                    println!("[her] ❌ 回合失败：{e}");
                    // A held wake never started a turn; clear pending busy mark so future wakes are not suppressed.
                    if e.starts_with(pipeline::HELD_ERROR_PREFIX) {
                        if let Some(d) = self.drivers.get_mut(key) {
                            d.sched.pending_wake_key_at = None;
                        }
                    }
                }
            }
        }
    }

    async fn flush_pending(
        &mut self,
        mut pipeline: Option<&mut pipeline::Pipeline>,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        self_nick: &str,
        self_id: i64,
    ) {
        for (key, reason) in std::mem::take(&mut self.pending_live) {
            if let Some(pipe) = pipeline.as_deref_mut() {
                println!("[her] 定时唤醒 {key}（{reason}）：执行 live 回合");
                self.execute_turn(pipe, context, cfg_raw, &key, &reason, self_nick, self_id, false)
                    .await;
            }
        }
    }

    async fn finish_in_flight(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.in_flight.is_empty() {
            let keys = std::mem::take(&mut self.in_flight);
            self.journal.mark_handled(keys, now_ms() as i64).await?;
        }
        Ok(())
    }

    async fn tick(
        &mut self,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        tick_now: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(state) = conversation_state::read_document_with_bom(context) {
            sync_wake_configs(
                context,
                cfg_raw,
                &state,
                &mut self.drivers,
                &mut self.wake_synced,
                tick_now,
            );
        }
        for (k, e) in session::pump_all(&mut self.drivers, tick_now) {
            inbound::start_processing(
                &self.journal,
                &mut self.awaiting,
                &mut self.in_flight,
                &k,
                now_ms() as i64,
            )
            .await?;
            self.wakes += 1;
            println!("[her] 定时唤醒 {k} ({})", e.reason);
            append_runtime_json_line(
                context,
                WAKE_NAME,
                WAKE_LOG,
                &json!({
                    "ts": e.at_ms,
                    "key": k,
                    "reason": e.reason,
                    "pid": std::process::id(),
                    "shadow": self.shadow_flag
                }),
            )?;
            if self.live_groups.iter().any(|g| g == &k) {
                self.pending_live.push((k, e.reason));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_event(
        &mut self,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        ev: &Event,
        text: &str,
        replayed: bool,
        self_id: i64,
        self_nick: &str,
        self_cards: &HashMap<i64, String>,
        http_ob: &onebot::OneBot,
        skip_probability: f64,
        slang_enabled: bool,
        slang_learning: &slang_learn::SlangLearning,
        mut pipeline: Option<&mut pipeline::Pipeline>,
        is_message: bool,
        is_notice: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let key = if is_message && ev.message_type == "private" {
            format!("private:{}", ev.user_id.unwrap_or(0))
        } else if is_notice && ev.sub_type == "poke" && ev.group_id.is_none() {
            let Some(peer) = ev
                .user_id
                .or(ev.sender_id)
                .or_else(|| ev.sender.as_ref().and_then(|s| s.user_id))
            else {
                return Ok(());
            };
            format!("private:{peer}")
        } else if ev.message_type == "group"
            || (is_notice && ev.sub_type == "poke" && ev.group_id.is_some())
        {
            format!("group:{}", ev.group_id.unwrap_or(0))
        } else {
            return Ok(());
        };

        // #186: the allow and deny lists of this event's configuration, never a startup snapshot.
        let allowed = match context.load_config() {
            Ok(gate_cfg) => social_state::inbound_admitted(&gate_cfg, &key),
            Err(error) => {
                println!("[her] ❌ 读取配置失败，{key} 的事件未处理：{error}");
                false
            }
        };
        if !allowed {
            return Ok(());
        }

        let (plain, at_self_direct, reply_id) = plain_text(&ev.message, self_id);
        let body = if plain.is_empty() {
            ev.raw_message.clone()
        } else {
            plain
        };
        // Quoting one of our own messages counts as addressing the bot directly.
        let quote_self = match (reply_id, self_id != 0) {
            (Some(mid), true) => fetch_msg_sender_is_self(http_ob, mid, self_id),
            _ => false,
        };
        // Conversations absent from social state are skipped rather than given synthetic default triggers.
        let Some(mut tr) = conversation_state::read_document_with_bom(context)
            .and_then(|v| conversation_state::Conversation::from_value(&v["conversations"][&key]))
            .and_then(|c| c.field("wakeConfig").filter(|w| !w.is_null()).cloned())
            .and_then(|w| serde_json::from_value::<WakeConfig>(w).ok())
            .map(|w| w.triggers)
        else {
            return Ok(());
        };
        // The persona's names wake her like keywords, except the ones she dropped.
        tr.keywords = wake_words::effective(context, &key, &tr.keywords, &tr.drop_names);

        let inbound_key =
            inbound::event_key(&key, &ev.post_type, event_message_id(ev).as_deref(), text);
        if !replayed
            && self
                .journal
                .record_received(
                    inbound_key.clone(),
                    key.clone(),
                    text.to_string(),
                    now_ms() as i64,
                )
                .await?
                == inbound::Receipt::Duplicate
        {
            return Ok(());
        }
        self.events += 1;
        let conv = conversation_state::read_document_with_bom(context)
            .map(|s| s["conversations"][&key].clone())
            .unwrap_or(Value::Null);
        let directed = question::directed(ev, &body, quote_self, &conv, cfg_raw, now_ms() as i64);
        let (mut wake, mut reason) = decide(
            ev,
            &body,
            at_self_direct || quote_self,
            self_nick,
            self_cards,
            &tr,
            directed,
        );

        // First contact with a reserved2 conversation: its first wake is the bootstrap wake that
        // carries the operator's initial prompt (`bootstrapSent`), whatever the triggers say.
        if pipeline.is_some()
            && (self.live_groups.is_empty() || self.live_groups.iter().any(|g| g == &key))
            && (is_message || ev.sub_type == "poke")
        {
            match social_state::claim_first_wake(context, cfg_raw, &key) {
                Ok(true) => {
                    println!("[her] {key} 首次唤醒：升级为 bootstrap");
                    wake = true;
                    reason = "bootstrap".to_string();
                }
                Ok(false) => {}
                Err(error) => {
                    println!("[her] ❌ {key} 首次唤醒标记失败（本次不注入初始提示词）：{error}")
                }
            }
        }

        // Batch window (`socialV2.wake.batchWindowMs`) coalesces burst messages inside the driver.
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
            self.wakes += 1;
            self.awaiting
                .entry(key.clone())
                .or_default()
                .push(inbound_key);
        } else {
            self.journal
                .mark_handled(vec![inbound_key], now_ms() as i64)
                .await?;
        }

        // Maintain per-conversation recent history regardless of wake outcome.
        {
            let who = ev
                .user_id
                .map(|u| u.to_string())
                .unwrap_or_else(|| "?".to_string());
            let line = format!("{who}: {body}");
            let buf = self.recent_by_key.entry(key.clone()).or_default();
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
                message_id: event_message_id(ev),
                sender,
                text: body.clone(),
                is_self: ev.user_id == Some(self_id),
                time_ms: now_ms().min(i64::MAX as u128) as i64,
            };
            let live_messages = self.runtime_recent_by_key.entry(key.clone()).or_default();
            live_messages.push(live_message);
            if live_messages.len() > 12 {
                live_messages.remove(0);
            }
        }

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
                        ev.user_id
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| "未知".to_string())
                    }
                })
                .or_else(|| ev.user_id.map(|u| u.to_string()))
                .unwrap_or_else(|| "未知".to_string());
            feed_slang(slang_learning, true, cfg_raw, &key, &sender, &body);
        }

        let now_i = now_ms() as i64;
        self.drivers.entry(key.clone()).or_insert_with(|| {
            let sess = conversation_state::read_document_with_bom(context)
                .and_then(|s| s.get("conversations").and_then(|c| c.get(&key)).cloned())
                .unwrap_or_else(|| serde_json::json!({}));
            let mut sched = session::scheduler_from_state(wake_cfg_of(cfg_raw), &sess);
            // Reset empty trigger configurations to defaults at load time so a conversation never sleeps forever.
            if session::ensure_wakeable_at_load(&mut sched, cfg_raw, now_i, 0.5) {
                println!("[her] {key} 的唤醒配置没有任何触发条件，已按默认配置兜底");
            }
            let mut d = session::Driver::new(sched, text_seed(&key, None));
            d.arm_proactive_now(now_i);
            d
        });

        if let Some(state) = conversation_state::read_document_with_bom(context) {
            sync_wake_configs(
                context,
                cfg_raw,
                &state,
                &mut self.drivers,
                &mut self.wake_synced,
                now_i,
            );
        }
        let driver = self.drivers.get_mut(&key).expect("driver exists");
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
        append_runtime_json_line(context, DECISION_NAME, DECISION_LOG, &rec)?;

        let fired = if wake {
            driver.apply(session::Input::Message {
                reason: reason.clone(),
                seq: now_i,
                at_ms: now_i,
            })
        } else {
            driver.apply(session::Input::Tick { at_ms: now_i })
        };

        if let Some(why) = driver.preempted.take() {
            if let Some(pipe) = pipeline.as_deref_mut() {
                println!("[her] 抢占 {key}（{why}）：取消当前回合");
                if let Err(e) = pipe.cancel(&key) {
                    println!("[her] ❌ 取消回合失败：{e}");
                }
            }
        }

        if !fired.is_empty() {
            inbound::start_processing(
                &self.journal,
                &mut self.awaiting,
                &mut self.in_flight,
                &key,
                now_ms() as i64,
            )
            .await?;
        }
        for e in fired {
            self.wakes += 1;
            println!("[her] wake {key} ({})", e.reason);
            append_runtime_json_line(
                context,
                WAKE_NAME,
                WAKE_LOG,
                &json!({
                    "ts": e.at_ms,
                    "key": key,
                    "reason": e.reason,
                    "pid": std::process::id(),
                    "shadow": self.shadow_flag
                }),
            )?;

            if let Some(pipe) = pipeline.as_deref_mut() {
                let taken =
                    self.live_groups.is_empty() || self.live_groups.iter().any(|g| g == &key);
                if !taken {
                    println!("[her] {key} 不在灰度名单，交给其他处理器");
                } else {
                    self.execute_turn(
                        pipe, context, cfg_raw, &key, &reason, self_nick, self_id, true,
                    )
                    .await;
                }
            }
        }
        Ok(())
    }
}

impl SimulationBridge {
    fn new(
        context: &runtime::RuntimeContext,
        journal: inbound::SharedJournal,
        live_groups: Vec<String>,
        live_all: bool,
    ) -> Self {
        if context.is_isolated() {
            Self::Isolated {
                hub: Box::new(reserved2::Hub::new(context, journal, live_groups, live_all)),
                events: 0,
            }
        } else {
            Self::Fallback(Box::new(FallbackWakeDrivers::new(journal, live_groups)))
        }
    }

    fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated { .. })
    }

    fn note_event(&mut self) {
        match self {
            Self::Isolated { events, .. } => *events += 1,
            Self::Fallback(fallback) => fallback.events += 1,
        }
    }

    fn counters(&self) -> (u64, u64) {
        match self {
            Self::Isolated { events, .. } => (*events, 0),
            Self::Fallback(fallback) => (fallback.events, fallback.wakes),
        }
    }

    fn reset_connection(&mut self) {
        match self {
            Self::Isolated { events, .. } => *events = 0,
            Self::Fallback(fallback) => fallback.reset_connection(),
        }
    }

    async fn flush_pending(
        &mut self,
        pipeline: Option<&mut pipeline::Pipeline>,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        self_nick: &str,
        self_id: i64,
    ) {
        if let Self::Fallback(fallback) = self {
            fallback
                .flush_pending(pipeline, context, cfg_raw, self_nick, self_id)
                .await;
        }
    }

    async fn finish_in_flight(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Self::Fallback(fallback) = self {
            fallback.finish_in_flight().await?;
        }
        Ok(())
    }

    async fn tick(
        &mut self,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        self_nick: &str,
        self_id: i64,
        tick_now: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Isolated { hub, .. } => {
                if let Err(error) = hub.tick(self_nick, self_id).await {
                    println!("[reserved2] scheduler error: {error}");
                }
                Ok(())
            }
            Self::Fallback(fallback) => fallback.tick(context, cfg_raw, tick_now).await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_event(
        &mut self,
        context: &runtime::RuntimeContext,
        cfg_raw: &Value,
        event_cfg: Option<Value>,
        ev: &Event,
        text: &str,
        replayed: bool,
        self_id: i64,
        self_nick: &str,
        self_cards: &HashMap<i64, String>,
        http_ob: &onebot::OneBot,
        skip_probability: f64,
        slang_enabled: bool,
        slang_learning: &slang_learn::SlangLearning,
        pipeline: Option<&mut pipeline::Pipeline>,
        is_message: bool,
        is_notice: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Isolated { hub, .. } => {
                if social_state::mode(context) == "reserved2" {
                    let handled = match event_cfg {
                        Some(cfg) => {
                            hub.handle_with_config(
                                cfg, ev, text, replayed, self_id, self_nick, self_cards,
                            )
                            .await
                        }
                        None => {
                            hub.handle(ev, text, replayed, self_id, self_nick, self_cards)
                                .await
                        }
                    };
                    if let Err(error) = handled {
                        println!("[reserved2] ingress error: {error}");
                    }
                }
                Ok(())
            }
            Self::Fallback(fallback) => {
                fallback
                    .handle_event(
                        context,
                        cfg_raw,
                        ev,
                        text,
                        replayed,
                        self_id,
                        self_nick,
                        self_cards,
                        http_ob,
                        skip_probability,
                        slang_enabled,
                        slang_learning,
                        pipeline,
                        is_message,
                        is_notice,
                    )
                    .await
            }
        }
    }
}

pub(crate) async fn run(
    runtime_context: &runtime::RuntimeContext,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let legacy_source = !runtime_context.is_isolated();
    let _live_transition = if runtime_context.is_isolated() {
        let gate_path = runtime_context.resolve_path(
            runtime::RuntimeArea::Data,
            "lifecycle/lifecycle.lock",
            runtime::PathIntent::WriteFile,
        )?;
        let gate = lifecycle::lock_with_wait(&gate_path, std::time::Duration::from_secs(5))?;
        let desired = lifecycle::DesiredStore::new(runtime_context.data_dir().join("lifecycle"));
        match lifecycle::effective(&desired.read(), now_ms() as i64) {
            lifecycle::Effective::Act { wants, .. } if wants.live => {}
            _ => {
                eprintln!("[her] desired state does not permit live startup");
                std::process::exit(3);
            }
        }
        Some(gate)
    } else {
        None
    };
    let Some(mut lock) = acquire_single_instance(runtime_context) else {
        recovery_log::record(
            runtime_context,
            run_record::Trigger::Startup,
            run_record::Component::Process,
            "single-instance",
            run_record::Outcome::Skipped,
            "another bridge process owns this runtime; this one exits",
        );
        return Ok(());
    };
    let _source_owner = if runtime_context.is_isolated() {
        session_map::verify_cutover(runtime_context)?;
        let endpoint = runtime_context
            .dependency_url("onebot_ws")
            .ok_or("isolated runtime requires a loopback onebot_ws dependency")?;
        let selected = args
            .iter()
            .position(|arg| arg == "--live-groups")
            .and_then(|index| args.get(index + 1));
        let groups = parse_live_groups(selected.map(String::as_str));
        Some(lifecycle::acquire_message_source(endpoint, &groups)?)
    } else {
        None
    };
    drop(_live_transition);
    recovery_log::bind_default(runtime_context);
    recovery_log::record(
        runtime_context,
        run_record::Trigger::Startup,
        run_record::Component::Process,
        "start",
        run_record::Outcome::Ok,
        "live bridge process started",
    );
    dependency_watch::install_crash_marker(runtime_context);
    // The marker is diagnostic; only the held OS gate decides ownership.
    let mut lock_touched = std::time::Instant::now();

    let mut cfg_raw = if runtime_context.is_isolated() {
        runtime_context.load_config().map_err(|e| e.to_string())?
    } else {
        read_json(CONFIG_PATH).ok_or("no config.json")?
    };
    let cfg: Config = serde_json::from_value(cfg_raw.clone())?;
    let mut ws = if runtime_context.is_isolated() {
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
    let mut token = if !legacy_source {
        cfg.snowluma
            .as_ref()
            .and_then(|s| s.access_token.clone())
            .filter(|t| !t.is_empty())
    } else {
        discover_ws_token().or_else(|| {
            cfg.snowluma
                .as_ref()
                .and_then(|s| s.access_token.clone())
                .filter(|t| !t.is_empty())
        })
    };

    println!("[her] mode: --live 已指定（是否真发消息还要看 --live-groups）");
    println!(
        "[her] connect {ws} (token {})",
        if token.is_some() { "ok" } else { "missing" }
    );
    // #186: the allow lists are read at each use below; startup reports only where they and rollout.rustGroups disagree.
    report_rollout_scope(runtime_context, &cfg_raw);

    let no_reconnect = args.iter().any(|a| a == "--no-reconnect");
    if no_reconnect {
        println!("[her] --no-reconnect：连接断开即退出（调试用）");
    }
    // Open the inbound journal before connecting so accepted frames can always be persisted.
    let journal = inbound::Journal::open(runtime_context)?;
    journal.recover_queued_gap_notices()?;
    let journal = inbound::SharedJournal::new(journal);
    let interrupted = journal.recover_interrupted(now_ms() as i64).await?;
    if !interrupted.is_empty() {
        println!(
            "[her] 上次运行中断的入站事件 {} 条：状态未知，不自动重提，已记入管理端",
            interrupted.len()
        );
        let notices = journal
            .run(move |j| j.interrupted_notice_details(&interrupted))
            .await?;
        let (notice_context, notice_cfg) = (runtime_context.clone(), cfg_raw.clone());
        tokio::task::spawn_blocking(move || {
            dependency_watch::report_interrupted(&notice_context, &notice_cfg, &notices);
        })
        .await?;
    }
    if runtime_context.is_isolated() {
        dependency_watch::report_last_crash(runtime_context, &cfg_raw);
        // #179: write the guarded DSH profile rows back and check the whole profile once at start.
        dsh_profile::startup(runtime_context);
        harness::route_health::reconcile_startup(runtime_context, &cfg_raw);
        if let Err(error) = harness::usage::reconcile_startup(runtime_context) {
            recovery_log::record(
                runtime_context,
                run_record::Trigger::Recovery,
                run_record::Component::ModelChain,
                "usage-reconcile-failed",
                run_record::Outcome::Failed,
                &error,
            );
        }
        let price_table = pricing::resolve(cfg_raw.get("pricing"));
        if let Err(error) = ledger::LedgerStorage::compact_if_needed(runtime_context, &price_table) {
            recovery_log::record(
                runtime_context,
                run_record::Trigger::Recovery,
                run_record::Component::ModelChain,
                "usage-compact-failed",
                run_record::Outcome::Failed,
                &error,
            );
        }
    }
    // Validate local live state (rollout scope, session map, DSH credentials, dependencies)
    // before publishing any heartbeat or contacting the gateway. Once local initialization
    // succeeds, publish the waiting-for-gateway heartbeat immediately and spawn the single
    // heartbeat task so a down gateway does not cause a supervisor startup kill.
    let initial_startup = prepare_live_startup(runtime_context, &cfg_raw, args)?;
    if !initial_startup.0.is_empty() {
        println!("[her] 灰度接管：{:?}", initial_startup.0);
    }
    let mut simulation = SimulationBridge::new(
        runtime_context,
        journal.clone(),
        initial_startup.0.clone(),
        initial_startup.1,
    );
    let mut reserved_hub = if runtime_context.is_isolated() {
        Some(reserved::Hub::new(
            runtime_context,
            journal.clone(),
            initial_startup.0.clone(),
            initial_startup.1,
        )?)
    } else {
        None
    };
    let mut interaction_hub = None;
    if runtime_context.is_isolated() {
        // DSH probe, health snapshot for the console and the SnowLuma status poll: once per process, not per connection.
        tokio::spawn(dependency_watch::run(runtime_context.clone()));
    }
    // Reconnect pacing: doubling waits up to 30 s, reset only by a connection that delivered frames (no storm
    // during a long outage); a warning at the third failure in a row.
    let mut gateway_backoff = dependency::ReconnectBackoff::gateway();
    let mut heal_throttle = snowluma_heal::Throttle::default();
    let mut heal_http_token: Option<String> = None;
    // #186: the gap lists of the last readable configuration, for a reconnect that cannot read it.
    let mut gap_admitted = (
        social_state::admitted_ids(&cfg_raw, "group"),
        social_state::admitted_ids(&cfg_raw, "private"),
    );
    // #192 (F3): saved events held while the configuration cannot be read (journal keys, arrival order), kept across
    // reconnects, and the refusals not recorded yet.
    let mut held: Vec<String> = Vec::new();
    let mut refusals = UnreadableRefusals::default();
    let heartbeat_name = "rust-heartbeat-live.json";
    publish_live_heartbeat(runtime_context, heartbeat_name, &initial_startup.0, false);
    let (gateway_tx, mut gateway_rx) = tokio::sync::watch::channel(false);
    {
        let groups = initial_startup.0.clone();
        let heartbeat_context = runtime_context.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {}
                    changed = gateway_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
                let connected = *gateway_rx.borrow_and_update();
                publish_live_heartbeat(&heartbeat_context, heartbeat_name, &groups, connected);
            }
        });
    }
    let mut initial_startup = Some(initial_startup);
    'reconnect: loop {
        // Isolated startup validates all selected-root state before contacting its WebSocket.
        // Keep this inside the reconnect loop: a failed handshake drops this candidate pipeline,
        // and the next attempt repeats local validation before dialing again. The selected-root
        // gate is already held, so pipeline setup may create only its selected workspace folder.
        let (live_groups, _live_all, mut pipeline) = if let Some(prepared) = initial_startup.take()
        {
            prepared
        } else {
            prepare_live_startup(runtime_context, &cfg_raw, args)?
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
                let mut wait = gateway_backoff.on_failure(dependency::jitter_unit(
                    now_ms() as i64,
                    u64::from(gateway_backoff.streak()),
                ));
                println!(
                    "[her] 连接失败：{e}；{} 秒后重试（连续第 {} 次）",
                    wait / 1000,
                    gateway_backoff.streak()
                );
                gateway_health::with(|h| h.connected = false);
                set_gateway_connected(&gateway_tx, false);
                recovery_log::record(
                    runtime_context,
                    run_record::Trigger::Timer,
                    run_record::Component::Onebot,
                    "connect-failed",
                    run_record::Outcome::Failed,
                    &format!("gateway connection failed: {e}"),
                );
                if gateway_backoff.should_warn() {
                    println!(
                        "[her] ⚠️ 与网关的连接已连续失败 {} 次，继续按退避重试",
                        gateway_backoff.streak()
                    );
                    recovery_log::record(
                        runtime_context,
                        run_record::Trigger::Timer,
                        run_record::Component::Onebot,
                        "connect-streak",
                        run_record::Outcome::Failed,
                        &format!(
                            "{} failed connection attempts in a row; retrying with backoff",
                            gateway_backoff.streak()
                        ),
                    );
                }
                if dependency_watch::adopt_heal(
                    runtime_context,
                    &mut heal_throttle,
                    &format!("connect failed {e}"),
                    &mut ws,
                    &mut token,
                    &mut heal_http_token,
                    &mut cfg_raw,
                )
                .await
                {
                    wait = 500;
                }
                if lock_touched.elapsed().as_secs() >= 10 {
                    let _ = lock.refresh(now_ms() as i64);
                    lock_touched = std::time::Instant::now();
                }
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                continue 'reconnect;
            }
        };
        println!("[her] connected");
        gateway_health::with(|h| h.connected = true);
        if gateway_backoff.streak() > 0 {
            recovery_log::record(
                runtime_context,
                run_record::Trigger::Recovery,
                run_record::Component::Onebot,
                "reconnected",
                run_record::Outcome::Ok,
                &format!(
                    "gateway connected again after {} failed attempts",
                    gateway_backoff.streak()
                ),
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

        // Idle-chat skip probability from `socialV2.social.skipProbability`.
        let skip_probability = cfg
            .social_v2
            .as_ref()
            .map(|s| s.social.skip_probability)
            .unwrap_or(0.0);
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
        let http_token = heal_http_token.clone().unwrap_or_else(|| {
            cfg.snowluma
                .as_ref()
                .and_then(|s| s.access_token.clone())
                .unwrap_or_default()
        });
        let http_ob = if runtime_context.is_isolated() {
            onebot::OneBot::try_new_url(&http_base, http_token.clone())
                .map_err(|_| "isolated runtime requires a valid loopback OneBot HTTP endpoint")?
        } else {
            onebot::OneBot::try_new_url(http_base.trim_end_matches('/'), http_token.clone())
                .map_err(|error| {
                    format!("live runtime requires a valid loopback OneBot HTTP endpoint: {error}")
                })?
        };

        // Disconnect gap recovery and replay before accepting new frames from the socket.
        {
            gap_admitted = gap_lists(runtime_context.load_config(), &gap_admitted);
            let (groups, privates) = gap_admitted.clone();
            let opened = journal
                .run(move |j| inbound::open_reconnect_gaps(j, &groups, &privates, now_ms() as i64))
                .await;
            if let Err(error) = opened {
                println!("[her] ❌ 断线缺口记录失败：{error}");
            }
            let (notice_context, notice_cfg) = (runtime_context.clone(), cfg_raw.clone());
            tokio::task::spawn_blocking(move || {
                dependency_watch::report_unresolved_gaps(
                    &notice_context,
                    &notice_cfg,
                    "inbound-gap-opened",
                );
            })
            .await?;
            let history_ob = http_ob.clone();
            let recovery_context = runtime_context.clone();
            let spawned = inbound::spawn_gap_recovery(
                move || inbound::Journal::open(&recovery_context),
                move |group_id, seq| inbound::fetch_group_history(&history_ob, group_id, seq),
                now_ms() as i64,
                {
                    let (gap_context, gap_cfg) = (runtime_context.clone(), cfg_raw.clone());
                    move |result| match result {
                        Ok(report) => {
                            if !report.is_empty() {
                                println!("[her] 断线缺口补收结果：{}", report.join("；"));
                            }
                            dependency_watch::report_unresolved_gaps(
                                &gap_context,
                                &gap_cfg,
                                "inbound-gap",
                            );
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
        let mut mute_refresh_armed = false;
        let mut last_mute_recheck = std::time::Instant::now();
        let mut last_prune = std::time::Instant::now();
        let mut replay_queue: std::collections::VecDeque<inbound::Replayable> =
            std::collections::VecDeque::new();
        let replay_cfg = runtime_context.load_config();
        if let Err(error) = &replay_cfg {
            println!("[her] ❌ 读取配置失败，已保存的入站事件留待下次连接重放：{error}");
        }
        let saved = journal.replayable().await?;
        if replay_cfg.is_err() {
            for item in &saved {
                hold(&mut held, item.event_key.clone());
            }
        } else {
            held.clear();
        }
        let (replay, settle) = split_replayable(replay_cfg.as_ref().ok(), saved);
        replay_queue.extend(replay);
        if !settle.is_empty() {
            journal.mark_handled(settle, now_ms() as i64).await?;
        }
        if !replay_queue.is_empty() {
            println!(
                "[her] 重放已保存但未处理完的入站事件 {} 条",
                replay_queue.len()
            );
        }

        if pipeline.is_some() {
            turn_recovery::spawn(runtime_context.clone());
        }
        let mut chat_hub: Option<chat::Hub> =
            if pipeline.is_some() && runtime_context.is_isolated() {
                if interaction_hub.is_none() {
                    interaction_hub = Some(closed_agent::Hub::start(runtime_context)?);
                }
                match chat::Hub::new(
                    runtime_context,
                    journal.clone(),
                    &cfg_raw,
                    interaction_hub.clone(),
                ) {
                    Ok(hub) => Some(hub),
                    Err(error) => {
                        println!("[her] ❌ chat 模式不可用：{error}");
                        None
                    }
                }
            } else {
                None
            };
        // Transition the heartbeat state to ready now that the gateway connection and
        // local recovery setup are active.
        set_gateway_connected(&gateway_tx, true);

        let slang_enabled = cfg_raw
            .get("slang")
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let slang_learning = slang_learn::SlangLearning::new(runtime_context);
        if slang_enabled {
            slang_learning.start_catch_up();
        }

        let mut last_heartbeat_ms: u128 = 0;
        let shadow_flag = live_groups.is_empty();
        append_runtime_json_line(
            runtime_context,
            DECISION_NAME,
            DECISION_LOG,
            &json!({
                "ts": now_ms(),
                "type": "start",
                "pid": std::process::id(),
                "shadow": shadow_flag,
                "groups": live_groups,
            }),
        )?;

        simulation.reset_connection();
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let tick_now = now_ms() as i64;

            simulation
                .flush_pending(
                    pipeline.as_mut(),
                    runtime_context,
                    &cfg_raw,
                    &self_nick,
                    self_id,
                )
                .await;

            if lock_touched.elapsed().as_secs() >= 10 {
                let _ = lock.refresh(tick_now);
                lock_touched = std::time::Instant::now();
            }
            if last_prune.elapsed().as_secs() >= 3600 {
                last_prune = std::time::Instant::now();
                let pruning = journal.clone();
                tokio::spawn(async move {
                    if let Err(error) = pruning.run(|j| j.prune(now_ms() as i64)).await {
                        println!("[her] ❌ 入站库保留清理失败：{error}");
                    }
                });
            }
            simulation.finish_in_flight().await?;
            replay_queue.extend(turn_recovery::take_resumable());
            if !held.is_empty() || refusals.count > 0 {
                if let Ok(now_cfg) = runtime_context.load_config() {
                    if let Some(count) = refusals.flush(std::time::Instant::now()) {
                        record_unreadable_refusals(runtime_context, count);
                    }
                    let keys = std::mem::take(&mut held);
                    if !keys.is_empty() {
                        let (replay, settle) =
                            take_held(&now_cfg, &keys, journal.replayable().await?);
                        if !settle.is_empty() {
                            journal.mark_handled(settle, now_ms() as i64).await?;
                        }
                        println!(
                            "[her] the configuration reads again: replaying {} held inbound events",
                            replay.len()
                        );
                        for item in replay.into_iter().rev() {
                            replay_queue.push_front(item);
                        }
                    }
                }
            }
            let (text, replayed) = if let Some(item) = replay_queue.pop_front() {
                (item.raw, true)
            } else {
                let msg = tokio::select! {
                    m = stream.next() => match m {
                        Some(m) => m,
                        None => break,
                    },
                    _ = ticker.tick() => {
                        if self_id > 0 && last_mute_recheck.elapsed() >= group_mute::RECHECK_EVERY {
                            last_mute_recheck = std::time::Instant::now();
                            let groups: Vec<(i64, Option<i64>)> = group_mute::unknown_end_groups(runtime_context, parity_clock::now_ms())
                                .into_iter()
                                .map(|g| (g, None))
                                .collect();
                            group_mute::spawn_refresh(runtime_context.clone(), http_base.clone(), http_token.clone(), self_id, groups);
                        }
                        if let Some(hub) = reserved_hub.as_mut() {
                            if let Err(error) = hub.tick(&self_nick).await {
                                println!("[reserved] scheduler error: {error}");
                            }
                        }
                        simulation
                            .tick(runtime_context, &cfg_raw, &self_nick, self_id, tick_now)
                            .await?;
                        continue;
                    }
                };
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
            // Unpaired UTF-16 surrogate escapes (\uD800-\uDFFF) cannot be represented in UTF-8 `String`;
            // replace them with U+FFFD and log rather than dropping the frame.
            let (ev, text) = match crate::aux_text::parse_json_replacing_lone_surrogates::<Event>(&text)
            {
                Ok((ev, None)) => (ev, text),
                Ok((ev, Some(repaired))) => {
                    println!("[her] 入站帧含孤立代理项转义，已替换为 U+FFFD 后照常处理");
                    (ev, repaired)
                }
                Err(_) => {
                    // #46: QQ's whole-group mute notice has user_id 0, which `Event` refuses.
                    if !replayed && self_id > 0 {
                        if let Some((gid, operator)) = group_mute::whole_group_notice(&text) {
                            if runtime_context.load_config().is_ok_and(|now_cfg| {
                                social_state::inbound_admitted(&now_cfg, &format!("group:{gid}"))
                            }) {
                                let (context, key) =
                                    (runtime_context.clone(), format!("group:{gid}"));
                                if let Err(error) = tokio::task::spawn_blocking(move || {
                                    group_mute::note_whole_group_notice(&context, &key)
                                })
                                .await
                                {
                                    println!(
                                        "[mute] group:{gid} 全员禁言通知的处理线程失败：{error}"
                                    );
                                }
                                group_mute::spawn_refresh(
                                    runtime_context.clone(),
                                    http_base.clone(),
                                    http_token.clone(),
                                    self_id,
                                    vec![(gid, operator)],
                                );
                            }
                        }
                    }
                    continue;
                }
            };
            if !replayed {
                got_traffic = true;
                dependency_watch::note_frame(
                    &ev.post_type,
                    &ev.meta_event_type,
                    ev.status.as_ref(),
                );
                if let Some(at) = seen.observe(now_ms() as i64, &ev.post_type) {
                    journal.touch_seen(at).await?;
                }
            }
            if !mute_refresh_armed {
                let me = if self_id > 0 {
                    self_id
                } else {
                    ev.self_id.unwrap_or(0)
                };
                if me > 0 {
                    mute_refresh_armed = true;
                    let admitted = runtime_context
                        .load_config()
                        .map(|now_cfg| social_state::admitted_ids(&now_cfg, "group"))
                        .unwrap_or_default();
                    let groups: Vec<(i64, Option<i64>)> = admitted
                        .iter()
                        .filter_map(|g| g.parse().ok())
                        .map(|g| (g, None))
                        .collect();
                    group_mute::spawn_refresh(
                        runtime_context.clone(),
                        http_base.clone(),
                        http_token.clone(),
                        me,
                        groups,
                    );
                }
            }

            if ev.echo.is_some() {
                if let Some(echo) = ev.echo.as_ref().and_then(|e| e.as_str()) {
                    if let Some(gid) = echo.strip_prefix("card:").and_then(|s| s.parse::<i64>().ok())
                    {
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
                            self_id = d
                                .get("user_id")
                                .and_then(protocol_integer)
                                .filter(|id| *id > 0)
                                .unwrap_or(self_id);
                            println!("[her] identity {self_nick} ({self_id})");
                            if runtime_context.is_isolated() {
                                let identity = serde_json::json!({
                                    "userId": self_id.to_string(),
                                    "nickname": self_nick,
                                    "updatedAt": now_ms()
                                });
                                if let Err(error) = runtime_context.write_atomic(
                                    runtime::RuntimeArea::Data,
                                    "identity.json",
                                    identity.to_string().as_bytes(),
                                ) {
                                    println!("[her] identity.json 未能写入：{error}");
                                }
                            }
                            if !http_token.is_empty() && self_id != 0 {
                                let admitted = runtime_context
                                    .load_config()
                                    .map(|now_cfg| social_state::admitted_ids(&now_cfg, "group"))
                                    .unwrap_or_default();
                                for g in &admitted {
                                    if let Ok(gid) = g.parse::<i64>() {
                                        match fetch_group_card(&http_ob, gid, self_id) {
                                            Some(card) => {
                                                println!("[her] 群 {gid} 的群名片：{card}");
                                                self_cards.insert(gid, card);
                                            }
                                            None => {
                                                println!("[her] 群 {gid} 无群名片（或查询失败）")
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
                    let t = now_ms();
                    if t - last_heartbeat_ms >= 60_000 {
                        last_heartbeat_ms = t;
                        let (events, wakes) = simulation.counters();
                        println!("[her] heartbeat (events={events} wakes={wakes})");
                    }
                }
                continue;
            }
            // Chat history log (#58): every conversation, before self filtering, the allow list and mode dispatch.
            if !replayed {
                history_log::record_inbound(runtime_context, &text);
            }
            let is_message = ev.post_type == "message";
            let is_notice = ev.post_type == "notice";
            if !is_message && !is_notice {
                continue;
            }
            if self_id == 0 {
                self_id = ev.self_id.unwrap_or(0);
                if self_id > 0 && runtime_context.is_isolated() {
                    let saved: Value = runtime_context
                        .read_optional_text(runtime::RuntimeArea::Data, "identity.json")
                        .ok()
                        .flatten()
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or(Value::Null);
                    let id = self_id.to_string();
                    let nickname = if !self_nick.is_empty() {
                        self_nick.as_str()
                    } else if saved["userId"] == id {
                        saved["nickname"].as_str().unwrap_or("")
                    } else {
                        ""
                    };
                    let identity = json!({"userId": id, "nickname": nickname, "updatedAt": now_ms()});
                    if let Err(error) = runtime_context.write_atomic(
                        runtime::RuntimeArea::Data,
                        "identity.json",
                        identity.to_string().as_bytes(),
                    ) {
                        println!("[her] identity.json 未能写入：{error}");
                    }
                }
            }
            // #46: QQ's mute notice. Before the self filter (it names her own id) and before mode dispatch.
            if is_notice && ev.notice_type.as_ref().and_then(Value::as_str) == Some("group_ban") {
                let me = if self_id > 0 {
                    self_id
                } else {
                    ev.self_id.unwrap_or(0)
                };
                if let (false, Some(gid)) = (replayed, ev.group_id) {
                    if me > 0
                        && ev.user_id == Some(me)
                        && runtime_context.load_config().is_ok_and(|now_cfg| {
                            social_state::inbound_admitted(&now_cfg, &format!("group:{gid}"))
                        })
                    {
                        let operator = ev
                            .operator_id
                            .as_ref()
                            .and_then(protocol_integer)
                            .filter(|id| *id > 0);
                        let (context, sub_type, duration, key) = (
                            runtime_context.clone(),
                            ev.sub_type.clone(),
                            ev.duration.clone(),
                            format!("group:{gid}"),
                        );
                        let log_key = format!("group:{gid}");
                        if let Err(error) = tokio::task::spawn_blocking(move || {
                            let cfg = context.load_config().unwrap_or_else(|_| json!({}));
                            group_mute::on_self_notice(
                                &context,
                                &cfg,
                                &key,
                                &sub_type,
                                duration.as_ref(),
                                operator,
                            );
                        })
                        .await
                        {
                            println!("[mute] {log_key} 禁言通知的处理线程失败：{error}");
                        }
                    }
                }
                continue;
            }
            let sender = if is_notice {
                ev.sender_id.or(ev.user_id)
            } else {
                ev.user_id
            }
            .or_else(|| ev.sender.as_ref().and_then(|s| s.user_id));
            if self_id > 0
                && (sender == Some(self_id)
                    || (is_message
                        && ev.sender.as_ref().and_then(|s| s.user_id) == Some(self_id)))
            {
                continue;
            }

            // #192 (F3): read config once here; while unreadable, reserved2 holds served events in order.
            let event_cfg = if runtime_context.is_isolated() {
                let read = runtime_context.load_config();
                let reserved2_now = simulation.is_isolated()
                    && social_state::mode(runtime_context) == "reserved2";
                let key = held_conversation(&ev).unwrap_or_default();
                let live_all = args.iter().any(|a| a == "--live-all");
                let served = reserved2_serves(live_all, &live_groups, &key);
                let listed = !key.is_empty() && live_groups.iter().any(|g| g == &key);
                let refusable =
                    !key.is_empty() && !reserved2_now && (live_groups.is_empty() || listed);
                match ingress_for(
                    read.is_ok(),
                    reserved2_now && served,
                    !held.is_empty(),
                    refusable,
                ) {
                    Ingress::Handle => read.ok(),
                    Ingress::Hold => {
                        let inbound_key = inbound::event_key(
                            &key,
                            &ev.post_type,
                            event_message_id(&ev).as_deref(),
                            &text,
                        );
                        let saved = replayed
                            || journal
                                .record_received(
                                    inbound_key.clone(),
                                    key.clone(),
                                    text.clone(),
                                    now_ms() as i64,
                                )
                                .await?
                                == inbound::Receipt::New;
                        if saved {
                            if !hold(&mut held, inbound_key) {
                                println!("[her] ⚠️ {key} event saved; {HELD_LIMIT} are held already, so it replays at the next connection");
                            } else if let Err(error) = &read {
                                println!("[her] ⚠️ configuration unreadable ({error}): {key} event saved and held until it reads again");
                            } else {
                                println!("[her] {key} event saved and held behind the events held earlier");
                            }
                        }
                        continue;
                    }
                    Ingress::Refuse => {
                        let error = read.err().unwrap_or_default();
                        println!("[her] ❌ configuration unreadable ({error}): {key} event not processed");
                        if let Some(count) = refusals.note(std::time::Instant::now()) {
                            record_unreadable_refusals(runtime_context, count);
                        }
                        continue;
                    }
                    Ingress::Skip => continue,
                }
            } else {
                None
            };

            // Administrative release approval and persona switching are independent of participation mode.
            if is_message && pipeline.is_some() && runtime_context.is_isolated() {
                let (plain, _, _) = plain_text(&ev.message, self_id);
                let persona_command = persona_command::is_persona_command(&plain);
                if persona_command || chat::is_update_command(&plain) {
                    let (kind, id) = if ev.message_type == "private" {
                        ("private", sender.unwrap_or(0))
                    } else {
                        ("group", ev.group_id.unwrap_or(0))
                    };
                    let key = format!("{kind}:{id}");
                    if id == 0 || !(live_groups.is_empty() || live_groups.contains(&key)) {
                        continue;
                    }
                    let inbound_key = inbound::event_key(
                        &key,
                        &ev.post_type,
                        event_message_id(&ev).as_deref(),
                        &text,
                    );
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
                    let _admission = if persona_command {
                        None
                    } else {
                        Some(release::turn_guard(runtime_context).await?)
                    };
                    let context = runtime_context.clone();
                    let user = sender.unwrap_or(0).to_string();
                    let result = tokio::task::spawn_blocking(move || {
                        if persona_command {
                            persona_command::handle(&context, &key, &user, &plain)
                        } else {
                            chat::handle_update_command(&context, &key, &user, &plain)
                        }
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        if persona_command {
                            crate::safe_eprintln!("[persona] persona command reply not confirmed");
                        } else {
                            crate::safe_eprintln!(
                                "[release] update command refused or reply not confirmed"
                            );
                        }
                    }
                    journal
                        .mark_handled(vec![inbound_key], now_ms() as i64)
                        .await?;
                    continue;
                }
            }

            if turn_recovery::owner_command(runtime_context, &journal, &ev, &text, replayed).await?
            {
                continue;
            }

            if let Some(hub) = chat_hub.as_mut() {
                if matches!(
                    social_state::mode(runtime_context).as_str(),
                    "chat" | "reserved" | "closed-agent"
                ) {
                    if !is_message {
                        continue;
                    }
                    let (kind, id) = if ev.message_type == "private" {
                        ("private", ev.user_id.unwrap_or(0))
                    } else {
                        ("group", ev.group_id.unwrap_or(0))
                    };
                    let me = ev.self_id.unwrap_or(self_id);
                    let own = ev.user_id == Some(me)
                        || ev.sender.as_ref().and_then(|s| s.user_id) == Some(me);
                    let key = format!("{kind}:{id}");
                    if id == 0
                        || own
                        || !(live_groups.is_empty() || live_groups.iter().any(|g| g == &key))
                    {
                        continue;
                    }
                    let chat_cfg = match runtime_context.load_config() {
                        Ok(chat_cfg) => chat_cfg,
                        Err(error) => {
                            println!("[her] ❌ chat 读取配置失败：{error}");
                            continue;
                        }
                    };
                    if !chat::allowed_in_mode(
                        &chat_cfg,
                        &social_state::mode(runtime_context),
                        &key,
                    ) {
                        continue;
                    }
                    let inbound_key = inbound::event_key(
                        &key,
                        &ev.post_type,
                        event_message_id(&ev).as_deref(),
                        &text,
                    );
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
                    if kind == "group" && group_mute::active(runtime_context, &key).is_some() {
                        journal
                            .mark_handled(vec![inbound_key], now_ms() as i64)
                            .await?;
                        continue;
                    }
                    simulation.note_event();
                    let inb = chat::Inbound {
                        kind,
                        id,
                        user_id: ev.user_id.unwrap_or(0),
                        self_id: me,
                        card: ev
                            .sender
                            .as_ref()
                            .map(|s| s.card.trim().to_string())
                            .unwrap_or_default(),
                        nickname: ev
                            .sender
                            .as_ref()
                            .map(|s| s.nickname.trim().to_string())
                            .unwrap_or_default(),
                        message_id: event_message_id(&ev),
                        message: ev.message.clone(),
                    };
                    if social_state::mode(runtime_context) == "reserved" {
                        if let Some(reserved) = reserved_hub.as_mut() {
                            if let Err(error) = reserved.handle(&inb, &inbound_key, &self_nick).await
                            {
                                println!("[reserved] ingress error: {error}");
                            }
                        }
                        continue;
                    }
                    match hub.handle(&chat_cfg, &inb, &inbound_key).await {
                        Ok(chat::Outcome::Dispatched { plain, sender }) => {
                            if kind == "group" && slang_learn::enabled(&chat_cfg) {
                                feed_slang(
                                    &slang_learning,
                                    true,
                                    &chat_cfg,
                                    &key,
                                    &sender,
                                    &plain,
                                );
                            }
                        }
                        Ok(chat::Outcome::Done) => {}
                        Err(error) => {
                            println!("[her] ❌ chat 消息处理失败 {key}：{error}");
                            journal
                                .mark_handled(vec![inbound_key], now_ms() as i64)
                                .await?;
                        }
                    }
                    continue;
                }
            }

            simulation
                .handle_event(
                    runtime_context,
                    &cfg_raw,
                    event_cfg,
                    &ev,
                    &text,
                    replayed,
                    self_id,
                    &self_nick,
                    &self_cards,
                    &http_ob,
                    skip_probability,
                    slang_enabled,
                    &slang_learning,
                    pipeline.as_mut(),
                    is_message,
                    is_notice,
                )
                .await?;
        }

        if let Some(at) = seen.pending() {
            if let Err(error) = journal.touch_seen(at).await {
                println!("[her] ❌ 记录最后存活时刻失败：{error}");
            }
        }
        if no_reconnect {
            println!("[her] --no-reconnect：不重连，直接退出");
            break 'reconnect;
        }
        gateway_health::with(|h| h.connected = false);
        set_gateway_connected(&gateway_tx, false);
        if got_traffic {
            gateway_backoff.on_traffic();
        }
        let mut wait = gateway_backoff.on_failure(dependency::jitter_unit(
            now_ms() as i64,
            u64::from(gateway_backoff.streak()),
        ));
        println!("[her] 连接已断开，{} 秒后重连", wait / 1000);
        recovery_log::record(
            runtime_context,
            run_record::Trigger::Timer,
            run_record::Component::Onebot,
            "disconnected",
            run_record::Outcome::Failed,
            "gateway connection closed; reconnecting with backoff",
        );
        if dependency_watch::adopt_heal(
            runtime_context,
            &mut heal_throttle,
            "close",
            &mut ws,
            &mut token,
            &mut heal_http_token,
            &mut cfg_raw,
        )
        .await
        {
            wait = 500;
        }
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    }

    println!("[her] 退出（重连循环已结束）");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_http_endpoint(url: &str) -> Option<(String, u16)> {
        let ep = crate::loopback_http::LoopbackEndpoint::parse(url).ok()?;
        Some((ep.host().to_string(), ep.port()))
    }

    #[test]
    fn reserved2_trigger_precedence_matches_frozen_js() {
        let event = ev(true, "needle");
        let cards = HashMap::new();
        let mut tr: Triggers =
            serde_json::from_value(json!({"anyMessage":true,"atMention":true})).unwrap();
        assert_eq!(
            decide(&event, "needle", true, "", &cards, &tr, false).1,
            "anyMessage"
        );
        tr.any_message = false;
        tr.at_mention = false;
        tr.speaker_ids = vec![json!(event.user_id.unwrap())];
        tr.keywords = vec!["needle".into()];
        assert_eq!(
            decide(&event, "needle", false, "", &cards, &tr, false).1,
            "keyword:needle"
        );
    }

    #[test]
    fn ingress_segments_accept_numeric_mentions_and_preserve_signed_reply_ids() {
        for id in [json!(-1234), json!("-1234")] {
            let (text, at, reply) = plain_text(
                &json!([
                    {"type":"at","data":{"qq":9001}},
                    {"type":"reply","data":{"id":id}},
                    {"type":"text","data":{"text":"hello"}}
                ]),
                9001,
            );
            assert_eq!((text.as_str(), at, reply), ("hello", true, Some(-1234)));
        }
    }

    #[test]
    fn ingress_voice_segment_reaches_the_model_as_the_baseline_voice_label() {
        let (text, at, reply) =
            plain_text(&json!([{"type":"record","data":{"file":"x.silk"}}]), 9001);
        assert_eq!((text.as_str(), at, reply), ("[语音]", false, None));
        let (text, _, _) = plain_text(
            &json!([{"type":"text","data":{"text":"听 "}},{"type":"record","data":{}}]),
            9001,
        );
        assert_eq!(text, "听 [语音]");
    }

    #[test]
    fn ingress_protocol_identities_accept_strings_and_reject_malformed_present_values() {
        for id in [serde_json::json!(9001), serde_json::json!("9001")] {
            let event: super::Event = serde_json::from_value(serde_json::json!({
                "user_id": id, "self_id": "9002", "group_id": "420042",
                "sender": {"user_id": "9001"}
            }))
            .expect("valid protocol identity");
            assert_eq!(event.user_id, Some(9001));
            assert_eq!(event.sender.unwrap().user_id, Some(9001));
        }
        for field in ["user_id", "self_id", "group_id", "sender_id", "target_id"] {
            for bad in [
                serde_json::json!(0),
                serde_json::json!(-1),
                serde_json::json!("oops"),
                serde_json::json!(1.5),
                serde_json::json!(true),
                serde_json::json!(9_007_199_254_740_992_i64),
            ] {
                assert!(
                    serde_json::from_value::<super::Event>(serde_json::json!({field: bad})).is_err(),
                    "{field}"
                );
            }
        }
    }

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
    fn publish_live_heartbeat_transitions_waiting_for_gateway_to_ready() {
        let root = isolated_runtime_fixture("live-heartbeat-state");
        let context = runtime::RuntimeContext::from_root(&root)
            .unwrap()
            .with_console_port(43112);
        let groups = vec!["group:42".to_string()];
        let marker = lifecycle::OwnerMarker {
            v: lifecycle::FORMAT_VERSION,
            role: "live".into(),
            identity: lifecycle::ProcessIdentity {
                pid: std::process::id(),
                start: 1,
                exe: "her".into(),
            },
            acquired_at: 0,
            refreshed_at: 0,
            groups: Some(groups.clone()),
        };

        publish_live_heartbeat(&context, "rust-heartbeat-live.json", &groups, false);
        let waiting_bytes = std::fs::read(root.join("data/rust-heartbeat-live.json")).unwrap();
        let waiting_json: Value = serde_json::from_slice(&waiting_bytes).unwrap();
        assert_eq!(waiting_json["state"], "waiting-for-gateway");
        assert_eq!(waiting_json["gatewayConnected"], false);
        assert_eq!(waiting_json["live"], true);
        assert_eq!(waiting_json["groups"], json!(["group:42"]));
        assert_eq!(waiting_json["pid"], std::process::id());
        assert_eq!(
            watchdog::read_heartbeat_for_owner(Ok(waiting_bytes), Some(&marker), now_ms() as i64),
            watchdog::Heartbeat::WaitingForGateway {
                live: true,
                groups: groups.clone(),
            }
        );

        publish_live_heartbeat(&context, "rust-heartbeat-live.json", &groups, true);
        let ready_bytes = std::fs::read(root.join("data/rust-heartbeat-live.json")).unwrap();
        let ready_json: Value = serde_json::from_slice(&ready_bytes).unwrap();
        assert_eq!(ready_json["state"], "ready");
        assert_eq!(ready_json["gatewayConnected"], true);
        assert_eq!(
            watchdog::read_heartbeat_for_owner(Ok(ready_bytes), Some(&marker), now_ms() as i64),
            watchdog::Heartbeat::Fresh {
                live: true,
                groups,
            }
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hotfix128_wrapped_fix1_structured_strings_are_masked_before_serialization() {
        let root = isolated_runtime_fixture("wrapped-log-128");
        std::fs::write(
            root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#,
        )
        .unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        for newline in ["\n", "\r\n"] {
            let record = json!({
                "message": format!("prefix known-structured- {newline} token-128 suffix"),
                "nested": [{"detail": format!("known-structured-{newline}token-128"), "ok": true}],
                "count": 2
            });
            append_runtime_json_line(&context, "logs/wrapped128.jsonl", "", &record).unwrap();
        }
        let raw = std::fs::read_to_string(root.join("data/logs/wrapped128.jsonl")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let values: Vec<Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            values,
            vec![json!({"message":"prefix *** suffix","nested":[{"detail":"***","ok":true}],"count":2}); 2]
        );
    }

    #[test]
    fn hotfix128_structured_decision_logs_mask_tokens_with_the_guard_off() {
        let root = isolated_runtime_fixture("token-log-128");
        std::fs::write(
            root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"fixture-console-token-128","security":{"outboundGuard":false}}"#,
        )
        .unwrap();
        let context = runtime::RuntimeContext::from_root(&root).unwrap();
        let shape = "abcdef".repeat(8);
        append_runtime_json_line(
            &context,
            "logs/decision128.jsonl",
            "",
            &json!({
                "message": format!("ordinary known-structured-token-128 {shape}"),
                "count": 2
            }),
        )
        .unwrap();
        let raw = std::fs::read_to_string(root.join("data/logs/decision128.jsonl")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            value,
            json!({"message":"ordinary *** ***", "count":2}),
            "{raw}"
        );
    }

    #[test]
    fn hotfix128_fix2_snapshot_debug_and_json_diagnostics_mask_escaped_breaks() {
        const CHILD_ROOT: &str = "HER_FIX2_SNAPSHOT_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let context = runtime::RuntimeContext::from_root(std::path::Path::new(&root)).unwrap();
            activity::set_log_context(&context);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::dsh::dump_snapshot(
                    context.dependency_url("dsh").unwrap(),
                    "synthetic-cookie",
                    "snapshot-fix2",
                ))
                .unwrap();
            let wrapped = context
                .read_text(runtime::RuntimeArea::Data, "snapshot-text")
                .unwrap();
            let diagnostic = format!("snapshot text={wrapped:?}");
            activity::append(&context, &diagnostic);
            append_runtime_json_line(
                &context,
                "logs/snapshot128.jsonl",
                "",
                &json!({"snapshot": diagnostic, "count": 2}),
            )
            .unwrap();
            return;
        }
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for separator in ["\n", "\r\n"] {
            let fixture = Fixture(isolated_runtime_fixture("snapshot-fix2"));
            let root = &fixture.0;
            let wrapped = format!("snapshot K128session{separator}VALUExyz end");
            std::fs::write(
                root.join("data/social-v2.json"),
                json!({"conversations":{
                    "group:42":{"agentToken":"K128sessionVALUExyz"}
                }})
                .to_string(),
            )
            .unwrap();
            std::fs::write(root.join("data/snapshot-text"), &wrapped).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            std::fs::write(
                root.join("dependencies.json"),
                json!({"dsh":base,"model":base,"onebot":base}).to_string(),
            )
            .unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "snapshot client did not connect"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("snapshot listener: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tokio_tungstenite::tungstenite::accept(stream).unwrap();
                let open: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(open["endpoint"], "session/follow");
                let frame = json!({"type":"item","streamId":open["streamId"],"value":{"type":"snapshot","records":[
                    {"event":{"type":"assistant/message","seq":1,"data":{"turn":1,"message":{"content":[{"type":"text","text":wrapped}]}}}},
                    {"event":{"type":"error","seq":2,"data":{"message":wrapped}}}
                ]}});
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        frame.to_string(),
                    ))
                    .unwrap();
            });
            let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
                .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
                .env(CHILD_ROOT, root)
                .output()
                .unwrap();
            server.join().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.contains("[dump] snapshot")
                    && text.contains("assistant/message")
                    && text.contains("error"),
                "{text}"
            );
            assert!(
                !text.contains("K128session") && !text.contains("VALUExyz"),
                "snapshot leaked: {text}"
            );
            assert_eq!(
                text.matches("***").count(),
                2,
                "one debug and one JSON mask: {text}"
            );
            for name in ["data/qq-activity.log", "data/logs/snapshot128.jsonl"] {
                let text = std::fs::read_to_string(root.join(name)).unwrap();
                assert!(
                    !text.contains("K128session")
                        && !text.contains("VALUExyz")
                        && text.contains("***"),
                    "{name}: {text}"
                );
            }
            let record: Value = serde_json::from_str(
                &std::fs::read_to_string(root.join("data/logs/snapshot128.jsonl")).unwrap(),
            )
            .unwrap();
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
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
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
        assert!(gate.is_file());
        first.refresh(123).unwrap();
        assert_eq!(first.marker().refreshed_at, 123);
        let marker_path = root.join("data/logs/rbridge-live.lock.owner.json");
        let marker: Value = serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
        assert_eq!(marker["identity"]["pid"], std::process::id());
        assert_eq!(marker["refreshedAt"], 123);
        drop(first);
        assert_eq!(std::fs::read_to_string(&gate).unwrap(), "crashed-owner\n1");

        assert!(
            gate.is_file(),
            "the persistent gate is never unlinked on normal drop"
        );
        let second = acquire_single_instance(&context)
            .expect("dropping the old handle releases ownership immediately");
        drop(second);
        assert!(gate.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollout_mismatch_catches_silent_dead_air() {
        let cfg = json!({"rollout": {"enabled": true, "rustGroups": ["group:1", "group:2"]}});
        let miss = rollout_mismatch(&cfg, &["group:1".to_string()]).expect("missing group reported");
        assert_eq!(miss, vec!["group:2".to_string()]);

        let ok = rollout_mismatch(&cfg, &["group:1".to_string(), "group:2".to_string()]);
        assert!(ok.is_none());

        assert!(rollout_mismatch(&json!({}), &[]).is_none());

        let off = json!({"rollout": {"enabled": false, "rustGroups": ["group:9"]}});
        assert!(rollout_mismatch(&off, &[]).is_none());

        let empty = json!({"rollout": {"enabled": true, "rustGroups": []}});
        assert!(rollout_mismatch(&empty, &[]).is_none());
    }

    #[test]
    fn t186_startup_reports_each_rollout_scope_gap() {
        let context = runtime::RuntimeContext::legacy();
        let split = json!({
            "allow": {"groups": [420042], "private": ["7001"]},
            "rollout": {"enabled": true, "rustGroups": ["group:420042", "group:420044"]}
        });
        let lines = report_rollout_scope(&context, &split);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("group:420044") && lines[1].contains("private:7001"),
            "{lines:?}"
        );
        let agreed = json!({"allow": {"groups": [420042]}, "rollout": {"enabled": true, "rustGroups": ["group:420042"]}});
        assert!(report_rollout_scope(&context, &agreed).is_empty());
    }

    #[test]
    fn t186_replay_after_reconnect_follows_the_configuration_read_now() {
        let item = |conversation: &str, id: &str| inbound::Replayable {
            event_key: format!("{conversation}|message:{id}"),
            conversation: conversation.to_string(),
            raw: "{}".to_string(),
        };
        let items = vec![
            item("group:420042", "1"),
            item("group:420043", "2"),
            item("group:420044", "3"),
            item("private:7001", "4"),
        ];
        let cfg = json!({"allow": {"groups": [420042, 420043], "private": ["7001"]}, "deny": {"groups": [420043]}});
        let (replay, settle) = split_replayable(Some(&cfg), items.clone());
        assert_eq!(
            replay.iter().map(|r| r.conversation.as_str()).collect::<Vec<_>>(),
            ["group:420042", "private:7001"],
            "admitted conversations replay, in order"
        );
        assert_eq!(
            settle,
            ["group:420043|message:2", "group:420044|message:3"],
            "denied and unlisted events are settled"
        );
        let (replay, settle) = split_replayable(None, items);
        assert!(
            replay.is_empty() && settle.is_empty(),
            "an unreadable configuration decides nothing"
        );
    }

    #[test]
    fn t186_gap_lists_follow_the_configuration_and_keep_the_last_on_a_read_failure() {
        let last = (vec!["420042".to_string()], vec!["7001".to_string()]);
        let cfg = json!({"allow": {"groups": [420042, 420043, 420044], "private": []}, "deny": {"groups": [420044]}});
        assert_eq!(
            gap_lists(Ok(cfg), &last),
            (vec!["420042".to_string(), "420043".to_string()], Vec::new()),
            "a group added mid-run gets its gaps recorded; a denied one does not"
        );
        assert_eq!(
            gap_lists(Err("runtime config is invalid"), &last),
            last,
            "an unreadable configuration keeps the last lists"
        );
    }

    #[test]
    fn t192_held_events_are_the_messages_and_pokes_of_a_conversation() {
        let event = |frame: Value| serde_json::from_value::<Event>(frame).unwrap();
        let cases = [
            (
                json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001}),
                Some("group:420042"),
            ),
            (
                json!({"post_type":"message","message_type":"private","user_id":7001}),
                Some("private:7001"),
            ),
            (
                json!({"post_type":"notice","notice_type":"notify","sub_type":"poke","group_id":420042,"user_id":1001}),
                Some("group:420042"),
            ),
            (
                json!({"post_type":"notice","notice_type":"notify","sub_type":"poke","sender_id":7001}),
                Some("private:7001"),
            ),
            (
                json!({"post_type":"notice","notice_type":"notify","sub_type":"poke","sender":{"user_id":7002}}),
                Some("private:7002"),
            ),
            (
                json!({"post_type":"message","message_type":"group","user_id":1001}),
                None,
            ),
            (
                json!({"post_type":"notice","notice_type":"group_increase","group_id":420042}),
                None,
            ),
            (
                json!({"post_type":"meta_event","meta_event_type":"heartbeat"}),
                None,
            ),
        ];
        for (frame, expected) in cases {
            assert_eq!(
                held_conversation(&event(frame.clone())).as_deref(),
                expected,
                "{frame}"
            );
        }
    }

    #[test]
    fn t192_at_most_the_held_limit_stays_in_memory() {
        let mut held: Vec<String> = (0..HELD_LIMIT - 1)
            .map(|n| format!("group:420042|message:{n}"))
            .collect();
        assert!(
            hold(&mut held, "group:420042|message:last".to_string()),
            "the event that reaches the limit is held"
        );
        assert_eq!(held.len(), HELD_LIMIT);
        assert!(
            hold(&mut held, "group:420042|message:0".to_string()),
            "an event held already stays held"
        );
        assert_eq!(held.len(), HELD_LIMIT, "and is not held twice");
        assert!(
            !hold(&mut held, "group:420042|message:over".to_string()),
            "one past the limit waits for the next connection"
        );
        assert_eq!(held.len(), HELD_LIMIT);
    }

    #[test]
    fn t192_held_events_replay_in_saved_order_once_the_configuration_reads() {
        let item = |conversation: &str, id: &str| inbound::Replayable {
            event_key: format!("{conversation}|message:{id}"),
            conversation: conversation.to_string(),
            raw: "{}".to_string(),
        };
        let saved = vec![
            item("group:420042", "1"),
            item("group:420042", "2"),
            item("group:420043", "3"),
            item("group:420042", "4"),
        ];
        let held = vec![
            "group:420042|message:4".to_string(),
            "group:420043|message:3".to_string(),
            "group:420042|message:1".to_string(),
        ];
        let cfg = json!({"allow": {"groups": [420042, 420043]}, "deny": {"groups": [420043]}});
        let (replay, settle) = take_held(&cfg, &held, saved);
        assert_eq!(
            replay.iter().map(|r| r.event_key.as_str()).collect::<Vec<_>>(),
            ["group:420042|message:1", "group:420042|message:4"],
            "held events of admitted conversations replay in saved order"
        );
        assert_eq!(
            settle,
            ["group:420043|message:3"],
            "a held event of a conversation denied meanwhile is settled"
        );
    }

    #[test]
    fn t192_refusals_are_recorded_at_most_once_a_minute() {
        let start = std::time::Instant::now();
        let at = |ms: u64| start + std::time::Duration::from_millis(ms);
        let mut refusals = UnreadableRefusals::default();
        assert_eq!(
            refusals.note(at(0)),
            Some(1),
            "the first refusal is recorded at once"
        );
        assert_eq!(refusals.note(at(1_000)), None);
        assert_eq!(
            refusals.flush(at(2_000)),
            None,
            "a recovery within the minute records nothing yet"
        );
        assert_eq!(
            refusals.note(at(3_000)),
            None,
            "nor does a refusal after that recovery"
        );
        assert_eq!(refusals.flush(at(59_999)), None, "just under a minute: not yet");
        assert_eq!(
            refusals.flush(at(60_000)),
            Some(2),
            "a minute after the last record: the two since then"
        );
        assert_eq!(refusals.flush(at(60_001)), None, "nothing is recorded twice");
        assert_eq!(
            refusals.note(at(61_000)),
            None,
            "the next record is a minute after this one"
        );
        assert_eq!(refusals.note(at(119_999)), None);
        assert_eq!(refusals.note(at(120_000)), Some(3));
    }

    #[test]
    fn t192_a_served_event_waits_behind_held_events_even_once_the_configuration_reads() {
        let cases = [
            ((true, true, true, false), Ingress::Hold),
            ((true, true, false, false), Ingress::Handle),
            ((false, true, false, false), Ingress::Hold),
            ((false, true, true, false), Ingress::Hold),
            ((true, false, true, true), Ingress::Handle),
            ((false, false, false, true), Ingress::Refuse),
            ((false, false, true, false), Ingress::Skip),
        ];
        for ((readable, holds, held_any, refusable), expected) in cases {
            assert_eq!(
                ingress_for(readable, holds, held_any, refusable),
                expected,
                "readable={readable} holds={holds} held_any={held_any} refusable={refusable}"
            );
        }
    }

    #[test]
    fn t192_live_all_is_served_and_held_like_the_reserved2_hub_serves_it() {
        let listed = vec!["group:420042".to_string()];
        assert!(reserved2_serves(false, &listed, "group:420042"));
        assert!(
            !reserved2_serves(false, &listed, "group:420043"),
            "an unlisted conversation without --live-all"
        );
        assert!(
            reserved2_serves(true, &listed, "group:420043"),
            "--live-all serves an unlisted conversation, as Hub::taken does"
        );
        assert!(
            reserved2_serves(true, &[], "private:7001"),
            "--live-all with no explicit groups"
        );
        assert!(
            !reserved2_serves(true, &[], ""),
            "a frame without a conversation is never served"
        );
        assert_eq!(
            ingress_for(false, reserved2_serves(true, &[], "group:420043"), false, false),
            Ingress::Hold
        );
    }

    #[test]
    fn simulation_bridge_unifies_isolated_and_fallback_modes_and_resets_connection() {
        let root = isolated_runtime_fixture("sim-bridge");
        let isolated = runtime::RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&isolated).unwrap());
        let mut isolated_bridge =
            SimulationBridge::new(&isolated, journal.clone(), vec!["group:42".into()], false);
        assert!(isolated_bridge.is_isolated());
        isolated_bridge.note_event();
        assert_eq!(isolated_bridge.counters(), (1, 0));
        isolated_bridge.reset_connection();
        assert_eq!(isolated_bridge.counters(), (0, 0));

        let legacy = runtime::RuntimeContext::legacy();
        let mut fallback_bridge =
            SimulationBridge::new(&legacy, journal, vec!["group:42".into()], false);
        assert!(!fallback_bridge.is_isolated());
        if let SimulationBridge::Fallback(ref mut fallback) = fallback_bridge {
            fallback.pending_live.push(("group:42".into(), "atMention".into()));
            fallback.in_flight.push("group:42|message:1".into());
        }
        fallback_bridge.note_event();
        assert_eq!(fallback_bridge.counters(), (1, 0));
        fallback_bridge.reset_connection();
        assert_eq!(fallback_bridge.counters(), (0, 0));
        if let SimulationBridge::Fallback(ref fallback) = fallback_bridge {
            assert!(fallback.pending_live.is_empty());
            assert!(fallback.in_flight.is_empty());
        }
        drop(isolated_bridge);
        drop(fallback_bridge);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn ev(group: bool, text: &str) -> Event {
        Event {
            post_type: "message".into(),
            message_type: if group { "group".into() } else { "private".into() },
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
        // ASCII word boundaries treat non-ASCII Han characters as non-word boundaries.
        assert!(keyword_hit("ds好", "DS"));
        assert!(keyword_hit("用ds画图", "DS"));
        assert!(!keyword_hit("ads_", "DS"));
        assert!(!keyword_hit("_ds", "DS"));
    }

    #[test]
    fn keyword_cjk_substring() {
        assert!(keyword_hit("大鲸鱼来了", "鲸鱼"));
        assert!(!keyword_hit("大肥鱼来了", "鲸鱼"));
    }

    /// Matrix pinning the four OneBot event shapes handled by `decide`.
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

        let (w, why) = decide(
            &shape("message", "group", "", Some(1)),
            "嗯",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(!w, "群消息无触发应不醒");
        assert_eq!(why, "no-trigger");

        let (w2, why2) = decide(
            &shape("message", "group", "", Some(1)),
            "Fixture Bot在吗",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(w2);
        assert_eq!(why2, "nameMention");

        let (w3, why3) = decide(
            &shape("message", "private", "", None),
            "随便说点",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(w3);
        assert_eq!(why3, "private");

        let (w4, why4) = decide(
            &shape("notice", "", "poke", Some(1)),
            "",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(w4, "群拍一拍必须醒");
        assert_eq!(why4, "poke");

        let (w5, why5) = decide(
            &shape("notice", "", "poke", None),
            "",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(w5, "私聊拍一拍也应醒");
        assert_eq!(why5, "poke");

        let (w6, why6) = decide(
            &shape("notice", "", "group_recall", Some(1)),
            "",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(!w6, "非拍一拍的 notice 不该醒");
        assert_eq!(why6, "no-trigger");

        let (w7, why7) = decide(
            &shape("meta_event", "", "", None),
            "",
            false,
            "Fixture Bot",
            &nicks,
            &t,
            false,
        );
        assert!(!w7);
        assert_eq!(why7, "no-trigger");
    }

    #[test]
    fn group_poke_notice_wakes() {
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

        let me = Event {
            post_type: "notice".into(),
            sub_type: "poke".into(),
            group_id: Some(7003),
            sender_id: Some(7004),
            target_id: Some(7001),
            self_id: Some(7004),
            ..Default::default()
        };
        let (wake2, why2) = decide(&me, "", false, "Fixture Bot", &HashMap::new(), &t, false);
        assert!(!wake2);
        assert_eq!(why2, "poke:selfskip");

        let mut off = tr();
        off.poke = false;
        let (wake3, _) = decide(&e, "", false, "Fixture Bot", &HashMap::new(), &off, false);
        assert!(!wake3);
    }

    #[test]
    fn name_mention_hits() {
        let e = ev(true, "Fixture Bot你在吗");
        let (wake, why) = decide(
            &e,
            "Fixture Bot你在吗",
            false,
            "Fixture Bot",
            &HashMap::new(),
            &tr(),
            false,
        );
        assert!(wake);
        assert_eq!(why, "nameMention");
    }

    #[test]
    fn question_requires_direction_and_the_existing_switch() {
        let e = ev(true, "你有空吗？");
        assert!(!decide(&e, "你有空吗？", false, "Fixture Bot", &HashMap::new(), &tr(), false).0);
        let (wake, why) = decide(
            &e,
            "你有空吗？",
            false,
            "Fixture Bot",
            &HashMap::new(),
            &tr(),
            true,
        );
        assert!(wake);
        assert_eq!(why, "question");
        let mut off = tr();
        off.question = false;
        assert!(!decide(&e, "你有空吗？", false, "Fixture Bot", &HashMap::new(), &off, true).0);
    }

    #[test]
    fn plain_group_message_does_not_wake() {
        let e = ev(true, "今天天气不错");
        let (wake, why) = decide(
            &e,
            "今天天气不错",
            false,
            "Fixture Bot",
            &HashMap::new(),
            &tr(),
            false,
        );
        assert!(!wake);
        assert_eq!(why, "no-trigger");
    }

    #[test]
    fn private_always_wakes() {
        let e = ev(false, "嗯");
        let (wake, why) = decide(&e, "嗯", false, "Fixture Bot", &HashMap::new(), &tr(), false);
        assert!(wake);
        assert_eq!(why, "private", "私聊唤醒原因固定为 private");
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
