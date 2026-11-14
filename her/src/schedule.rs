//! Ticket 33: the one store and interface for timed items: system jobs (config and pause), owner and HER
//! reminders, and HER's sleep, reply-check and reach-out timers. The store lives in `data/schedule.json`
//! (guarded by `locks/schedule.lock`); the live hub publishes its in-memory timers to `data/schedule-timers.json`.
//! System jobs keep the `tasks.rs` scheduler; this module only adds the owner pause, read through `supervisor::enabled_in`.
#![allow(dead_code)]

use crate::runtime::{RuntimeArea, RuntimeContext};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const STORE_FILE: &str = "schedule.json";
const TIMERS_FILE: &str = "schedule-timers.json";
const LOCK_FILE: &str = "locks/schedule.lock";
pub const REMINDER_REASON: &str = "reminder:";
const LATE_GRACE_MS: i64 = 60_000;
const MAX_AHEAD_MS: i64 = 366 * 86_400_000;
const HISTORY_CAP: usize = 500;
const FINISHED_ITEM_CAP: usize = 200;
const TIMER_CANCEL_TTL_MS: i64 = 120_000;
const TIMERS_REFRESH_MS: i64 = 60_000;
const MAX_TEXT_CHARS: usize = 500;
/// Finished items this recent are never compacted away: a queued reminder's text stays until its wake has run.
const RECENT_FINISH_MS: i64 = 24 * 3_600_000;
/// HER's timer snapshot older than this is shown as stale and cannot be cancelled against (the hub is not running).
const STALE_TIMERS_MS: i64 = 2 * TIMERS_REFRESH_MS;
/// The shortest repeat the store accepts, for the owner and for HER.
const MIN_REPEAT_MINUTES: u32 = 15;
/// The model-facing note for a reminder that was cancelled or deleted before its wake ran.
pub const REMINDER_GONE: &str = "【提醒内容】这条提醒已被取消或删除。\n\n";
/// Set once an unreadable store has been logged; cleared by the next good read, so a new failure logs again.
static PAUSE_READ_FAILURE_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub const PAUSABLE_JOBS: &[&str] = &["memory-digest", "memory-digest-check", "backup", "upstream-check"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Creator {
    System,
    Owner,
    Her,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Repeat {
    Once,
    Every { minutes: u32 },
    Daily { hour: u32, minute: u32 },
}

impl Repeat {
    pub fn describe(self) -> String {
        match self {
            Repeat::Once => "一次".to_string(),
            Repeat::Every { minutes } if minutes % 60 == 0 => format!("每 {} 小时", minutes / 60),
            Repeat::Every { minutes } => format!("每 {minutes} 分钟"),
            Repeat::Daily { hour, minute } => format!("每天 {hour:02}:{minute:02}"),
        }
    }

    pub fn validate(self) -> Result<(), StoreError> {
        match self {
            Repeat::Once => Ok(()),
            Repeat::Every { minutes } if minutes < MIN_REPEAT_MINUTES => {
                Err(StoreError::Invalid(format!("重复间隔至少 {MIN_REPEAT_MINUTES} 分钟")))
            }
            Repeat::Every { .. } => Ok(()),
            Repeat::Daily { hour, minute } => {
                if hour > 23 || minute > 59 {
                    Err(StoreError::Invalid("重复时间无效".into()))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// The next slot after `now` for a repeat whose last slot was `at`; `None` for one-off items.
    pub fn next_after(self, at: i64, now: i64) -> Option<i64> {
        match self {
            Repeat::Once => None,
            Repeat::Every { minutes } => {
                let period = minutes as i64 * 60_000;
                if period <= 0 {
                    None
                } else if at > now {
                    Some(at)
                } else {
                    Some(at + ((now - at) / period + 1) * period)
                }
            }
            Repeat::Daily { hour, minute } => crate::tasks::next_due(
                crate::tasks::Schedule::Daily { hour, minute },
                now,
                crate::tasks::local_offset_ms(),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    Active,
    Paused,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub at: i64,
    pub summary: String,
    pub late: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub creator: Creator,
    pub conversation: String,
    pub text: String,
    pub next_at: i64,
    pub repeat: Repeat,
    pub status: ItemStatus,
    pub created_at: i64,
    #[serde(default)]
    pub run_now: bool,
    #[serde(default)]
    pub last_result: Option<RunResult>,
    /// When the item became Done or Cancelled; compaction keeps the most recently finished items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewItem {
    pub creator: Creator,
    pub conversation: String,
    pub text: String,
    pub at: i64,
    pub repeat: Repeat,
}

#[derive(Debug, Clone, Default)]
pub struct ItemChange {
    pub text: Option<String>,
    pub at: Option<i64>,
    pub repeat: Option<Repeat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimerKind {
    Sleep,
    ReplyCheck,
    ReachOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerView {
    pub conversation: String,
    pub kind: TimerKind,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerCancel {
    pub conversation: String,
    pub kind: TimerKind,
    /// When the owner asked (the request expires `TIMER_CANCEL_TTL_MS` later).
    pub at: i64,
    /// The timer's time as the owner saw it; the hub cancels only a timer still armed for exactly this time.
    #[serde(default)]
    pub timer_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecord {
    pub at: i64,
    pub id: String,
    pub conversation: String,
    pub summary: String,
    pub late: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    version: u32,
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    system_paused: Vec<String>,
    #[serde(default)]
    timer_cancels: Vec<TimerCancel>,
    #[serde(default)]
    history: Vec<HistoryRecord>,
}

impl Document {
    fn empty() -> Self {
        Self { version: 1, items: Vec::new(), system_paused: Vec::new(), timer_cancels: Vec::new(), history: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimersDocument {
    published_at: i64,
    timers: Vec<TimerView>,
}

/// One system job as the Schedule page sees it (built by the console from the supervisor's task API).
#[derive(Debug, Clone)]
pub struct SystemJob {
    pub id: &'static str,
    pub effect: &'static str,
    pub repeat: String,
    pub next_at: Option<i64>,
    pub enabled_by_config: bool,
    pub running: bool,
    pub last_result: Option<RunResult>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    id: String,
    kind: &'static str,
    creator: Creator,
    conversation: Option<String>,
    next_run_at: Option<i64>,
    repeat: String,
    action: String,
    status: &'static str,
    created_at: Option<i64>,
    last_result: Option<RunResult>,
    actions: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Unavailable(String),
}

impl StoreError {
    pub fn status(&self) -> u16 {
        match self {
            StoreError::NotFound => 404,
            StoreError::Invalid(_) => 400,
            StoreError::Conflict(_) => 409,
            StoreError::Unavailable(_) => 503,
        }
    }

    pub fn message(&self) -> String {
        match self {
            StoreError::NotFound => "未找到该日程项".to_string(),
            StoreError::Invalid(message) | StoreError::Conflict(message) | StoreError::Unavailable(message) => message.clone(),
        }
    }
}

impl From<String> for StoreError {
    fn from(message: String) -> Self {
        StoreError::Unavailable(message)
    }
}

fn finished_conflict() -> StoreError {
    StoreError::Conflict("已结束".into())
}

fn is_finished(status: ItemStatus) -> bool {
    matches!(status, ItemStatus::Done | ItemStatus::Cancelled)
}

fn find_mut<'a>(doc: &'a mut Document, id: &str) -> Result<&'a mut Item, StoreError> {
    doc.items.iter_mut().find(|item| item.id == id).ok_or(StoreError::NotFound)
}

fn valid_conversation(key: &str) -> bool {
    match key.strip_prefix("group:").or_else(|| key.strip_prefix("private:")) {
        Some(digits) => {
            !digits.is_empty()
                && digits.len() <= 20
                && digits.bytes().all(|b| b.is_ascii_digit())
                && !digits.starts_with('0')
        }
        None => false,
    }
}

fn clean_text(text: &str) -> Result<String, StoreError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Invalid("提醒内容不能为空".into()));
    }
    if trimmed.chars().count() > MAX_TEXT_CHARS {
        return Err(StoreError::Invalid(format!("提醒内容不能超过 {MAX_TEXT_CHARS} 个字符")));
    }
    Ok(trimmed.to_string())
}

fn check_at(at: i64, now: i64) -> Result<(), StoreError> {
    if at >= now - LATE_GRACE_MS && at <= now + MAX_AHEAD_MS {
        Ok(())
    } else {
        Err(StoreError::Invalid("提醒时间必须在现在起一年之内".into()))
    }
}

/// Keeps the newest history records and the most recently finished items. Items finished in the last day, and any
/// item finished by the current update, are never dropped, so a queued reminder keeps its text; the cap may be
/// exceeded while they are kept.
fn compact(doc: &mut Document, finished_before: &[String], now: i64) {
    let excess = doc.history.len().saturating_sub(HISTORY_CAP);
    if excess > 0 {
        doc.history.drain(..excess);
    }
    let finished = doc.items.iter().filter(|item| is_finished(item.status)).count();
    if finished <= FINISHED_ITEM_CAP {
        return;
    }
    let mut droppable: Vec<(i64, String)> = doc
        .items
        .iter()
        .filter(|item| is_finished(item.status) && finished_before.contains(&item.id) && finish_key(item) < now - RECENT_FINISH_MS)
        .map(|item| (finish_key(item), item.id.clone()))
        .collect();
    droppable.sort();
    let drop: Vec<String> = droppable.into_iter().take(finished - FINISHED_ITEM_CAP).map(|(_, id)| id).collect();
    doc.items.retain(|item| !drop.contains(&item.id));
}

fn finish_key(item: &Item) -> i64 {
    item.finished_at.or(item.last_result.as_ref().map(|result| result.at)).unwrap_or(item.created_at)
}

fn timer_name(kind: TimerKind) -> &'static str {
    match kind {
        TimerKind::Sleep => "sleep",
        TimerKind::ReplyCheck => "replyCheck",
        TimerKind::ReachOut => "reachOut",
    }
}

pub struct ScheduleStore {
    context: RuntimeContext,
}

impl ScheduleStore {
    pub fn new(context: &RuntimeContext) -> Self {
        Self { context: context.clone() }
    }

    fn read(&self) -> Result<Document, StoreError> {
        match self.context.read_optional_text(RuntimeArea::Data, STORE_FILE) {
            Err(error) => Err(StoreError::Unavailable(error.to_string())),
            Ok(None) => Ok(Document::empty()),
            Ok(Some(text)) => {
                let mut doc: Document = serde_json::from_str(&text)
                    .map_err(|error| StoreError::Unavailable(format!("schedule store invalid: {error}")))?;
                if doc.version != 1 {
                    return Err(StoreError::Unavailable("schedule store version unsupported".into()));
                }
                // A hand-edited or legacy pause of a job that cannot pause (watchdog, manual tasks) is ignored.
                doc.system_paused.retain(|job| PAUSABLE_JOBS.contains(&job.as_str()));
                Ok(doc)
            }
        }
    }

    fn update<T>(&self, change: impl FnOnce(&mut Document) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let _gate = self.lock()?;
        let mut doc = self.read()?;
        let before = serde_json::to_string(&doc).map_err(|error| StoreError::Unavailable(error.to_string()))?;
        let finished_before: Vec<String> =
            doc.items.iter().filter(|item| is_finished(item.status)).map(|item| item.id.clone()).collect();
        let value = change(&mut doc)?;
        compact(&mut doc, &finished_before, crate::parity_clock::now_ms() as i64);
        let after = serde_json::to_string(&doc).map_err(|error| StoreError::Unavailable(error.to_string()))?;
        if after != before {
            let bytes = serde_json::to_vec_pretty(&doc).map_err(|error| StoreError::Unavailable(error.to_string()))?;
            self.context.write_atomic(RuntimeArea::Data, STORE_FILE, &bytes)?;
        }
        Ok(value)
    }

    /// The store lock (`locks/schedule.lock`), retried for up to 2 s.
    fn lock(&self) -> Result<std::fs::File, StoreError> {
        let started = Instant::now();
        loop {
            match self.context.open_locked_file(RuntimeArea::Data, LOCK_FILE) {
                Ok(file) => return Ok(file),
                Err(error) if error == "runtime file is already locked" && started.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(StoreError::Unavailable(error)),
            }
        }
    }

    pub fn create(&self, new: NewItem, now: i64) -> Result<Item, StoreError> {
        if new.creator == Creator::System {
            return Err(StoreError::Invalid("系统任务不能创建提醒".into()));
        }
        if !valid_conversation(&new.conversation) {
            return Err(StoreError::Invalid("会话标识无效".into()));
        }
        let text = clean_text(&new.text)?;
        check_at(new.at, now)?;
        new.repeat.validate()?;
        let item = Item {
            id: crate::turn_ledger::mint_id("rem-"),
            creator: new.creator,
            conversation: new.conversation,
            text,
            next_at: new.at,
            repeat: new.repeat,
            status: ItemStatus::Active,
            created_at: now,
            run_now: false,
            last_result: None,
            finished_at: None,
        };
        let stored = item.clone();
        self.update(|doc| {
            doc.items.push(item);
            Ok(stored)
        })
    }

    pub fn items(&self) -> Result<Vec<Item>, StoreError> {
        Ok(self.read()?.items)
    }

    pub fn item(&self, id: &str) -> Result<Option<Item>, StoreError> {
        Ok(self.read()?.items.into_iter().find(|item| item.id == id))
    }

    pub fn update_item(&self, id: &str, change: ItemChange, now: i64) -> Result<Item, StoreError> {
        self.update(|doc| {
            let item = find_mut(doc, id)?;
            if is_finished(item.status) {
                return Err(finished_conflict());
            }
            let text = change.text.as_deref().map(clean_text).transpose()?;
            if let Some(at) = change.at {
                check_at(at, now)?;
            }
            if let Some(repeat) = change.repeat {
                repeat.validate()?;
            }
            if let Some(text) = text {
                item.text = text;
            }
            if let Some(at) = change.at {
                item.next_at = at;
            }
            if let Some(repeat) = change.repeat {
                item.repeat = repeat;
            }
            Ok(item.clone())
        })
    }

    pub fn set_item_paused(&self, id: &str, paused: bool) -> Result<Item, StoreError> {
        self.update(|doc| {
            let item = find_mut(doc, id)?;
            match item.status {
                ItemStatus::Done | ItemStatus::Cancelled => return Err(finished_conflict()),
                ItemStatus::Active if paused => item.status = ItemStatus::Paused,
                ItemStatus::Paused if !paused => item.status = ItemStatus::Active,
                _ => {}
            }
            Ok(item.clone())
        })
    }

    pub fn cancel(&self, id: &str, now: i64) -> Result<Item, StoreError> {
        self.update(|doc| {
            let item = find_mut(doc, id)?;
            if is_finished(item.status) {
                return Err(finished_conflict());
            }
            item.status = ItemStatus::Cancelled;
            item.finished_at = Some(now);
            let record = HistoryRecord {
                at: now,
                id: item.id.clone(),
                conversation: item.conversation.clone(),
                summary: "已取消".into(),
                late: false,
            };
            let stored = item.clone();
            doc.history.push(record);
            Ok(stored)
        })
    }

    pub fn run_now(&self, id: &str) -> Result<Item, StoreError> {
        self.update(|doc| {
            let item = find_mut(doc, id)?;
            if is_finished(item.status) {
                return Err(finished_conflict());
            }
            item.run_now = true;
            Ok(item.clone())
        })
    }

    /// Items due at `now`, at most one per conversation, earliest first. Each claimed item is marked
    /// as run before it is returned, so a crash after the claim loses the run rather than repeating it.
    pub fn claim_due(&self, now: i64, deliverable: impl Fn(&str) -> bool) -> Result<Vec<Item>, StoreError> {
        let is_due = |item: &Item| {
            (item.run_now && matches!(item.status, ItemStatus::Active | ItemStatus::Paused))
                || (item.status == ItemStatus::Active && item.next_at <= now)
        };
        if !self.read()?.items.iter().any(is_due) {
            return Ok(Vec::new());
        }
        self.update(|doc| {
            let mut order: Vec<usize> = (0..doc.items.len()).collect();
            order.sort_by_key(|&index| doc.items[index].next_at);
            let mut seen: Vec<String> = Vec::new();
            let mut claimed = Vec::new();
            let mut history = Vec::new();
            for index in order {
                let item = &mut doc.items[index];
                if !is_due(item) || seen.contains(&item.conversation) || !deliverable(&item.conversation) {
                    continue;
                }
                seen.push(item.conversation.clone());
                let manual = item.run_now;
                let late = !manual && now - item.next_at > LATE_GRACE_MS;
                let summary = if manual {
                    "立即运行".to_string()
                } else if late {
                    format!("迟到 {} 分钟后运行一次", (now - item.next_at) / 60_000)
                } else {
                    "按时运行".to_string()
                };
                item.run_now = false;
                if item.repeat == Repeat::Once {
                    item.status = ItemStatus::Done;
                    item.finished_at = Some(now);
                } else if item.next_at <= now {
                    item.next_at = item.repeat.next_after(item.next_at, now).unwrap_or(item.next_at + 60_000);
                }
                item.last_result = Some(RunResult { at: now, summary: summary.clone(), late });
                history.push(HistoryRecord {
                    at: now,
                    id: item.id.clone(),
                    conversation: item.conversation.clone(),
                    summary,
                    late,
                });
                claimed.push(item.clone());
            }
            doc.history.extend(history);
            Ok(claimed)
        })
    }

    /// Whether the owner paused this system job; see [`system_job_paused`].
    pub fn system_paused(&self, job: &str) -> bool {
        system_job_paused(&self.context, job)
    }

    /// The owner pause as stored (pausable jobs only). Errors when the store cannot be read.
    pub fn paused_jobs(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.read()?.system_paused)
    }

    /// Scheduled admission of a pausable system job. Reads the owner pause afresh under the store lock and returns
    /// the lock, which the task executor holds until the run is marked running: a pause that has answered cannot be
    /// overtaken by a due slot. An unreadable store refuses the start, so an owner pause is never dropped.
    pub fn admit_scheduled(&self, job: &str) -> Result<std::fs::File, String> {
        let gate = self.lock().map_err(|error| error.message())?;
        match self.read() {
            Ok(doc) if doc.system_paused.iter().any(|paused| paused == job) => Err("paused on the Schedule page".to_string()),
            Ok(_) => Ok(gate),
            Err(error) => Err(format!("schedule store unavailable; scheduled start refused: {}", error.message())),
        }
    }

    pub fn set_system_paused(&self, job: &str, paused: bool) -> Result<(), StoreError> {
        if !PAUSABLE_JOBS.contains(&job) {
            return Err(StoreError::Conflict("看门狗和手动任务不能暂停".into()));
        }
        self.update(|doc| {
            doc.system_paused.retain(|name| name != job);
            if paused {
                doc.system_paused.push(job.to_string());
            }
            Ok(())
        })
    }

    /// Queue a console cancel of one of HER's reply-check or reach-out timers. `timer_at` is the timer's time as the
    /// page showed it; the cancel is refused unless the hub's fresh snapshot still lists exactly that timer.
    pub fn request_timer_cancel(&self, conversation: &str, kind: TimerKind, timer_at: i64, now: i64) -> Result<(), StoreError> {
        if kind == TimerKind::Sleep {
            return Err(StoreError::Invalid("睡眠由唤醒配置管理，不能用取消队列".into()));
        }
        if !valid_conversation(conversation) {
            return Err(StoreError::Invalid("会话标识无效".into()));
        }
        let (published_at, timers) = self.timers();
        let seen = now - published_at <= STALE_TIMERS_MS
            && timers.iter().any(|timer| timer.conversation == conversation && timer.kind == kind && timer.at == timer_at);
        if !seen {
            return Err(StoreError::Conflict("该计时器已变化，或 HER 当前没有在运行；请刷新后再试".into()));
        }
        self.update(|doc| {
            doc.timer_cancels.retain(|cancel| now - cancel.at <= TIMER_CANCEL_TTL_MS);
            doc.timer_cancels.push(TimerCancel { conversation: conversation.to_string(), kind, at: now, timer_at });
            Ok(())
        })
    }

    /// Cancel requests the hub has not yet taken and that have not expired.
    pub fn take_timer_cancels(&self, now: i64) -> Result<Vec<TimerCancel>, StoreError> {
        if self.read()?.timer_cancels.is_empty() {
            return Ok(Vec::new());
        }
        self.update(|doc| {
            let taken = std::mem::take(&mut doc.timer_cancels);
            Ok(taken.into_iter().filter(|cancel| now - cancel.at <= TIMER_CANCEL_TTL_MS).collect())
        })
    }

    /// Write the hub's timer snapshot. Unchanged timers are rewritten only once a refresh period has passed.
    pub fn publish_timers(&self, mut timers: Vec<TimerView>, now: i64) -> Result<(), StoreError> {
        timers.sort_by(|a, b| (a.conversation.as_str(), a.kind).cmp(&(b.conversation.as_str(), b.kind)));
        let (published_at, current) = self.timers();
        if current == timers && now - published_at < TIMERS_REFRESH_MS {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&TimersDocument { published_at: now, timers })
            .map_err(|error| StoreError::Unavailable(error.to_string()))?;
        self.context.write_atomic(RuntimeArea::Data, TIMERS_FILE, &bytes)?;
        Ok(())
    }

    fn timers(&self) -> (i64, Vec<TimerView>) {
        self.context
            .read_optional_text(RuntimeArea::Data, TIMERS_FILE)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<TimersDocument>(&text).ok())
            .map(|doc| (doc.published_at, doc.timers))
            .unwrap_or((0, Vec::new()))
    }

    /// Every entry on the Schedule page: system jobs, owner and HER reminders, and HER's timers, soonest first.
    pub fn entries(&self, jobs: &[SystemJob], now: i64) -> Result<Vec<Entry>, StoreError> {
        let doc = self.read()?;
        let mut entries = Vec::new();
        for job in jobs {
            let paused = doc.system_paused.iter().any(|name| name == job.id);
            let status = if job.running {
                "running"
            } else if paused {
                "paused"
            } else if !job.enabled_by_config {
                "disabled"
            } else {
                "active"
            };
            let mut actions = Vec::new();
            if PAUSABLE_JOBS.contains(&job.id) {
                actions.push(if paused { "resume" } else { "pause" });
            }
            actions.push("run");
            entries.push(Entry {
                id: format!("system:{}", job.id),
                kind: "job",
                creator: Creator::System,
                conversation: None,
                next_run_at: if paused { None } else { job.next_at },
                repeat: job.repeat.clone(),
                action: job.effect.to_string(),
                status,
                created_at: None,
                last_result: job.last_result.clone(),
                actions,
                observed_at: None,
            });
        }
        for item in doc.items.iter().filter(|item| matches!(item.status, ItemStatus::Active | ItemStatus::Paused)) {
            let paused = item.status == ItemStatus::Paused;
            let status = if paused {
                "paused"
            } else if now - item.next_at > LATE_GRACE_MS {
                "overdue"
            } else {
                "active"
            };
            entries.push(Entry {
                id: format!("item:{}", item.id),
                kind: "reminder",
                creator: item.creator,
                conversation: Some(item.conversation.clone()),
                next_run_at: Some(item.next_at),
                repeat: item.repeat.describe(),
                action: item.text.clone(),
                status,
                created_at: Some(item.created_at),
                last_result: item.last_result.clone(),
                actions: vec![if paused { "resume" } else { "pause" }, "edit", "cancel", "run"],
                observed_at: None,
            });
        }
        let (published_at, timers) = self.timers();
        // A snapshot the hub stopped refreshing is shown as stale with no actions: the hub may not be running.
        let stale = now - published_at > STALE_TIMERS_MS;
        for timer in timers {
            let (action, mut actions): (&str, Vec<&'static str>) = match timer.kind {
                TimerKind::Sleep => ("潜水结束后唤醒", vec!["edit", "cancel", "run"]),
                TimerKind::ReplyCheck => ("检查是否有人回复", vec!["cancel", "run"]),
                TimerKind::ReachOut => ("主动冒泡机会", vec!["cancel", "run"]),
            };
            if stale {
                actions.clear();
            }
            entries.push(Entry {
                id: format!("{}:{}", timer_name(timer.kind), timer.conversation),
                kind: timer_name(timer.kind),
                creator: Creator::Her,
                conversation: Some(timer.conversation),
                next_run_at: Some(timer.at),
                repeat: Repeat::Once.describe(),
                action: action.to_string(),
                status: if stale { "stale" } else { "active" },
                created_at: None,
                last_result: None,
                actions,
                observed_at: Some(published_at),
            });
        }
        entries.sort_by_key(|entry| (entry.next_run_at.is_none(), entry.next_run_at));
        Ok(entries)
    }

    /// Past runs and cancellations, newest first.
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryRecord>, StoreError> {
        Ok(self.read()?.history.into_iter().rev().take(limit).collect())
    }
}

/// Whether a system job is paused on the Schedule page. Only pausable jobs can be paused. An unreadable store keeps
/// every pausable job paused (fail closed: an owner pause is never dropped by a bad file) and is logged once until the
/// store reads again; the watchdog and manual tasks never depend on the store.
pub fn system_job_paused(context: &RuntimeContext, job: &str) -> bool {
    use std::sync::atomic::Ordering;
    if !PAUSABLE_JOBS.contains(&job) {
        return false;
    }
    match ScheduleStore::new(context).paused_jobs() {
        Ok(paused) => {
            PAUSE_READ_FAILURE_LOGGED.store(false, Ordering::Relaxed);
            paused.iter().any(|name| name == job)
        }
        Err(error) => {
            if !PAUSE_READ_FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
                crate::safe_eprintln!("[schedule] store unreadable; pausable system jobs stay paused until it is fixed: {}", error.message());
            }
            true
        }
    }
}

/// The model-facing line for a reminder wake: the reminder text, or a note that it was cancelled or deleted.
/// Empty for other reasons and when the store cannot be read.
pub fn reminder_line(context: &RuntimeContext, reason: &str) -> String {
    let Some(id) = reason.strip_prefix(REMINDER_REASON) else {
        return String::new();
    };
    match ScheduleStore::new(context).item(id) {
        Ok(Some(item)) if item.status != ItemStatus::Cancelled => {
            let label = match item.creator {
                Creator::Owner => "管理员",
                Creator::Her => "你自己",
                Creator::System => "系统",
            };
            format!("【提醒内容】{}\n（由{}设置）\n\n", item.text, label)
        }
        Ok(_) => REMINDER_GONE.to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const DAY: i64 = 86_400_000;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-schedule-{}-{}-{}",
                std::process::id(),
                crate::parity_clock::now_ms(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )));
            for name in ["config", "data", "resources"] {
                std::fs::create_dir_all(root.join(name)).unwrap();
            }
            std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-schedule"}"#).unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(
                root.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","model":"http://127.0.0.1:1"}"#,
            )
            .unwrap();
            Self(root)
        }
        fn context(&self) -> RuntimeContext {
            RuntimeContext::from_root(&self.0).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn owner(conversation: &str, text: &str, at: i64, repeat: Repeat) -> NewItem {
        NewItem { creator: Creator::Owner, conversation: conversation.into(), text: text.into(), at, repeat }
    }

    #[test]
    fn overdue_repeat_runs_once_after_restart_and_never_catches_up() {
        let fixture = Fixture::new();
        let item = ScheduleStore::new(&fixture.context())
            .create(owner("group:420042", "喝水", NOW + 60_000, Repeat::Every { minutes: 60 }), NOW)
            .unwrap();

        // A new store over the same file stands in for the restart.
        let later = NOW + 3 * DAY;
        let claimed = ScheduleStore::new(&fixture.context()).claim_due(later, |_| true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, item.id);
        assert!(claimed[0].last_result.as_ref().unwrap().late);
        assert!(claimed[0].next_at > later, "a missed repeat jumps to the next slot after now");

        let again = ScheduleStore::new(&fixture.context()).claim_due(later + 1_000, |_| true).unwrap();
        assert!(again.is_empty());

        // The slot boundary: a last slot strictly after now is kept; one at or before now moves forward.
        let every = Repeat::Every { minutes: 60 };
        assert_eq!(every.next_after(NOW + 1, NOW), Some(NOW + 1));
        assert_eq!(every.next_after(NOW, NOW), Some(NOW + 3_600_000));
    }

    #[test]
    fn overdue_once_item_runs_late_once_then_is_done() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let at = NOW + 60_000;
        let on_time = store.create(owner("group:1", "甲", at, Repeat::Once), NOW).unwrap();
        let late = store.create(owner("group:2", "乙", at, Repeat::Once), NOW).unwrap();

        // Exactly the grace period after the slot is still on time.
        let on_grace = store.claim_due(at + LATE_GRACE_MS, |key| key == "group:1").unwrap();
        assert_eq!(on_grace.len(), 1);
        assert_eq!(on_grace[0].id, on_time.id);
        let result = on_grace[0].last_result.clone().unwrap();
        assert!(!result.late);
        assert_eq!(result.summary, "按时运行");
        assert_eq!(on_grace[0].status, ItemStatus::Done);

        // One millisecond more is late.
        let over = store.claim_due(at + LATE_GRACE_MS + 1, |_| true).unwrap();
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].id, late.id);
        let result = over[0].last_result.clone().unwrap();
        assert!(result.late);
        assert_eq!(result.summary, "迟到 1 分钟后运行一次");
        assert_eq!(over[0].status, ItemStatus::Done);
        assert!(store.claim_due(at + 10 * 60_000, |_| true).unwrap().is_empty());
    }

    #[test]
    fn paused_items_wait_and_run_now_fires_once() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let item = store
            .create(
                NewItem {
                    creator: Creator::Her,
                    conversation: "private:7001".into(),
                    text: "记得回".into(),
                    at: NOW + 60_000,
                    repeat: Repeat::Once,
                },
                NOW,
            )
            .unwrap();
        let paused = store.set_item_paused(&item.id, true).unwrap();
        assert_eq!(paused.status, ItemStatus::Paused);
        assert!(store.claim_due(NOW + 3 * DAY, |_| true).unwrap().is_empty(), "a paused reminder waits");

        store.run_now(&item.id).unwrap();
        let claimed = store.claim_due(NOW + 3 * DAY + 1, |_| true).unwrap();
        assert_eq!(claimed.len(), 1);
        let result = claimed[0].last_result.clone().unwrap();
        assert_eq!(result.summary, "立即运行");
        assert!(!result.late);
        assert_eq!(claimed[0].status, ItemStatus::Done);
        assert!(store.claim_due(NOW + 3 * DAY + 2, |_| true).unwrap().is_empty(), "a run now fires once");
    }

    #[test]
    fn one_reminder_per_conversation_per_claim_and_undeliverable_items_stay_due() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let at = NOW + 60_000;
        let first = store.create(owner("group:1", "一", at, Repeat::Once), NOW).unwrap();
        let second = store.create(owner("group:1", "二", at + 1_000, Repeat::Once), NOW).unwrap();
        let other = store.create(owner("group:2", "三", at + 2_000, Repeat::Once), NOW).unwrap();

        let later = at + 10_000;
        let claimed = store.claim_due(later, |key| key != "group:2").unwrap();
        let ids: Vec<&str> = claimed.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, vec![first.id.as_str()]);

        let next = store.claim_due(later, |_| true).unwrap();
        let ids: Vec<&str> = next.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, vec![second.id.as_str(), other.id.as_str()]);
    }

    #[test]
    fn create_edit_cancel_validate() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let at = NOW + 60_000;

        assert!(matches!(store.create(owner("group:1", "   ", at, Repeat::Once), NOW), Err(StoreError::Invalid(_))));
        let long = "字".repeat(501);
        assert!(matches!(store.create(owner("group:1", &long, at, Repeat::Once), NOW), Err(StoreError::Invalid(_))));
        let max = "字".repeat(500);
        assert!(store.create(owner("group:1", &max, at, Repeat::Once), NOW).is_ok());

        assert!(matches!(store.create(owner("group:1", "x", NOW - LATE_GRACE_MS - 1, Repeat::Once), NOW), Err(StoreError::Invalid(_))));
        assert!(store.create(owner("group:1", "x", NOW - LATE_GRACE_MS, Repeat::Once), NOW).is_ok());
        assert!(matches!(store.create(owner("group:1", "x", NOW + MAX_AHEAD_MS + 1, Repeat::Once), NOW), Err(StoreError::Invalid(_))));
        assert!(store.create(owner("group:1", "x", NOW + MAX_AHEAD_MS, Repeat::Once), NOW).is_ok());

        let system = NewItem { creator: Creator::System, ..owner("group:1", "x", at, Repeat::Once) };
        assert!(matches!(store.create(system, NOW), Err(StoreError::Invalid(_))));
        let too_long = format!("private:{}", "1".repeat(21));
        for bad in ["group:01", "group:", "group:12a", too_long.as_str()] {
            assert!(matches!(store.create(owner(bad, "x", at, Repeat::Once), NOW), Err(StoreError::Invalid(_))), "{bad}");
        }
        assert!(store.create(owner(&format!("group:{}", "1".repeat(20)), "x", at, Repeat::Once), NOW).is_ok());
        assert!(store.create(owner(&format!("private:{}", "1".repeat(19)), "x", at, Repeat::Once), NOW).is_ok());

        assert!(matches!(store.create(owner("group:1", "x", at, Repeat::Every { minutes: 0 }), NOW), Err(StoreError::Invalid(_))));
        assert!(matches!(store.create(owner("group:1", "x", at, Repeat::Every { minutes: 14 }), NOW), Err(StoreError::Invalid(_))));
        assert!(store.create(owner("group:1", "x", at, Repeat::Every { minutes: 15 }), NOW).is_ok());
        assert!(matches!(store.create(owner("group:1", "x", at, Repeat::Daily { hour: 24, minute: 0 }), NOW), Err(StoreError::Invalid(_))));
        assert!(matches!(store.create(owner("group:1", "x", at, Repeat::Daily { hour: 0, minute: 60 }), NOW), Err(StoreError::Invalid(_))));
        assert!(store.create(owner("group:1", "x", at, Repeat::Daily { hour: 23, minute: 59 }), NOW).is_ok());

        let item = store.create(owner("group:9", "喝水", at, Repeat::Every { minutes: 60 }), NOW).unwrap();
        assert!(matches!(store.update_item("rem-missing", ItemChange::default(), NOW), Err(StoreError::NotFound)));
        let edited = store
            .update_item(&item.id, ItemChange { text: Some("  多喝水 ".into()), at: Some(at + 60_000), repeat: None }, NOW)
            .unwrap();
        assert_eq!(edited.text, "多喝水");
        assert_eq!(edited.next_at, at + 60_000);
        let refused = store.update_item(&item.id, ItemChange { text: Some("改".into()), at: Some(NOW + MAX_AHEAD_MS + 1), repeat: None }, NOW);
        assert!(matches!(refused, Err(StoreError::Invalid(_))));
        assert_eq!(store.item(&item.id).unwrap().unwrap().text, "多喝水", "a refused edit changes nothing");

        let cancelled = store.cancel(&item.id, NOW).unwrap();
        assert_eq!(cancelled.status, ItemStatus::Cancelled);
        assert_eq!(store.history(10).unwrap()[0].summary, "已取消");
        assert!(matches!(store.update_item(&item.id, ItemChange::default(), NOW), Err(StoreError::Conflict(_))));
        assert!(matches!(store.cancel(&item.id, NOW), Err(StoreError::Conflict(_))));
        assert!(matches!(store.run_now(&item.id), Err(StoreError::Conflict(_))));
        assert!(store.claim_due(NOW + 3 * DAY, |_| true).unwrap().iter().all(|claimed| claimed.id != item.id));
    }

    #[test]
    fn system_pause_is_limited_to_pausable_jobs() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        assert!(matches!(store.set_system_paused("watchdog", true), Err(StoreError::Conflict(_))));
        assert!(matches!(store.set_system_paused("console-restart", true), Err(StoreError::Conflict(_))));

        store.set_system_paused("backup", true).unwrap();
        assert!(store.system_paused("backup"));
        assert!(!store.system_paused("memory-digest"));

        let job = SystemJob {
            id: "backup",
            effect: "备份配置",
            repeat: "每天 04:00".into(),
            next_at: Some(NOW + 3_600_000),
            enabled_by_config: true,
            running: false,
            last_result: None,
        };
        let entries = store.entries(&[job.clone()], NOW).unwrap();
        let value = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(value["id"], "system:backup");
        assert_eq!(value["status"], "paused");
        assert!(value["nextRunAt"].is_null());
        assert_eq!(value["actions"], serde_json::json!(["resume", "run"]));
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["action", "actions", "conversation", "createdAt", "creator", "id", "kind", "lastResult", "nextRunAt", "repeat", "status"]
        );

        store.set_system_paused("backup", false).unwrap();
        assert!(!store.system_paused("backup"));
        let value = serde_json::to_value(&store.entries(&[job], NOW).unwrap()[0]).unwrap();
        assert_eq!(value["status"], "active");
        assert_eq!(value["nextRunAt"], NOW + 3_600_000);
    }

    #[test]
    fn timers_snapshot_lists_her_timers_and_cancels_expire() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let at = NOW + 3_600_000;
        let snapshot = || {
            vec![
                TimerView { conversation: "private:7001".into(), kind: TimerKind::ReachOut, at: at + 2 },
                TimerView { conversation: "private:7001".into(), kind: TimerKind::ReplyCheck, at: at + 1 },
                TimerView { conversation: "private:7001".into(), kind: TimerKind::Sleep, at },
            ]
        };
        store.publish_timers(snapshot(), NOW).unwrap();
        let (published, timers) = store.timers();
        assert_eq!(published, NOW);
        let kinds: Vec<TimerKind> = timers.iter().map(|timer| timer.kind).collect();
        assert_eq!(kinds, vec![TimerKind::Sleep, TimerKind::ReplyCheck, TimerKind::ReachOut]);

        // An unchanged snapshot is rewritten only once the refresh period has passed.
        store.publish_timers(snapshot(), NOW + TIMERS_REFRESH_MS - 1).unwrap();
        assert_eq!(store.timers().0, NOW);
        store.publish_timers(snapshot(), NOW + TIMERS_REFRESH_MS).unwrap();
        let published = NOW + TIMERS_REFRESH_MS;
        assert_eq!(store.timers().0, published);

        let value = serde_json::to_value(&store.entries(&[], published).unwrap()[0]).unwrap();
        assert_eq!(value["id"], "sleep:private:7001");
        assert_eq!(value["kind"], "sleep");
        assert_eq!(value["status"], "active");
        assert_eq!(value["actions"], serde_json::json!(["edit", "cancel", "run"]));
        assert_eq!(value["observedAt"], published);

        // A cancel names the timer the owner saw; a changed or unknown timer is refused.
        assert!(matches!(store.request_timer_cancel("private:7001", TimerKind::Sleep, at, published), Err(StoreError::Invalid(_))));
        assert!(matches!(store.request_timer_cancel("private:7001", TimerKind::ReachOut, at + 3, published), Err(StoreError::Conflict(_))));
        store.request_timer_cancel("private:7001", TimerKind::ReachOut, at + 2, published).unwrap();
        let taken = store.take_timer_cancels(published).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!((taken[0].kind, taken[0].timer_at), (TimerKind::ReachOut, at + 2));
        assert!(store.take_timer_cancels(published).unwrap().is_empty());

        // A cancel request is honoured at exactly the TTL and expires one millisecond later.
        store.request_timer_cancel("private:7001", TimerKind::ReplyCheck, at + 1, published).unwrap();
        assert_eq!(store.take_timer_cancels(published + TIMER_CANCEL_TTL_MS).unwrap().len(), 1);
        store.request_timer_cancel("private:7001", TimerKind::ReplyCheck, at + 1, published).unwrap();
        assert!(store.take_timer_cancels(published + TIMER_CANCEL_TTL_MS + 1).unwrap().is_empty());

        // A snapshot the hub stopped refreshing is stale: no actions, and nothing can be cancelled against it.
        let edge = published + STALE_TIMERS_MS;
        assert_eq!(serde_json::to_value(&store.entries(&[], edge).unwrap()[0]).unwrap()["status"], "active");
        let late = edge + 1;
        let value = serde_json::to_value(&store.entries(&[], late).unwrap()[0]).unwrap();
        assert_eq!(value["status"], "stale");
        assert_eq!(value["actions"], serde_json::json!([]));
        assert!(matches!(store.request_timer_cancel("private:7001", TimerKind::ReplyCheck, at + 1, late), Err(StoreError::Conflict(_))));
    }

    #[test]
    fn unreadable_store_keeps_pausable_jobs_paused_and_ignores_unpausable_entries() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let path = fixture.0.join("data/schedule.json");
        store.set_system_paused("backup", true).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert!(store.system_paused("backup"), "an unreadable store keeps the owner's pause");
        assert!(store.system_paused("memory-digest"), "and holds every pausable job until it reads again");
        assert!(!store.system_paused("watchdog"), "the watchdog never depends on the store");
        assert!(store.admit_scheduled("backup").is_err(), "a scheduled start is refused while the store is unreadable");

        // A hand-edited or legacy pause of a job that cannot pause is ignored on read.
        std::fs::write(&path, r#"{"version":1,"systemPaused":["watchdog","backup"]}"#).unwrap();
        assert!(!store.system_paused("watchdog"));
        assert!(store.system_paused("backup"));
        assert!(!store.system_paused("memory-digest"), "a readable store releases the jobs it does not pause");
        let watchdog = SystemJob { id: "watchdog", effect: "看门狗", repeat: "每 1 分钟".into(), next_at: Some(NOW + 60_000), enabled_by_config: true, running: false, last_result: None };
        let value = serde_json::to_value(&store.entries(&[watchdog], NOW).unwrap()[0]).unwrap();
        assert_eq!(value["status"], "active");
        assert_eq!(value["actions"], serde_json::json!(["run"]));
    }

    #[test]
    fn scheduled_admission_holds_the_store_lock_so_a_pause_cannot_slip_past() {
        let fixture = Fixture::new();
        let store = ScheduleStore::new(&fixture.context());
        let gate = store.admit_scheduled("backup").expect("an unpaused job is admitted");
        let context = fixture.context();
        let pausing = std::thread::spawn(move || ScheduleStore::new(&context).set_system_paused("backup", true));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!pausing.is_finished(), "a pause waits while a scheduled start is being admitted");
        drop(gate);
        pausing.join().unwrap().unwrap();
        let refused = store.admit_scheduled("backup").unwrap_err();
        assert!(refused.contains("paused"), "{refused}");
    }

    fn finished_item(id: &str, created_at: i64, finished_at: i64) -> Item {
        Item { id: id.into(), creator: Creator::Owner, conversation: "group:2".into(), text: "旧".into(), next_at: created_at,
            repeat: Repeat::Once, status: ItemStatus::Cancelled, created_at, run_now: false, last_result: None, finished_at: Some(finished_at) }
    }

    #[test]
    fn compaction_by_finish_time_keeps_the_reminder_finished_in_the_claiming_write() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let mut doc = Document::empty();
        // The oldest item by creation: a reminder set far ahead. More than the cap of newer items finish before it fires.
        doc.items.push(Item { id: "rem-ahead".into(), creator: Creator::Owner, conversation: "group:1".into(), text: "半年后的提醒".into(),
            next_at: NOW + 180 * DAY, repeat: Repeat::Once, status: ItemStatus::Active, created_at: NOW, run_now: false, last_result: None, finished_at: None });
        // Finished more than a day ago on the real clock, so they are droppable; the earliest finish is rem-f0.
        let real = crate::parity_clock::now_ms() as i64;
        for index in 0..FINISHED_ITEM_CAP + 5 {
            let offset = index as i64;
            // Created in the reverse of their finish order, so creation time and finish time disagree.
            doc.items.push(finished_item(&format!("rem-f{index}"), NOW + 1_000 - offset, real - 2 * DAY + offset));
        }
        std::fs::write(fixture.0.join("data/schedule.json"), serde_json::to_vec(&doc).unwrap()).unwrap();

        let store = ScheduleStore::new(&context);
        let claimed = store.claim_due(NOW + 180 * DAY, |_| true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, "rem-ahead");
        let kept = store.item("rem-ahead").unwrap().expect("the claiming write keeps the reminder it dispatches");
        assert_eq!(kept.status, ItemStatus::Done);
        assert_eq!(kept.finished_at, Some(NOW + 180 * DAY));
        assert_eq!(reminder_line(&context, "reminder:rem-ahead"), "【提醒内容】半年后的提醒\n（由管理员设置）\n\n");

        // The earliest finished items are dropped (by finish time, not creation time), down to the cap.
        let items = store.items().unwrap();
        assert_eq!(items.iter().filter(|item| is_finished(item.status)).count(), FINISHED_ITEM_CAP);
        for gone in 0..6 {
            assert!(items.iter().all(|item| item.id != format!("rem-f{gone}")), "rem-f{gone} finished earliest");
        }
        assert!(items.iter().any(|item| item.id == "rem-f6"));
        assert!(items.iter().any(|item| item.id == format!("rem-f{}", FINISHED_ITEM_CAP + 4)));
    }

    #[test]
    fn recently_finished_items_survive_later_writes_and_older_ones_compact_to_the_cap() {
        let real = crate::parity_clock::now_ms() as i64;
        let later_at = real + 3_600_000;

        // Scenario A: more than the cap finished within the last day. An unrelated write keeps every one of them,
        // and drops the older items that finished more than a day ago.
        let fixture = Fixture::new();
        let mut doc = Document::empty();
        for index in 0..FINISHED_ITEM_CAP + 1 {
            doc.items.push(finished_item(&format!("rem-r{index}"), real - 2 * DAY, real - 60_000 - index as i64));
        }
        for index in 0..60 {
            doc.items.push(finished_item(&format!("rem-o{index}"), real - 4 * DAY, real - 3 * DAY - index as i64));
        }
        std::fs::write(fixture.0.join("data/schedule.json"), serde_json::to_vec(&doc).unwrap()).unwrap();
        let store = ScheduleStore::new(&fixture.context());
        store.create(owner("group:9", "无关", later_at, Repeat::Once), real).unwrap();
        let items = store.items().unwrap();
        assert_eq!(items.iter().filter(|item| is_finished(item.status)).count(), FINISHED_ITEM_CAP + 1, "every recent finish is kept");
        for index in 0..FINISHED_ITEM_CAP + 1 {
            assert!(items.iter().any(|item| item.id == format!("rem-r{index}")), "rem-r{index} is recent and kept");
        }
        assert!(items.iter().all(|item| !item.id.starts_with("rem-o")), "items finished more than a day ago are dropped");

        // Scenario B: the recent finishes fit under the cap. Older items are dropped, earliest finish first, down to the cap.
        let fixture = Fixture::new();
        let mut doc = Document::empty();
        for index in 0..150 {
            doc.items.push(finished_item(&format!("rem-r{index}"), real - 2 * DAY, real - 60_000 - index as i64));
        }
        for index in 0..100 {
            doc.items.push(finished_item(&format!("rem-o{index}"), real - 4 * DAY, real - 3 * DAY - index as i64));
        }
        std::fs::write(fixture.0.join("data/schedule.json"), serde_json::to_vec(&doc).unwrap()).unwrap();
        let store = ScheduleStore::new(&fixture.context());
        store.create(owner("group:9", "无关", later_at, Repeat::Once), real).unwrap();
        let items = store.items().unwrap();
        assert_eq!(items.iter().filter(|item| is_finished(item.status)).count(), FINISHED_ITEM_CAP, "older items compact to the cap");
        assert_eq!(items.iter().filter(|item| item.id.starts_with("rem-r")).count(), 150, "every recent finish is kept");
        // The oldest finishes (the highest indices) went first; the 50 newest old items remain.
        assert!(items.iter().any(|item| item.id == "rem-o49"));
        assert!(items.iter().all(|item| item.id != "rem-o50"), "rem-o50 finished before rem-o49 and is dropped first");
    }

    #[test]
    fn reminder_line_renders_only_for_reminder_reasons() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let store = ScheduleStore::new(&context);
        let at = NOW + 60_000;
        let by_owner = store.create(owner("group:1", "喝水", at, Repeat::Once), NOW).unwrap();
        let by_her = store
            .create(NewItem { creator: Creator::Her, conversation: "private:7001".into(), text: "问候".into(), at, repeat: Repeat::Once }, NOW)
            .unwrap();

        assert_eq!(reminder_line(&context, &format!("reminder:{}", by_owner.id)), "【提醒内容】喝水\n（由管理员设置）\n\n");
        assert_eq!(reminder_line(&context, &format!("reminder:{}", by_her.id)), "【提醒内容】问候\n（由你自己设置）\n\n");
        assert_eq!(reminder_line(&context, "reminder:rem-missing"), "【提醒内容】这条提醒已被取消或删除。\n\n");
        assert_eq!(reminder_line(&context, "replyCheck"), "");
        assert_eq!(reminder_line(&context, "bootstrap"), "");

        store.cancel(&by_her.id, NOW).unwrap();
        assert_eq!(reminder_line(&context, &format!("reminder:{}", by_her.id)), REMINDER_GONE, "a cancelled reminder is not delivered");
        std::fs::write(fixture.0.join("data/schedule.json"), "not json").unwrap();
        assert_eq!(reminder_line(&context, &format!("reminder:{}", by_owner.id)), "");
    }
}
