//! First-generation simulation. State is shared with the companion console under one OS gate;
//! only the live hub schedules turns. Timers and jobs never survive a process generation.
use crate::{
    chat, console,
    inbound::SharedJournal,
    pipeline::Pipeline,
    runtime::{RuntimeArea, RuntimeContext},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

const FILE: &str = "social-v1.json";
const SPLIT_HINT: &str = "想分多条消息时用空格分隔；不想分条就不要加空格，用标点连接。注意：中英文/数字之间的空格也会被当作分条信号。";
const DIRECTION: &str = "注意：消息里的 [引用 某人：...] 表示这句话是在回应被引用的人；引用的是你的消息才是在找你，引用别人时别默认是在找你。";
fn now() -> i64 {
    crate::parity_clock::now_ms()
}

pub fn config(cfg: &Value) -> Value {
    let mut v = json!({"enabled":true,"triggerProbability":0.1,"contextWindow":20,
        "activeCheckMinMs":10000,"activeCheckMaxMs":30000,"activeReplyDelayMinMs":2000,"activeReplyDelayMaxMs":8000,
        "activeDurationEnabled":true,"activeDurationMinMs":900000,"activeDurationMaxMs":1800000,
        "idleWindowMs":360000,"idleRetryProbability":0.25,"idleRetryWaitMs":120000,
        "proactiveEnabled":true,"proactiveIdleThresholdMs":1800000,"proactiveCheckMinMs":2700000,"proactiveCheckMaxMs":5400000,
        "proactiveProbability":0.2,"skipProbability":0.15,"surrenderProbability":0,"maxReplyChars":500,
        "mustReplyKeywords":["deepseek","小鲸鱼","大肥鱼","鲸鱼","d指导","在吗"],
        "burstEnabled":true,"burstIntervalMinMs":1000,"burstIntervalMaxMs":3000,"longGapProbability":0.1,"longGapMinMs":2500,"longGapMaxMs":5000});
    if let Some(extra) = cfg.get("social").and_then(Value::as_object) {
        v.as_object_mut().unwrap().extend(extra.clone());
    }
    v
}
fn n(cfg: &Value, name: &str) -> i64 {
    console::social_v1_number(&cfg[name])
        .max(0.0)
        .min(i64::MAX as f64) as i64
}
fn probability(cfg: &Value, name: &str) -> f64 {
    console::social_v1_number(&cfg[name]).clamp(0.0, 1.0)
}
// The stream belongs to the shared driver state, so admin and scheduler draws stay serialized.
fn initial_seed() -> u32 {
    #[cfg(feature = "parity-clock")]
    if let Ok(seed) = std::env::var("HER_PARITY_RANDOM_SEED") {
        if let Ok(seed) = seed.parse::<u32>() {
            return seed.max(1);
        }
    }
    (now() as u32).max(1)
}
fn random(seed: &mut u32) -> f64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed as f64 / 4294967296.0
}
fn range(seed: &mut u32, cfg: &Value, lo: &str, hi: &str) -> i64 {
    let (a, b) = (n(cfg, lo), n(cfg, hi));
    let (a, b) = (a.min(b), a.max(b));
    a.saturating_add((random(seed) * (b.saturating_sub(a) as f64 + 1.0)).floor() as i64)
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Message {
    sender: String,
    text: String,
    plain: String,
    quote_target_is_self: bool,
    is_owner: bool,
    media: Vec<Value>,
    message_id: String,
    event_key: String,
    time: i64,
}
impl Message {
    fn line(&self, owner: bool) -> String {
        format!(
            "{}{}：{}{}",
            if owner && self.is_owner {
                "【管理员】"
            } else {
                ""
            },
            self.sender,
            self.text,
            chat::media_hint(&self.message_id, self.media.len())
        )
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Conversation {
    phase: String,
    last_check_at: i64,
    next_check_at: i64,
    last_active_message_at: i64,
    active_entered_at: i64,
    active_deadline_at: i64,
    active_exit_at: i64,
    last_ai_reply_at: i64,
    probe_deadline: i64,
    proactive_next_check_at: i64,
    generation: String,
    recent: Vec<Message>,
    summary: Vec<Message>,
    silent: Vec<Message>,
}
impl Default for Conversation {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            last_check_at: 0,
            next_check_at: 0,
            last_active_message_at: 0,
            active_entered_at: 0,
            active_deadline_at: 0,
            active_exit_at: 0,
            last_ai_reply_at: 0,
            probe_deadline: 0,
            proactive_next_check_at: 0,
            generation: crate::turn_ledger::mint_id("v1-"),
            recent: vec![],
            summary: vec![],
            silent: vec![],
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct State {
    owner: String,
    seed: u32,
    console_config_stamp: String,
    console_social_config: Option<Value>,
    conversations: BTreeMap<String, Conversation>,
    flush: BTreeMap<String, Flush>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Flush {
    key: String,
    status: String,
    remaining: usize,
}
fn read(context: &RuntimeContext) -> Result<State, String> {
    match context
        .read_optional_text(RuntimeArea::Data, FILE)
        .map_err(|e| e.to_string())?
    {
        Some(s) => serde_json::from_str(&s).map_err(|_| "reserved state is invalid".into()),
        None => Ok(State::default()),
    }
}
fn mutate<T>(
    context: &RuntimeContext,
    f: impl FnOnce(&mut State) -> Result<T, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, "locks/social-v1.lock") {
            Ok(g) => break g,
            Err(e) if e == "runtime file is already locked" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(format!("reserved state gate: {e}")),
        }
    };
    let mut state = read(context)?;
    let result = f(&mut state)?;
    context
        .write_atomic(
            RuntimeArea::Data,
            FILE,
            serde_json::to_vec_pretty(&state)
                .map_err(|e| e.to_string())?
                .as_slice(),
        )
        .map_err(|e| e.to_string())?;
    Ok(result)
}
fn enter(c: &mut Conversation, seed: &mut u32, cfg: &Value, key: &str, at: i64) {
    c.phase = "active".into();
    c.last_check_at = at;
    c.next_check_at = at.saturating_add(range(seed, cfg, "activeCheckMinMs", "activeCheckMaxMs"));
    c.last_active_message_at = at;
    c.active_entered_at = at;
    c.probe_deadline = 0;
    c.active_deadline_at = if key.starts_with("group:") && cfg["activeDurationEnabled"] != false {
        at.saturating_add(range(
            seed,
            cfg,
            "activeDurationMinMs",
            "activeDurationMaxMs",
        ))
    } else {
        0
    };
    c.active_exit_at = 0;
}
fn leave(c: &mut Conversation) {
    let recent = std::mem::take(&mut c.recent);
    let summary = std::mem::take(&mut c.summary);
    *c = Conversation::default();
    c.recent = recent;
    c.summary = summary;
}
fn finish_exit(c: &mut Conversation) {
    c.phase = "idle".into();
    c.active_deadline_at = 0;
    c.active_exit_at = 0;
}
fn context_block(c: &Conversation, cfg: &Value) -> String {
    let lines = c
        .recent
        .iter()
        .rev()
        .take(n(cfg, "contextWindow") as usize)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| m.line(true))
        .collect::<Vec<_>>()
        .join("\n");
    if lines.is_empty() {
        "（无）".into()
    } else {
        lines
    }
}
fn role_prefix(context: &RuntimeContext, cfg: &Value, key: &str) -> String {
    let hint = crate::prompt::identity_hint_v1(context, cfg, key);
    if hint.is_empty() {
        hint
    } else {
        hint + "\n\n"
    }
}
fn instruction(direct: bool) -> String {
    if direct {
        "请回复消息。".into()
    } else {
        "请根据情况决定是否回复。如果不需要回应、想潜水/不接话，请只输出 [SILENT]；否则正常回复。"
            .into()
    }
}
fn must_reply(text: &str, nick: &str, cfg: &Value) -> bool {
    let text = text.to_lowercase();
    if !nick.is_empty() && text.contains(&nick.to_lowercase()) {
        return true;
    }
    if cfg["mustReplyKeywords"].as_array().is_some_and(|ks| {
        ks.iter().any(|k| {
            text.contains(
                &k.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| k.to_string())
                    .to_lowercase(),
            )
        })
    }) {
        return true;
    }
    // Frozen JS isDirectedAtAi, including follow-up pressure without a name.
    static RE: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let r=RE.get_or_init(|| [r"deepseek|claude|chatgpt|gpt|大肥鱼|小鲸鱼|鲸鱼|d指导|d老师|d师傅|深度求索|\bds\b|ai|人工智障|机器人|模型",r"强|弱|行不行|能不能|会不会|是不是|一半|水平|垃圾|废物|白嫖|菜|不如|厉害|赢|输|[?？吗呢吧]|怎么|为什么|哪|谁",r"(你|您).{0,10}(吗|呢|？|\?|怎么|是不是|能不能|行不行|有没有|有|没有|比|不如|强|弱|一半|厉害|垃圾|菜|赢|输)|^(你|您)(是不是|行不行|能不能|会不会|觉得|有|没有)|(你|您)是[^？?。！!]{0,14}(还是|或者|吗|么|？|\?)|怎么不说话|人呢|回我|说话啊|理我|别装死|在不在|装死|说话"].iter().map(|s|regex::Regex::new(s).unwrap()).collect());
    r[0].is_match(&text) && r[1].is_match(&text) || r[2].is_match(&text)
}

fn config_stamp(context: &RuntimeContext) -> Result<String, String> {
    let path = context.resolve_path(RuntimeArea::Config, "config.json", crate::runtime::PathIntent::ReadFile)
        .map_err(str::to_string)?;
    let metadata = std::fs::metadata(path).map_err(|_| "reserved config metadata is unavailable".to_string())?;
    let modified = metadata.modified().map_err(|_| "reserved config timestamp is unavailable".to_string())?
        .duration_since(std::time::UNIX_EPOCH).map_err(|_| "reserved config timestamp is invalid".to_string())?;
    Ok(format!("{}:{}", modified.as_nanos(), metadata.len()))
}

/// Frozen JS starts with defaults, but HTTP hot reload replaces top-level social wholesale.
pub fn console_config(context: &RuntimeContext, cfg: &Value) -> Result<Value, String> {
    let stamp = config_stamp(context)?;
    let state = read(context)?;
    if state.console_config_stamp == stamp {
        if let Some(config) = state.console_social_config {
            return Ok(config);
        }
    }
    mutate(context, |s| {
        if s.console_social_config.is_none() {
            s.console_social_config = Some(config(cfg));
        } else if s.console_config_stamp != stamp {
            // Object.assign(cfg, fresh) retains social when the file omits that property.
            if let Some(social) = cfg.get("social") {
                s.console_social_config = Some(social.clone());
            }
        }
        s.console_config_stamp = stamp;
        Ok(s.console_social_config.clone().unwrap())
    })
}

pub fn refresh_console_config(context: &RuntimeContext, cfg: &Value) -> Result<(), String> {
    if read(context)?.console_social_config.is_some() {
        console_config(context, cfg)?;
    }
    Ok(())
}

pub fn posted_console_config(context: &RuntimeContext, prior: Value, merged: &Value) -> Result<Value, String> {
    let mut next = console::social_v1_spread(Some(&prior));
    next.extend(console::social_v1_spread(Some(merged)));
    let next = Value::Object(next);
    mutate(context, |s| {
        s.console_social_config = Some(next.clone());
        // Keep the old stamp: JS observes its own write on the next HTTP request.
        Ok(next)
    })
}

pub fn view(context: &RuntimeContext, cfg: &Value) -> Result<Value, String> {
    let config = console_config(context, cfg)?;
    let state = read(context)?;
    Ok(
        json!({"config":config,"pendingSummaries":state.conversations.iter().filter(|(_,c)|!c.summary.is_empty()).map(|(k,c)|(k.clone(),c.summary.len())).collect::<BTreeMap<_,_>>(),
        "states":state.conversations.iter().filter(|(_,c)|c.phase!="idle"||!c.recent.is_empty()).map(|(k,c)|(k.clone(),json!({"phase":c.phase}))).collect::<BTreeMap<_,_>>()}),
    )
}
pub fn admin_state(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let v: Value = serde_json::from_str(body).map_err(|_| (400, "请求体不是合法 JSON".into()))?;
    let key = v["key"].as_str().unwrap_or("").trim();
    let phase = v["phase"].as_str().unwrap_or("");
    if key.is_empty() {
        return Err((400, "key 不能为空".into()));
    }
    if phase != "active" && phase != "idle" {
        return Err((400, "phase 必须是 active 或 idle".into()));
    }
    let cfg = context.load_config().map_err(|e| (503, e.to_string()))?;
    if !chat::allowed_key(&cfg, key) {
        return Err((403, "会话不在白名单内".into()));
    }
    mutate(context, |s| {
        let c = s.conversations.entry(key.into()).or_default();
        if phase == "active" {
            enter(c, &mut s.seed, &config(&cfg), key, now());
            // Everything already buffered precedes manual activation, even if the
            // companion console's parity clock starts later than the live clock.
            c.last_check_at = c.recent.iter().map(|m| m.time).max().unwrap_or(0).max(c.last_check_at);
        } else {
            settle_buffer(context, c)?;
            leave(c)
        };
        Ok(())
    })
    .map_err(|e| (503, e))?;
    Ok(json!({"ok":true,"key":key,"phase":phase}))
}
fn settle_buffer(context: &RuntimeContext, c: &Conversation) -> Result<(), String> {
    let keys = c
        .recent
        .iter()
        .map(|m| m.event_key.clone())
        .filter(|k| !k.is_empty())
        .collect::<Vec<_>>();
    crate::inbound::Journal::open(context)?.mark_handled(&keys, now())
}
/// Reset settles queued ingress before removing the old conversation generation.
pub fn reset_conversation(context: &RuntimeContext, key: &str) -> Result<(), String> {
    if context
        .read_optional_text(RuntimeArea::Data, FILE)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Ok(());
    }
    mutate(context, |s| {
        if let Some(c) = s.conversations.get(key) {
            settle_buffer(context, c)?;
        }
        s.conversations.remove(key);
        Ok(())
    })
}
pub fn admin_flush(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let v: Value = serde_json::from_str(body).map_err(|_| (400, "请求体不是合法 JSON".into()))?;
    let key = v["key"].as_str().unwrap_or("").trim().to_string();
    let id = crate::turn_ledger::mint_id("flush-");
    let needed = mutate(context, |s| {
        let needed = s
            .conversations
            .iter()
            .any(|(k, c)| (key.is_empty() || k == &key) && !c.summary.is_empty());
        if needed {
            s.flush.insert(
                id.clone(),
                Flush {
                    key,
                    status: "pending".into(),
                    remaining: 0,
                },
            );
        }
        Ok(needed)
    })
    .map_err(|e| (503, e))?;
    if !needed {
        return Ok(json!({"ok":true}));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = read(context).map_err(|e| (503, e))?;
        match state.flush.get(&id).map(|f| f.status.as_str()) {
            Some("done") => {
                let _ = mutate(context, |s| {
                    s.flush.remove(&id);
                    Ok(())
                });
                return Ok(json!({"ok":true}));
            }
            Some("failed") | None => return Err((503, "摘要投喂未完成，待摘要仍保留".into())),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err((503, "摘要投喂仍在等待 DSH".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Conversations the first-generation state holds (for access-change invalidation).
/// A state that cannot be read is an error: the caller must report that it could not revoke those keys.
pub fn known_keys(context: &RuntimeContext) -> Result<Vec<String>, String> {
    read(context).map(|s| s.conversations.keys().cloned().collect())
}

pub fn mode_stamp(context: &RuntimeContext) -> Result<String, String> {
    context
        .read_optional_text(RuntimeArea::Data, "mode.json")
        .map_err(|e| e.to_string())
        .map(|s| s.unwrap_or_default())
}
/// Retire the public first-generation state before acknowledging mode exit.
/// DSH cleanup is separate and must use only the captured obsolete epoch.
pub fn retire_mode_state(context: &RuntimeContext) -> Result<(), String> {
    mutate(context, |s| {
        s.conversations.clear();
        for f in s.flush.values_mut() {
            f.status = "failed".into();
        }
        Ok(())
    })
}
#[derive(Clone)]
pub struct Authority {
    owner: String,
    generation: String,
    mode: String,
    social: bool,
    pub token: String,
}
impl Authority {
    pub fn social(&self) -> bool {
        self.social
    }
    pub fn check_generation(&self, context: &RuntimeContext, key: &str) -> Result<(), String> {
        let cfg = context.load_config().map_err(|e| e.to_string())?;
        if console::runtime_mode(context) != "reserved"
            || mode_stamp(context)? != self.mode
            || (config(&cfg)["enabled"] != false) != self.social
            || !chat::allowed_key(&cfg, key)
        {
            return Err("reserved mode or permission changed".into());
        }
        let state = read(context)?;
        if state.owner != self.owner
            || state.conversations.get(key).map(|c| c.generation.as_str())
                != Some(self.generation.as_str())
        {
            return Err("reserved generation changed".into());
        }
        Ok(())
    }
    pub fn check(&self, context: &RuntimeContext, key: &str) -> Result<(), String> {
        self.check_generation(context, key)?;
        if console::conversation_token(context, key)?.as_deref() != Some(self.token.as_str()) {
            return Err("reserved token changed".into());
        }
        Ok(())
    }
}
#[derive(Clone)]
struct Job {
    key: String,
    prompt: String,
    media: Vec<Value>,
    due: i64,
    farewell: bool,
    summary: Vec<Message>,
    flush: String,
    authority: Authority,
    inbound: Vec<String>,
    attempt: u32,
}
/// Add a job to the pending queue. Each conversation holds at most `dsh_health::QUEUE_MAX` jobs (JS `QUEUE_MAX` per key);
/// beyond that its oldest are dropped and returned for the caller to settle. There is no process-wide cap: one
/// conversation's backlog never costs another its work.
fn push_bounded(pending: &mut VecDeque<Job>, job: Job) -> Vec<Job> {
    let cap = crate::dsh_health::QUEUE_MAX;
    let mut dropped = Vec::new();
    while pending.iter().filter(|j| j.key == job.key).count() >= cap {
        let Some(at) = pending.iter().position(|j| j.key == job.key) else { break };
        dropped.extend(pending.remove(at));
    }
    pending.push_back(job);
    dropped
}
struct Active {
    job: Job,
    task: tokio::task::JoinHandle<(Result<(), String>, bool)>,
}
pub struct Hub {
    context: RuntimeContext,
    journal: SharedJournal,
    lookups: chat::Lookups,
    pending: VecDeque<Job>,
    /// Jobs `push_bounded` dropped; settled (events handled, flush and farewell bookkeeping) by the next `tick`.
    dropped: Vec<Job>,
    /// Conversations already announced as waiting for DSH in this outage (one log line each).
    waiting_noted: std::collections::BTreeSet<String>,
    active: BTreeMap<String, Active>,
    groups: Vec<String>,
    all: bool,
    last_tick: i64,
    stamp: String,
    slang: crate::slang_learn::SlangLearning,
}
impl Hub {
    pub fn new(
        context: &RuntimeContext,
        journal: SharedJournal,
        groups: Vec<String>,
        all: bool,
    ) -> Result<Self, String> {
        let cfg = context.load_config().map_err(|e| e.to_string())?;
        let ob = crate::onebot::OneBot::try_new_url(
            context.dependency_url("onebot").ok_or("missing OneBot")?,
            cfg.pointer("/snowluma/accessToken")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
        )?;
        mutate(context, |s| {
            *s = State {
                owner: crate::turn_ledger::mint_id("process-"),
                seed: initial_seed(),
                console_config_stamp: config_stamp(context)?,
                console_social_config: Some(config(&cfg)),
                ..State::default()
            };
            Ok(())
        })?;
        Ok(Self {
            context: context.clone(),
            journal,
            lookups: chat::Lookups::new(ob),
            pending: VecDeque::new(),
            dropped: Vec::new(),
            waiting_noted: Default::default(),
            active: BTreeMap::new(),
            groups,
            all,
            last_tick: now(),
            stamp: mode_stamp(context)?,
            slang: {
                let slang = crate::slang_learn::SlangLearning::new(context);
                slang.start_dsh_watch();
                slang
            },
        })
    }
    fn allowed(&self, cfg: &Value, key: &str) -> bool {
        chat::allowed_key(cfg, key) && (self.all || self.groups.iter().any(|g| g == key))
    }
    fn job(
        &mut self,
        state: &State,
        key: &str,
        prompt: String,
        media: Vec<Value>,
        farewell: bool,
        summary: Vec<Message>,
        flush: String,
        delay: i64,
        inbound: Vec<String>,
    ) -> Result<(), String> {
        let token = console::ensure_or_rotate_agent_token(&self.context, key, false)?;
        let job = Job {
            key: key.into(),
            prompt,
            media,
            due: now().saturating_add(delay),
            farewell,
            summary,
            flush,
            authority: Authority {
                owner: state.owner.clone(),
                generation: state.conversations[key].generation.clone(),
                mode: self.stamp.clone(),
                social: config(&self.context.load_config().map_err(|e| e.to_string())?)["enabled"]
                    != false,
                token,
            },
            inbound,
            attempt: 0,
        };
        let dropped = push_bounded(&mut self.pending, job);
        self.dropped.extend(dropped);
        Ok(())
    }
    /// What a finished (or dropped) job leaves in the persisted state: the farewell phase, the summary it carried and the
    /// manual flush it belonged to.
    fn settle_state(&self, key: &str, job: &Job, failed: bool) -> Result<(), String> {
        mutate(&self.context, |s| {
            if let Some(c) = s.conversations.get_mut(key) {
                if c.generation == job.authority.generation {
                    if job.farewell {
                        finish_exit(c);
                    }
                    if !failed && !job.summary.is_empty() {
                        c.summary.retain(|m| {
                            !job.summary
                                .iter()
                                .any(|x| x.time == m.time && x.message_id == m.message_id)
                        });
                    }
                }
            }
            if let Some(f) = s.flush.get_mut(&job.flush) {
                if failed {
                    f.status = "failed".into();
                } else {
                    f.remaining = f.remaining.saturating_sub(1);
                    if f.remaining == 0 {
                        f.status = "done".into();
                    }
                }
            }
            Ok(())
        })
    }
    pub async fn handle(
        &mut self,
        inb: &chat::Inbound,
        event: &str,
        nick: &str,
    ) -> Result<(), String> {
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        let sc = config(&cfg);
        let key = format!("{}:{}", inb.kind, inb.id);
        if !self.allowed(&cfg, &key) {
            return Ok(());
        }
        self.sync_mode().await?;
        if console::runtime_mode(&self.context) != "reserved" {
            return Ok(());
        }
        let mut res = chat::GroupResolver {
            lookups: &mut self.lookups,
            group: (inb.kind == "group").then(|| inb.id.to_string()),
            kind: inb.kind.into(),
            conv: inb.id.to_string(),
            self_id: inb.self_id,
        };
        let text = chat::segments_to_text(&inb.message, &mut res, true);
        let plain = chat::segments_to_text(&inb.message, &mut res, false);
        let owner = inb.user_id.to_string() == chat::owner_qq(&cfg);
        let role = console::chat_role_state(&self.context)?;
        if plain.is_empty() && text.is_empty() || role["mode"] == "silent" && !owner {
            if !(plain.is_empty() && text.is_empty()) {
                crate::activity::append(
                    &self.context,
                    &format!("{key}（静默模式）群友 {}：{}", inb.user_id, crate::activity::slice(&text, 80)),
                );
            }
            self.journal.mark_handled(vec![event.into()], now()).await?;
            return Ok(());
        }
        let quote_self = inb.message.as_array().is_some_and(|a| {
            a.iter().any(|s| {
                s["type"] == "reply"
                    && res
                        .lookups
                        .reply_info(
                            inb.kind,
                            &inb.id.to_string(),
                            &s["data"]["id"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| s["data"]["id"].to_string()),
                            inb.self_id,
                        )
                        .is_some_and(|r| r.user_id == Some(inb.self_id.to_string()))
            })
        });
        // Share the public command decision; retain the reserved mode in every send guard.
        let current_role = role["role"].as_str().map(str::to_string);
        let role_mode = role["mode"].as_str().unwrap_or("active").to_string();
        let facts = chat::Facts {
            plain: &plain,
            is_owner: owner,
            quote_self,
            role: current_role.clone(),
            role_mode: role_mode.clone(),
            session: crate::session_map::lookup(&self.context, &key)?,
            allowed: true,
        };
        let decision = chat::decide(&facts, &|name| {
            self.context
                .resolve_path(
                    RuntimeArea::Resources,
                    &format!("roles/{name}.md"),
                    crate::runtime::PathIntent::ReadFile,
                )
                .map(|p| p.is_file())
                .unwrap_or(false)
        });
        let say = match decision {
            chat::Decision::Say(text) => Some(text),
            chat::Decision::SetRole { name, say } => {
                if crate::persona::applies(&self.context) {
                    Some(crate::persona_command::ROLE_RETIRED.to_string())
                } else {
                    console::chat_write_role_state(&self.context, name.as_deref(), &role_mode)?;
                    Some(say)
                }
            }
            chat::Decision::SetRoleMode { mode, say } => {
                console::chat_write_role_state(&self.context, current_role.as_deref(), mode)?;
                Some(say.into())
            }
            chat::Decision::Reset => {
                let reset = console::chat_reset_conversation(&self.context, &key);
                if reset.is_ok() {
                    mutate(&self.context, |s| {
                        s.conversations.remove(&key);
                        Ok(())
                    })?;
                    Some("已重置会话，下次消息将开新上下文".into())
                } else {
                    self.journal.mark_handled(vec![event.into()], now()).await?;
                    return Ok(());
                }
            }
            chat::Decision::Ignore(_) => {
                self.journal.mark_handled(vec![event.into()], now()).await?;
                return Ok(());
            }
            chat::Decision::Forward => None,
        };
        if let Some(text) = say {
            let cfg = self.context.load_config().map_err(|e| e.to_string())?;
            let ob = crate::onebot::OneBot::try_new_url(
                self.context
                    .dependency_url("onebot")
                    .ok_or("missing OneBot")?,
                cfg.pointer("/snowluma/accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            )?;
            chat::send_recorded(
                &self.context,
                &ob,
                &key,
                "reserved-command",
                "command",
                &text,
                chat::send_delay_ms(&cfg),
                &|| {
                    let current = self.context.load_config().map_err(|e| e.to_string())?;
                    if console::runtime_mode(&self.context) != "reserved"
                        || mode_stamp(&self.context)? != self.stamp
                        || !self.allowed(&current, &key)
                    {
                        Err("reserved command invalidated".into())
                    } else {
                        Ok(())
                    }
                },
            );
            self.journal.mark_handled(vec![event.into()], now()).await?;
            return Ok(());
        }
        if inb.kind == "group" && crate::slang_learn::enabled(&cfg) {
            let sender = if !inb.card.is_empty() {
                inb.card.clone()
            } else if !inb.nickname.is_empty() {
                inb.nickname.clone()
            } else {
                inb.user_id.to_string()
            };
            crate::feed_slang(&self.slang, true, &cfg, &key, &sender, &plain);
        }
        crate::activity::append(
            &self.context,
            &format!(
                "{key} {} {}：{}",
                if owner { "管理员" } else { "群友" },
                if inb.nickname.is_empty() { inb.user_id.to_string() } else { inb.nickname.clone() },
                crate::activity::slice(&text, 80)
            ),
        );
        if sc["enabled"] == false {
            let prompt = chat::build_prompt(
                &crate::prompt::identity_hint_v1(&self.context, &cfg, &key),
                owner,
                (inb.kind == "group").then_some(if !inb.card.is_empty() {
                    inb.card.as_str()
                } else {
                    inb.nickname.as_str()
                }),
                &text,
                &chat::media_hint(
                    inb.message_id.as_deref().unwrap_or(""),
                    chat::media_count(&inb.message),
                ),
            );
            let state = mutate(&self.context, |s| {
                s.conversations.entry(key.clone()).or_default();
                Ok(s.clone())
            })?;
            self.job(
                &state,
                &key,
                prompt,
                crate::forward_fmt::collect_segment_meta(
                    inb.message.as_array().map(Vec::as_slice).unwrap_or(&[]),
                )
                .0,
                false,
                vec![],
                String::new(),
                0,
                vec![event.into()],
            )?;
            return Ok(());
        }
        let at_self = inb.message.as_array().is_some_and(|a| {
            a.iter().any(|s| {
                s["type"] == "at"
                    && s["data"]["qq"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| s["data"]["qq"].to_string())
                        == inb.self_id.to_string()
            })
        });
        let direct = inb.kind == "private"
            || at_self
            || plain.contains(&format!("@{}", inb.self_id))
            || quote_self
            || must_reply(&plain, nick, &sc);
        let message = Message {
            sender: if inb.kind == "private" {
                "私聊".into()
            } else if !inb.card.is_empty() {
                inb.card.clone()
            } else if !inb.nickname.is_empty() {
                inb.nickname.clone()
            } else {
                inb.user_id.to_string()
            },
            text: crate::role::utf16_take(&text, 200),
            plain: crate::role::utf16_take(&plain, 200),
            quote_target_is_self: quote_self,
            is_owner: owner,
            media: crate::forward_fmt::collect_segment_meta(
                inb.message.as_array().map(Vec::as_slice).unwrap_or(&[]),
            )
            .0,
            message_id: inb.message_id.clone().unwrap_or_default(),
            event_key: event.into(),
            time: now(),
        };
        let context = self.context.clone();
        let mut job = None;
        let state = mutate(&context, |s| {
            let c = s.conversations.entry(key.clone()).or_default();
            c.recent.push(message.clone());
            let cap = (n(&sc, "contextWindow") * 2).max(50) as usize;
            if c.recent.len() > cap {
                c.recent.remove(0);
            }
            let prefix = role_prefix(&context, &cfg, &key);
            if c.phase == "exiting" {
                c.summary.push(message.clone());
            } else if c.phase == "active"
                && key.starts_with("group:")
                && sc["activeDurationEnabled"] != false
                && c.active_deadline_at > 0
                && now() >= c.active_deadline_at
            {
                c.phase = "exiting".into();
                c.active_exit_at = now();
                c.generation = crate::turn_ledger::mint_id("exit-");
                c.summary.push(message.clone());
                job=Some((format!("{prefix}【群聊上下文】\n{}\n\n你已经参与群聊有一阵子了，现在该自然地收尾/潜水了。请说一句简短的退场话（例如“我先潜水了”“你们聊，我摸鱼去了”），说完后就安静下来，不再继续接话。",context_block(c,&sc)),true));
            } else if c.phase == "active" || c.phase == "probing" {
                c.phase = "active".into();
                c.last_active_message_at = now();
                c.probe_deadline = 0;
                if inb.kind == "private" {
                    job = Some((
                        format!(
                            "{prefix}{}{text}{}",
                            if owner { "【管理员】" } else { "" },
                            chat::media_hint(&message.message_id, message.media.len())
                        ),
                        false,
                    ));
                }
            } else {
                let trigger = random(&mut s.seed) < probability(&sc, "triggerProbability");
                if !direct && !trigger {
                    c.summary.push(message.clone());
                } else {
                    enter(c, &mut s.seed, &sc, &key, now());
                    job = Some((
                        format!(
                            "{prefix}【群聊上下文】\n{}\n【当前消息】{}\n\n{}\n{}{SPLIT_HINT}",
                            context_block(c, &sc),
                            Message {
                                text: text.clone(),
                                ..message.clone()
                            }
                            .line(owner && inb.kind == "private"),
                            instruction(direct),
                            if text.contains("[引用") {
                                format!("{DIRECTION}\n")
                            } else {
                                String::new()
                            }
                        ),
                        false,
                    ));
                }
            }
            if c.summary.len() > 60 {
                c.summary.remove(0);
            }
            Ok(s.clone())
        })?;
        if let Some((prompt, farewell)) = job {
            let delay = if owner && inb.kind == "private" {
                0
            } else {
                mutate(&context, |s| {
                    Ok(range(
                        &mut s.seed,
                        &sc,
                        "activeReplyDelayMinMs",
                        "activeReplyDelayMaxMs",
                    ))
                })?
            };
            self.job(
                &state,
                &key,
                prompt,
                message.media.clone(),
                farewell,
                vec![],
                String::new(),
                delay,
                vec![event.into()],
            )?;
        } else if !(inb.kind == "group" && state.conversations[&key].phase == "active") {
            self.journal.mark_handled(vec![event.into()], now()).await?;
        }
        Ok(())
    }
    async fn sync_mode(&mut self) -> Result<(), String> {
        let stamp = mode_stamp(&self.context)?;
        if stamp != self.stamp {
            self.stamp = stamp;
            self.pending.clear();
            retire_mode_state(&self.context)?;
            for (key, active) in std::mem::take(&mut self.active) {
                let context = self.context.clone();
                let cfg = context.load_config().map_err(|e| e.to_string())?;
                if let Ok(p) = Pipeline::new_with_runtime(&context, &cfg, true) {
                    let _ = p.cancel_reserved_epoch(&key, &active.job.authority.token);
                }
                active.task.abort();
            }
        }
        Ok(())
    }
    pub async fn tick(&mut self, nick: &str) -> Result<(), String> {
        self.sync_mode().await?;
        // Jobs the per-conversation cap pushed out: settled, never silently left open (JS drops the oldest and logs it).
        for job in std::mem::take(&mut self.dropped) {
            println!("[reserved] {} 排队的消息已满（每个会话 {}），丢弃最早的一条", job.key, crate::dsh_health::QUEUE_MAX);
            crate::recovery_log::record(
                &self.context,
                crate::run_record::Trigger::Turn,
                crate::run_record::Component::Queue,
                "dropped-oldest",
                crate::run_record::Outcome::Failed,
                &format!("{}: queue of {} waiting messages is full", job.key, crate::dsh_health::QUEUE_MAX),
            );
            self.settle_state(&job.key, &job, true)?;
            self.journal.mark_handled(job.inbound, now()).await?;
        }
        if !crate::dsh_health::is_down(&self.context) {
            self.waiting_noted.clear();
        }
        let done = self
            .active
            .iter()
            .filter(|(_, a)| a.task.is_finished())
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in done {
            let active = self.active.remove(&key).unwrap();
            let job = active.job;
            let (result, unsubmitted) = active.task.await.map_err(|e| e.to_string())?;
            if result.is_err() && unsubmitted && job.authority.check(&self.context, &key).is_ok() {
                if crate::dsh_health::is_down(&self.context) {
                    // The prompt never reached DSH and DSH is known down: the message waits for it (JS queues while
                    // dshReady is false and replays on recovery) instead of burning its retries and being lost. No attempt
                    // is used; the dispatch below holds it until DSH is back, then re-checks its authority.
                    println!("[reserved] {key} DSH 不可用，消息排队等待恢复");
                    self.waiting_noted.insert(key.clone());
                    let mut waiting = job;
                    waiting.due = now();
                    let dropped = push_bounded(&mut self.pending, waiting);
                    self.dropped.extend(dropped);
                    continue;
                }
                if job.attempt < 3 {
                    let mut retry = job;
                    retry.attempt += 1;
                    retry.due = now() + 5000 * (1 << (retry.attempt - 1));
                    self.pending.push_back(retry);
                    continue;
                }
            }
            self.settle_state(&key, &job, result.is_err())?;
            self.journal.mark_handled(job.inbound, now()).await?;
        }
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        let sc = config(&cfg);
        if console::runtime_mode(&self.context) != "reserved" {
            return Ok(());
        }
        let scan = sc["enabled"] != false && now() - self.last_tick >= 5000;
        if scan {
            self.last_tick = now();
        }
        let context = self.context.clone();
        let mut planned = Vec::new();
        let mut settled = Vec::new();
        let state = mutate(&context, |s| {
            for (id, f) in s.flush.iter_mut().filter(|(_, f)| f.status == "pending") {
                for (key, c) in &s.conversations {
                    if !self.allowed(&cfg, key)
                        || !f.key.is_empty() && &f.key != key
                        || c.summary.is_empty()
                    {
                        continue;
                    }
                    let prefix = role_prefix(&context, &cfg, key);
                    let lines = c
                        .summary
                        .iter()
                        .map(|m| m.line(true))
                        .collect::<Vec<_>>()
                        .join("\n");
                    planned.push((key.clone(),format!("{prefix}【群聊摘要】过去一段时间群里发生了这些（你未逐条参与）：\n{lines}\n\n【最近对话】\n{}\n\n你不需要回复，只需记住这些内容，后续聊天会更自然。",context_block(c,&sc)),false,c.summary.clone(),id.clone(),0));
                    f.remaining += 1;
                }
                f.status = if f.remaining == 0 { "done" } else { "running" }.into();
            }
            if scan {
                for (key, c) in &mut s.conversations {
                    if !self.allowed(&cfg, key) {
                        leave(c);
                        continue;
                    }
                    let mut prompt = None;
                    let mut farewell = false;
                    let mut media = vec![];
                    if c.phase == "idle" {
                        if sc["proactiveEnabled"] != false
                            && key.starts_with("group:")
                            && now() - c.recent.last().map(|m| m.time).unwrap_or(0)
                                >= n(&sc, "proactiveIdleThresholdMs")
                        {
                            if c.proactive_next_check_at == 0 {
                                c.proactive_next_check_at = now()
                                    + range(
                                        &mut s.seed,
                                        &sc,
                                        "proactiveCheckMinMs",
                                        "proactiveCheckMaxMs",
                                    );
                            }
                            if now() >= c.proactive_next_check_at {
                                c.proactive_next_check_at = now()
                                    + range(
                                        &mut s.seed,
                                        &sc,
                                        "proactiveCheckMinMs",
                                        "proactiveCheckMaxMs",
                                    );
                                if random(&mut s.seed) < probability(&sc, "proactiveProbability") {
                                    enter(c, &mut s.seed, &sc, key, now());
                                    prompt=Some(format!("{}【群聊上下文】\n{}\n\n群内已经长时间没人说话了，你打算开启一个新话题。优先结合你的人格/角色设定的兴趣，其次结合群里大家的兴趣，挑一个合适的话题。可以联网搜索一些新鲜话题来聊。如果你觉得现在不适合开口，可以只输出 [SILENT]。\n{SPLIT_HINT}",role_prefix(&context,&cfg,key),context_block(c,&sc)));
                                }
                            }
                        }
                    } else if c.phase == "active" {
                        if key.starts_with("group:")
                            && sc["activeDurationEnabled"] != false
                            && c.active_deadline_at > 0
                            && now() >= c.active_deadline_at
                        {
                            c.phase = "exiting".into();
                            c.active_exit_at = now();
                            c.generation = crate::turn_ledger::mint_id("exit-");
                            farewell = true;
                            prompt=Some(format!("{}【群聊上下文】\n{}\n\n你已经参与群聊有一阵子了，现在该自然地收尾/潜水了。请说一句简短的退场话（例如“我先潜水了”“你们聊，我摸鱼去了”），说完后就安静下来，不再继续接话。",role_prefix(&context,&cfg,key),context_block(c,&sc)));
                        } else if now() >= c.next_check_at {
                            let new = c
                                .recent
                                .iter()
                                .filter(|m| m.time > c.last_check_at)
                                .cloned()
                                .collect::<Vec<_>>();
                            c.last_check_at = now();
                            c.next_check_at = now()
                                + range(&mut s.seed, &sc, "activeCheckMinMs", "activeCheckMaxMs");
                            if !new.is_empty() {
                                c.last_active_message_at = now();
                                let direct = key.starts_with("private:")
                                    || new.iter().any(|m| {
                                        m.quote_target_is_self || must_reply(&m.plain, nick, &sc)
                                    });
                                let skip = if now() - c.last_ai_reply_at < 60000 {
                                    0.0
                                } else {
                                    probability(&sc, "skipProbability")
                                };
                                if !direct
                                    && random(&mut s.seed)
                                        < (skip - (new.len() as f64 * 0.1).min(0.5)).max(0.0)
                                {
                                    settled.extend(new.iter().map(|m| m.event_key.clone()));
                                    c.silent.extend(new.clone());
                                    if c.silent.len() > 30 {
                                        c.silent.drain(..c.silent.len() - 30);
                                    }
                                    c.summary.extend(new);
                                    if c.summary.len() > 60 {
                                        c.summary.drain(..c.summary.len() - 60);
                                    }
                                } else {
                                    let mut messages = std::mem::take(&mut c.silent);
                                    messages.extend(new);
                                    let lines = messages
                                        .iter()
                                        .map(|m| m.line(true))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    media = messages;
                                    prompt = Some(format!(
                                        "【新消息】\n{lines}\n\n{}{}\n{SPLIT_HINT}",
                                        if lines.contains("[引用") {
                                            format!("{DIRECTION}\n")
                                        } else {
                                            String::new()
                                        },
                                        instruction(direct)
                                    ));
                                }
                            } else if now() - c.last_active_message_at >= n(&sc, "idleWindowMs") {
                                if random(&mut s.seed) < probability(&sc, "idleRetryProbability") {
                                    c.phase = "probing".into();
                                    c.probe_deadline = now() + n(&sc, "idleRetryWaitMs");
                                    prompt=Some(format!("【群聊上下文】\n{}\n\n群里安静了一会儿，你可以说点什么。如果不想说，请只输出 [SILENT]。\n{SPLIT_HINT}",context_block(c,&sc)));
                                } else {
                                    leave(c);
                                }
                            }
                        }
                    } else if c.phase == "probing" && now() > c.probe_deadline {
                        leave(c);
                    } else if c.phase == "exiting" && now() - c.active_exit_at > 3600000 {
                        finish_exit(c);
                    }
                    if let Some(prompt) = prompt {
                        let delay = range(
                            &mut s.seed,
                            &sc,
                            "activeReplyDelayMinMs",
                            "activeReplyDelayMaxMs",
                        );
                        planned.push((key.clone(), prompt, farewell, media, String::new(), delay));
                    }
                }
            }
            Ok(s.clone())
        })?;
        self.journal.mark_handled(settled, now()).await?;
        for (key, prompt, farewell, messages, flush, delay) in planned {
            let media = messages.iter().flat_map(|m| m.media.clone()).collect();
            let inbound = if flush.is_empty() {
                messages
                    .iter()
                    .map(|m| m.event_key.clone())
                    .filter(|k| !k.is_empty())
                    .collect()
            } else {
                vec![]
            };
            let summary = if flush.is_empty() { vec![] } else { messages };
            self.job(
                &state, &key, prompt, media, farewell, summary, flush, delay, inbound,
            )?;
        }
        let count = self.pending.len();
        for _ in 0..count {
            let job = self.pending.pop_front().unwrap();
            if let Err(why) = job.authority.check(&context, &job.key) {
                println!("[reserved] {} queued work cancelled: {why}", job.key);
                self.journal.mark_handled(job.inbound, now()).await?;
                continue;
            }
            if job.due > now() || self.active.contains_key(&job.key) {
                self.pending.push_back(job);
                continue;
            }
            if crate::dsh_health::is_down(&context) {
                // DSH is known down: no call is made for waiting work. Its authority was re-checked just above; it is
                // checked again, with everything else, on the pass that finds DSH back.
                if self.waiting_noted.insert(job.key.clone()) {
                    println!("[reserved] {} DSH 不可用，消息排队等待恢复", job.key);
                }
                self.pending.push_back(job);
                continue;
            }
            let records = crate::turn_ledger::read_records(&context)?;
            let (turns, _) = crate::turn_ledger::fold(&records);
            if crate::turn_ledger::blocking_turn(&turns, &job.key).is_some() {
                self.pending.push_back(job);
                continue;
            }
            self.journal
                .mark_processing(job.inbound.clone(), now())
                .await?;
            let ctx = context.clone();
            let cfg = cfg.clone();
            let work = job.clone();
            let task = tokio::spawn(async move {
                let mut pipe = match Pipeline::new_with_runtime(&ctx, &cfg, true) {
                    Ok(p) => p,
                    Err(e) => return (Err(e), true),
                };
                let result = pipe
                    .run_reserved_turn(
                        &work.key,
                        &work.prompt,
                        &cfg,
                        &work.inbound,
                        &work.media,
                        !work.summary.is_empty(),
                        work.authority.clone(),
                    )
                    .await;
                (result, pipe.dispatch_definitely_unsubmitted())
            });
            self.active.insert(job.key.clone(), Active { job, task });
        }
        Ok(())
    }
}

pub fn reply_parts(text: &str, cfg: &Value) -> Vec<String> {
    // Summary notation may contain whitespace; remove it before burst splitting can tear it apart.
    let canonical = crate::outbound_message::normalize(text);
    let text = canonical.as_str();
    let mut groups: Vec<String> = vec![];
    fn cjk(c: char) -> bool {
        matches!(c as u32,0x4e00..=0x9fff|0x3400..=0x4dbf|0xf900..=0xfaff)
    }
    if cfg["burstEnabled"] == false {
        groups.push(text.trim().replace("\r\n", "\n"));
    } else {
        for token in text.split_whitespace() {
            if let Some(last) = groups.last_mut() {
                if !last.chars().last().is_some_and(cjk) && !token.chars().next().is_some_and(cjk) {
                    last.push(' ');
                    last.push_str(token);
                    continue;
                }
            }
            groups.push(token.into());
        }
    }
    let max = n(cfg, "maxReplyChars");
    let max = if max < 1 { 500 } else { max as usize };
    groups
        .into_iter()
        .flat_map(|g| split_long_segment(&g, max))
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect()
}

/// JS splitLongSegment: secondary punctuation, UTF-16 cuts, atomic URLs and supported faces.
fn split_long_segment(text: &str, max: usize) -> Vec<String> {
    let text = text.trim();
    if text.encode_utf16().count() <= max {
        return vec![text.into()];
    }
    static URL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static PLACEHOLDER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let url = URL.get_or_init(|| regex::Regex::new(r"https?://[^\s，。；、]+").unwrap());
    let placeholder = PLACEHOLDER.get_or_init(|| regex::Regex::new(r"\x00URL\d+\x00").unwrap());
    let mut urls = Vec::new();
    let masked = url
        .replace_all(text, |c: &regex::Captures| {
            let id = urls.len();
            urls.push(c[0].to_string());
            format!("\0URL{id}\0")
        })
        .into_owned();
    let mut tokens = Vec::new();
    let mut start = 0;
    for (i, c) in masked.char_indices() {
        if "，、；：,;:".contains(c) {
            tokens.push(masked[start..i + c.len_utf8()].trim());
            start = i + c.len_utf8();
        }
    }
    tokens.push(masked[start..].trim());
    fn hard(text: &str, max: usize, out: &mut Vec<String>) {
        let mut part = String::new();
        let mut len = 0;
        for atom in crate::outbound_message::atoms(text) {
            let size = atom.encode_utf16().count();
            if len + size > max && !part.is_empty() {
                out.push(std::mem::take(&mut part));
                len = 0;
            }
            part.push_str(atom);
            len += size;
        }
        if !part.is_empty() {
            out.push(part);
        }
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for token in tokens.into_iter().filter(|t| !t.is_empty()) {
        if token.encode_utf16().count() > max {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let mut start = 0;
            for m in placeholder.find_iter(token) {
                hard(&token[start..m.start()], max, &mut out);
                out.push(m.as_str().into());
                start = m.end();
            }
            hard(&token[start..], max, &mut out);
        } else if current.encode_utf16().count() + token.encode_utf16().count() <= max {
            current.push_str(token);
        } else {
            out.push(std::mem::replace(&mut current, token.into()));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.into_iter()
        .map(|mut part| {
            for (id, url) in urls.iter().enumerate() {
                part = part.replace(&format!("\0URL{id}\0"), url);
            }
            part
        })
        .filter(|p| !p.is_empty())
        .collect()
}
pub fn reply_delay(context: &RuntimeContext, cfg: &Value) -> Result<u64, String> {
    mutate(context, |s| {
        let (lo, hi, fallback) = if random(&mut s.seed) < probability(cfg, "longGapProbability") {
            ("longGapMinMs", "longGapMaxMs", 8000.0)
        } else {
            ("burstIntervalMinMs", "burstIntervalMaxMs", 1000.0)
        };
        // JS applies Number(value) || fallback before clamping, and clamps max
        // to min rather than swapping the bounds.
        let min = console::social_v1_number(&cfg[lo]);
        let min = if min == 0.0 { fallback } else { min.max(0.0) } as u64;
        let max = console::social_v1_number(&cfg[hi]);
        let max = if max == 0.0 {
            min
        } else {
            (max.max(0.0) as u64).max(min)
        };
        Ok(min.saturating_add(
            (random(&mut s.seed) * (max.saturating_sub(min) as f64 + 1.0)).floor() as u64,
        ))
    })
}

pub fn record_reply(context: &RuntimeContext, key: &str, at: i64) -> Result<(), String> {
    mutate(context, |s| {
        if let Some(c) = s.conversations.get_mut(key) {
            if c.phase != "exiting" {
                c.last_ai_reply_at = at;
                c.last_active_message_at = at;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_ledger::{self, Recorder};

    fn fixture() -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(turn_ledger::mint_id("her-reserved-review-")));
        for dir in ["config", "data", "resources", "logs", "state", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("config/config.json"),
            json!({"allow":{"groups":[42]},"consoleToken":"synthetic-console-token"}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("dependencies.json"), json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}).to_string()).unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    #[test]
    fn gap_ledger_known_keys_reports_an_unreadable_state_instead_of_hiding_it() {
        let (root, context) = fixture();
        assert_eq!(known_keys(&context).unwrap(), Vec::<String>::new(), "no state yet is no keys");
        std::fs::write(root.join("data").join(FILE), r#"{"conversations":{"group:42":{}}}"#).unwrap();
        assert_eq!(known_keys(&context).unwrap(), vec!["group:42".to_string()]);
        std::fs::write(root.join("data").join(FILE), "{ not json").unwrap();
        assert!(known_keys(&context).is_err(), "a state that cannot be read must not look like no keys");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stale_reserved_cleanup_preserves_replacement_chat_turn() {
        let (root, context) = fixture();
        let key = "group:42";
        context
            .write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved"}"#)
            .unwrap();
        let journal = SharedJournal::new(crate::inbound::Journal::open(&context).unwrap());
        let mut hub = Hub::new(&context, journal, vec![key.into()], false).unwrap();
        let state = mutate(&context, |s| {
            s.conversations.insert(key.into(), Conversation::default());
            Ok(s.clone())
        })
        .unwrap();
        hub.job(
            &state,
            key,
            "old reserved input".into(),
            vec![],
            false,
            vec![],
            String::new(),
            0,
            vec![],
        )
        .unwrap();
        let job = hub.pending.pop_front().unwrap();
        let old = Recorder::begin(
            &context,
            key,
            "dsh",
            "reserved",
            &crate::send_outbox::token_epoch(&job.authority.token),
            &[],
            None,
        )
        .unwrap();
        old.session("old-reserved-session").unwrap();
        let task = tokio::spawn(std::future::pending());
        hub.active.insert(key.into(), Active { job, task });

        // Mode retirement happened, then new chat ingress replaced the mapping/epoch.
        context
            .write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"chat"}"#)
            .unwrap();
        let chat_token = console::ensure_or_rotate_agent_token(&context, key, true).unwrap();
        crate::session_map::bind(&context, key, "replacement-chat-session", None).unwrap();
        let chat = Recorder::begin(
            &context,
            key,
            "dsh",
            "chat",
            &crate::send_outbox::token_epoch(&chat_token),
            &["new-chat-message".into()],
            None,
        )
        .unwrap();
        chat.session("replacement-chat-session").unwrap();
        hub.tick("bot").await.unwrap();

        assert_eq!(
            console::conversation_token(&context, key).unwrap(),
            Some(chat_token),
            "stale cleanup must preserve the replacement chat token"
        );
        assert_eq!(
            crate::session_map::lookup(&context, key)
                .unwrap()
                .as_deref(),
            Some("replacement-chat-session")
        );
        let (turns, _) = turn_ledger::fold(&turn_ledger::read_records(&context).unwrap());
        assert!(
            turns
                .iter()
                .find(|t| t.id == chat.id)
                .unwrap()
                .state
                .is_open(),
            "replacement chat turn must remain active"
        );
        assert_eq!(
            turns.iter().find(|t| t.id == old.id).unwrap().state,
            turn_ledger::State::Cancelled
        );
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zero_burst_bounds_use_js_fallback_minima() {
        let (root, context) = fixture();
        let mut cfg = config(
            &json!({"social":{"burstIntervalMinMs":0,"burstIntervalMaxMs":0,"longGapMinMs":0,"longGapMaxMs":0,"longGapProbability":0}}),
        );
        assert_eq!(
            reply_delay(&context, &cfg).unwrap(),
            1000,
            "JS ordinary zero bounds fall back to 1000 ms"
        );
        cfg["longGapProbability"] = json!(1);
        assert_eq!(
            reply_delay(&context, &cfg).unwrap(),
            8000,
            "JS long zero bounds fall back to 8000 ms"
        );
        cfg["longGapMinMs"] = json!(9000);
        assert_eq!(
            reply_delay(&context, &cfg).unwrap(),
            9000,
            "zero maximum falls back to the effective minimum"
        );
        cfg["longGapMaxMs"] = json!(1);
        assert_eq!(
            reply_delay(&context, &cfg).unwrap(),
            9000,
            "JS clamps the maximum to the minimum instead of swapping bounds"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn waiting_job(key: &str, text: &str) -> Job {
        Job {
            key: key.into(),
            prompt: text.into(),
            media: vec![],
            due: 0,
            farewell: false,
            summary: vec![],
            flush: String::new(),
            authority: Authority { owner: "o".into(), generation: "g".into(), mode: "m".into(), social: true, token: "t".into() },
            inbound: vec![format!("event-{text}")],
            attempt: 0,
        }
    }

    /// Ticket 25 review 1: work that waits for DSH is bounded per conversation (JS `QUEUE_MAX` per key, the oldest dropped),
    /// not per process: one busy conversation never costs another its waiting messages.
    #[test]
    fn waiting_work_is_bounded_per_conversation_and_drops_that_conversations_oldest() {
        let mut pending: VecDeque<Job> = VecDeque::new();
        for n in 0..crate::dsh_health::QUEUE_MAX {
            assert!(push_bounded(&mut pending, waiting_job("group:1", &format!("a{n}"))).is_empty());
        }
        assert!(push_bounded(&mut pending, waiting_job("group:2", "b0")).is_empty(), "another conversation has its own room");
        let dropped = push_bounded(&mut pending, waiting_job("group:1", "a-new"));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].prompt, "a0", "the oldest message of the full conversation goes");
        assert_eq!(pending.iter().filter(|j| j.key == "group:1").count(), crate::dsh_health::QUEUE_MAX);
        assert!(pending.iter().any(|j| j.key == "group:2" && j.prompt == "b0"), "the other conversation kept its message");
        assert_eq!(pending.back().unwrap().prompt, "a-new");
    }

    #[test]
    fn many_conversations_may_each_wait_without_any_global_cap_dropping_work() {
        let mut pending: VecDeque<Job> = VecDeque::new();
        for n in 0..300 {
            assert!(push_bounded(&mut pending, waiting_job(&format!("group:{n}"), "x")).is_empty());
        }
        assert_eq!(pending.len(), 300);
    }
}
