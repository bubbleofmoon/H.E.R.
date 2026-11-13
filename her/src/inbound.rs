//! 入站持久化与断线补收（ticket 07）。
//!
//! 会产生工作的入站事件先写入 `inbound.db`（WAL + synchronous=FULL）再交给唤醒调度。
//! 阶段：
//! - `received`：已保存，尚未触发唤醒；重连或重启时按原顺序重放。
//! - `processing`：唤醒已触发，回合可能在途。重启时仍处于此阶段的事件状态未知，
//!   标为 `interrupted` 交管理员，不自动重提（对齐 agreement 5.3）。
//! - `handled`：回合已结束，或该事件不产生工作（不再重放）。
//! - `backfilled`：断线补收写回的历史消息，只留证据，不唤醒。
//!
//! 缺口（`inbound_gaps`）只有补收结果为 `recovered` 才关闭；重连成功本身不关闭缺口。

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

const DB_NAME: &str = "inbound.db";
const DAY_MS: i64 = 86_400_000;
/// 保留策略（有界）：只清理已结束的数据，永不触碰未解决缺口以及 `received`/`processing` 事件。
/// - `handled`/`backfilled` 事件：1 天后清空原始报文（保留身份键用于去重），7 天后删除整行；
///   这两类合计最多保留 `RETAIN_MAX_FINISHED_ROWS` 行，超出部分从最旧的开始删。
/// - `interrupted` 事件（等待管理员查看）：30 天后删除。
/// - 已 `recovered` 的缺口：7 天后删除；其余状态的缺口永久保留。
pub const RAW_RETAIN_MS: i64 = DAY_MS;
pub const FINISHED_RETAIN_MS: i64 = 7 * DAY_MS;
pub const INTERRUPTED_RETAIN_MS: i64 = 30 * DAY_MS;
pub const RECOVERED_GAP_RETAIN_MS: i64 = 7 * DAY_MS;
pub const RETAIN_MAX_FINISHED_ROWS: i64 = 50_000;
/// 未解决缺口的重试策略：每次失败后等待 `60s * 2^(次数-1)`（上限 6 小时）才会再试，
/// 累计 `MAX_GAP_ATTEMPTS` 次后不再自动重试。缺口本身永不因此被清除或改为已恢复。
pub const GAP_BACKOFF_BASE_MS: i64 = 60_000;
pub const GAP_BACKOFF_MAX_MS: i64 = 6 * 3_600_000;
pub const MAX_GAP_ATTEMPTS: i64 = 8;

pub fn gap_backoff_ms(attempts: i64) -> i64 {
    let shift = (attempts - 1).clamp(0, 20) as u32;
    GAP_BACKOFF_BASE_MS.saturating_mul(1i64 << shift).min(GAP_BACKOFF_MAX_MS)
}
/// 补收最多翻页次数；超过则记为 unknown（无法确定是否已补齐）。
pub const MAX_HISTORY_PAGES: usize = 20;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbound_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_key TEXT NOT NULL UNIQUE,
    conversation TEXT NOT NULL,
    origin TEXT NOT NULL,
    stage TEXT NOT NULL,
    received_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    raw TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS inbound_events_stage ON inbound_events(stage, seq);
CREATE TABLE IF NOT EXISTS inbound_gaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation TEXT NOT NULL,
    from_ms INTEGER NOT NULL,
    to_ms INTEGER NOT NULL,
    status TEXT NOT NULL,
    reason TEXT NOT NULL,
    detail TEXT NOT NULL,
    recovered INTEGER NOT NULL DEFAULT 0,
    opened_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_retry_ms INTEGER NOT NULL DEFAULT 0,
    notified_ms INTEGER NOT NULL DEFAULT 0,
    notice_state TEXT NOT NULL DEFAULT 'pending'
);
CREATE TABLE IF NOT EXISTS inbound_meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
";

fn db_err(error: rusqlite::Error) -> String {
    format!("入站数据库错误：{error}")
}

/// 同一事件的稳定身份。OneBot 消息以 `message_id` 为准（补收与实时收到的同一条得到同一个键）；
/// 没有消息号的事件（通知等）以原始报文的哈希为准。
pub fn event_key(conversation: &str, post_type: &str, message_id: Option<&str>, raw: &str) -> String {
    match (post_type, message_id) {
        ("message", Some(id)) if !id.is_empty() => format!("{conversation}|message:{id}"),
        _ => {
            let mut hasher = Sha256::new();
            hasher.update(raw.as_bytes());
            format!("{conversation}|{post_type}:{:x}", hasher.finalize())
        }
    }
}

/// 可跨线程共享的入站库句柄。所有 SQLite 操作（含 fsync 提交）都在阻塞线程池里执行，
/// 异步主循环只 `await`，不会因磁盘提交而卡住 WebSocket 读取。
#[derive(Clone)]
pub struct SharedJournal(Arc<Mutex<Journal>>);

impl SharedJournal {
    pub fn new(journal: Journal) -> Self {
        Self(Arc::new(Mutex::new(journal)))
    }

    pub async fn run<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Journal) -> Result<T, String> + Send + 'static,
    {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| "入站数据库锁已损坏".to_string())?;
            f(&guard)
        })
        .await
        .map_err(|e| format!("入站数据库任务失败：{e}"))?
    }

    pub async fn record_received(&self, event_key: String, conversation: String, raw: String, now: i64) -> Result<Receipt, String> {
        self.run(move |j| j.record_received(&event_key, &conversation, &raw, now)).await
    }

    pub async fn mark_processing(&self, keys: Vec<String>, now: i64) -> Result<(), String> {
        self.run(move |j| j.mark_processing(&keys, now)).await
    }

    pub async fn mark_handled(&self, keys: Vec<String>, now: i64) -> Result<(), String> {
        self.run(move |j| j.mark_handled(&keys, now)).await
    }

    pub async fn touch_seen(&self, now: i64) -> Result<(), String> {
        self.run(move |j| j.touch_seen(now)).await
    }

    pub async fn recover_interrupted(&self, now: i64) -> Result<Vec<String>, String> {
        self.run(move |j| j.recover_interrupted(now)).await
    }

    pub async fn replayable(&self) -> Result<Vec<Replayable>, String> {
        self.run(|j| j.replayable()).await
    }
}

/// 仅含心跳等元事件、不需要去重或重放的帧。
pub fn is_meta_post_type(post_type: &str) -> bool {
    post_type == "meta_event"
}

/// 记录最近一帧的时刻。元事件（心跳）只更新内存，不落盘；断线时再补写一次，
/// 因此缺口起点仍是真实的最后存活时刻，却不必每 N 秒做一次 fsync 提交。
#[derive(Debug, Default)]
pub struct SeenTracker {
    latest_ms: Option<i64>,
    persisted_ms: Option<i64>,
}

impl SeenTracker {
    /// 返回需要立即落盘的时刻（非元事件帧）；元事件返回 `None`。
    pub fn observe(&mut self, now: i64, post_type: &str) -> Option<i64> {
        self.latest_ms = Some(now);
        if is_meta_post_type(post_type) {
            return None;
        }
        self.persisted_ms = Some(now);
        Some(now)
    }

    /// 断线时需要补写的时刻：只在内存里比已落盘的更新时才有值。
    pub fn pending(&self) -> Option<i64> {
        match (self.latest_ms, self.persisted_ms) {
            (Some(latest), Some(done)) if latest > done => Some(latest),
            (Some(latest), None) => Some(latest),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    New,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayable {
    pub event_key: String,
    pub conversation: String,
    pub raw: String,
}

/// Content-free metadata for the startup owner notice; raw event text never leaves the journal.
#[derive(Debug)]
pub struct InterruptedNotice {
    pub event_key: String,
    pub conversation: String,
    pub event_ms: i64,
    pub received_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapStatus {
    /// 已创建、尚未有补收结果。
    Open,
    /// 窗口内的历史已全部确认补齐，唯一会关闭缺口的状态。
    Recovered,
    /// 无法确定是否补齐（请求失败、历史不完整、翻页上限等）。
    Unknown,
    /// 明确不能补收（网关不支持或拒绝、该会话没有已验证的历史能力）。
    Unrecovered,
}

impl GapStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GapStatus::Open => "open",
            GapStatus::Recovered => "recovered",
            GapStatus::Unknown => "unknown",
            GapStatus::Unrecovered => "unrecovered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub id: i64,
    pub conversation: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub status: String,
    pub reason: String,
    pub detail: String,
    pub recovered: i64,
    pub opened_ms: i64,
    pub updated_ms: i64,
    pub attempts: i64,
    pub next_retry_ms: i64,
    /// Confirmed owner delivery time; zero includes pending and unknown outcomes.
    pub notified_ms: i64,
    /// Durable owner notice state, independent of history recovery status.
    pub notice_state: String,
}

impl Gap {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "conversation": self.conversation,
            "fromMs": self.from_ms,
            "toMs": self.to_ms,
            "status": self.status,
            "reason": self.reason,
            "detail": self.detail,
            "recoveredCount": self.recovered,
            "openedMs": self.opened_ms,
            "updatedMs": self.updated_ms,
            "attempts": self.attempts,
            "nextRetryMs": self.next_retry_ms,
            "notifiedMs": self.notified_ms,
            "noticeState": self.notice_state,
        })
    }
}

fn gap_columns(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn.prepare("PRAGMA table_info(inbound_gaps)").map_err(db_err)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1)).map_err(db_err)?;
    names.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// 打开选定运行目录下的入站库。失败必须阻止启动：没有持久化就不能声称已确认接收。
    pub fn open(context: &RuntimeContext) -> Result<Self, String> {
        let path = context
            .resolve_path(RuntimeArea::Data, DB_NAME, PathIntent::WriteFile)
            .map_err(|e| format!("入站数据库路径不可用：{e}"))?;
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
        if context.is_isolated() {
            flags |= OpenFlags::SQLITE_OPEN_NOFOLLOW;
        }
        let conn = Connection::open_with_flags(&path, flags)
            .map_err(|e| format!("打不开入站数据库：{e}"))?;
        Self::init(conn)
    }

    pub fn open_at(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("打不开入站数据库：{e}"))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;")
            .map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        let migrations = [
            ("attempts", "INTEGER NOT NULL DEFAULT 0"),
            ("next_retry_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("notified_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("notice_state", "TEXT NOT NULL DEFAULT 'pending'"),
        ];
        let private_gap_filter = "reason = 'private-history-unverified' AND status <> 'recovered'
                                  AND (status <> 'unrecovered' OR next_retry_ms <> 0)";
        // Ordinary console opens must remain readers once migration is complete.
        // Missing retry columns short-circuit the row check until they are added.
        let columns = gap_columns(&conn)?;
        if migrations.iter().any(|(column, _)| !columns.iter().any(|name| name == column))
            || conn.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM inbound_gaps WHERE {private_gap_filter})"),
                [], |row| row.get::<_, bool>(0),
            ).map_err(db_err)?
        {
            let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).map_err(db_err)?;
            // Another opener may have migrated while we waited for the reservation.
            let columns = gap_columns(&tx)?;
            for (column, declaration) in migrations {
                if !columns.iter().any(|name| name == column) {
                    tx.execute_batch(&format!("ALTER TABLE inbound_gaps ADD COLUMN {column} {declaration};")).map_err(db_err)?;
                    // Historical windows have no new notice intent.
                    if column == "notice_state" {
                        tx.execute("UPDATE inbound_gaps SET notice_state = 'unknown'", []).map_err(db_err)?;
                    }
                }
            }
            // Preserve historical evidence/attempts while applying the terminal policy.
            tx.execute(
                &format!("UPDATE inbound_gaps SET status = 'unrecovered', next_retry_ms = 0 WHERE {private_gap_filter}"),
                [],
            ).map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        Ok(Self { conn })
    }

    fn record(
        &self,
        event_key: &str,
        conversation: &str,
        origin: &str,
        stage: &str,
        raw: &str,
        received_ms: i64,
        now: i64,
    ) -> Result<Receipt, String> {
        let changed = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO inbound_events
                   (event_key, conversation, origin, stage, received_ms, updated_ms, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![event_key, conversation, origin, stage, received_ms, now, raw],
            )
            .map_err(db_err)?;
        if changed == 1 {
            Ok(Receipt::New)
        } else {
            self.bump_meta("duplicates")?;
            Ok(Receipt::Duplicate)
        }
    }

    /// 实时事件：保存为 `received`。调用方必须在保存成功后才进入唤醒。
    pub fn record_received(
        &self,
        event_key: &str,
        conversation: &str,
        raw: &str,
        now: i64,
    ) -> Result<Receipt, String> {
        self.record(event_key, conversation, "live", "received", raw, now, now)
    }

    /// 补收的历史消息：保存为 `backfilled`，不唤醒。
    pub fn record_backfilled(
        &self,
        event_key: &str,
        conversation: &str,
        raw: &str,
        message_ms: i64,
        now: i64,
    ) -> Result<Receipt, String> {
        self.record(event_key, conversation, "backfill", "backfilled", raw, message_ms, now)
    }

    /// 唤醒已触发：`received` → `processing`。只推进仍在 `received` 的事件。
    pub fn mark_processing(&self, keys: &[String], now: i64) -> Result<(), String> {
        self.transition(keys, now, "received", "processing")
    }

    /// Dispatch preparation failed before a task could submit work. Keep the events replayable.
    pub fn requeue_unsubmitted(&self, keys: &[String], now: i64) -> Result<(), String> {
        self.transition(keys, now, "processing", "received")
    }

    /// 回合结束或事件不产生工作：`received`/`processing` → `handled`。
    pub fn mark_handled(&self, keys: &[String], now: i64) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for key in keys {
            tx.execute(
                "UPDATE inbound_events SET stage = 'handled', updated_ms = ?1
                 WHERE event_key = ?2 AND stage IN ('received', 'processing')",
                params![now, key],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    fn transition(&self, keys: &[String], now: i64, from: &str, to: &str) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for key in keys {
            tx.execute(
                "UPDATE inbound_events SET stage = ?1, updated_ms = ?2
                 WHERE event_key = ?3 AND stage = ?4",
                params![to, now, key, from],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    /// 仍需重放的实时事件（`received`），按保存顺序。
    pub fn replayable(&self) -> Result<Vec<Replayable>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_key, conversation, raw FROM inbound_events
                 WHERE stage = 'received' AND origin = 'live' ORDER BY seq",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Replayable {
                    event_key: row.get(0)?,
                    conversation: row.get(1)?,
                    raw: row.get(2)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    /// 启动时调用：仍为 `processing` 的事件状态未知，改为 `interrupted` 并返回其键。
    /// 不会重新放入唤醒，避免整轮重提交。
    pub fn recover_interrupted(&self, now: i64) -> Result<Vec<String>, String> {
        // Reserve before reading so SQLite can wait for a competing writer.
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate).map_err(db_err)?;
        let keys = {
            let mut stmt = tx
                .prepare("SELECT event_key FROM inbound_events WHERE stage = 'processing' ORDER BY seq")
                .map_err(db_err)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(db_err)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?
        };
        tx.execute(
            "UPDATE inbound_events SET stage = 'interrupted', updated_ms = ?1 WHERE stage = 'processing'",
            params![now],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(keys)
    }

    /// 崩溃留下的 `interrupted` 事件（旧的在前）。回合恢复（ticket 10）据此核对，不在此重提。
    pub fn interrupted_events(&self) -> Result<Vec<Replayable>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_key, conversation, raw FROM inbound_events
                 WHERE stage = 'interrupted' ORDER BY seq",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Replayable { event_key: row.get(0)?, conversation: row.get(1)?, raw: row.get(2)? })
            })
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn interrupted_notice_details(
        &self,
        keys: &[String],
    ) -> Result<Vec<InterruptedNotice>, String> {
        let mut notices = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT conversation, received_ms, raw FROM inbound_events WHERE event_key = ?1 AND stage = 'interrupted'"
        ).map_err(db_err)?;
        for key in keys {
            let row = statement
                .query_row(params![key], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .optional()
                .map_err(db_err)?;
            if let Some((conversation, received_ms, raw)) = row {
                let event_ms = serde_json::from_str::<Value>(&raw)
                    .ok()
                    .and_then(|event| event.get("time").and_then(Value::as_i64))
                    .filter(|seconds| *seconds > 0)
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .unwrap_or(received_ms);
                notices.push(InterruptedNotice {
                    event_key: key.clone(),
                    conversation,
                    event_ms,
                    received_ms,
                });
            }
        }
        Ok(notices)
    }

    /// 事件最近一次改变阶段的时刻（毫秒）。
    pub fn updated_ms_of(&self, event_key: &str) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT updated_ms FROM inbound_events WHERE event_key = ?1",
                params![event_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// 按键取事件原文（回合恢复把暂缓的事件交回重处理时用）。
    pub fn events_by_keys(&self, keys: &[String]) -> Result<Vec<Replayable>, String> {
        let mut out = Vec::new();
        for key in keys {
            let found = self
                .conn
                .query_row(
                    "SELECT event_key, conversation, raw FROM inbound_events WHERE event_key = ?1",
                    params![key],
                    |row| Ok(Replayable { event_key: row.get(0)?, conversation: row.get(1)?, raw: row.get(2)? }),
                )
                .optional()
                .map_err(db_err)?;
            out.extend(found);
        }
        Ok(out)
    }

    /// 回合恢复核对完成后的去向：`handled`（已完成/取消/失效，不再处理）或 `received`
    /// （明确未执行且仍有效，交回重放）。只动 `interrupted`，以及被暂缓后已标 `handled` 的事件
    /// 交回时的 `handled`；其它阶段原样保留。
    pub fn settle_events(&self, keys: &[String], to: &str, now: i64) -> Result<(), String> {
        let from = match to {
            "handled" => "('interrupted')",
            "received" => "('interrupted', 'handled')",
            other => return Err(format!("不支持的目标阶段：{other}")),
        };
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for key in keys {
            tx.execute(
                &format!("UPDATE inbound_events SET stage = ?1, updated_ms = ?2 WHERE event_key = ?3 AND stage IN {from}"),
                params![to, now, key],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    pub fn stage_of(&self, event_key: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT stage FROM inbound_events WHERE event_key = ?1",
                params![event_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// 连接存活的最后时刻（任何入站帧都会刷新）。断线后用它作为缺口的起点。
    pub fn touch_seen(&self, now: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO inbound_meta (key, value) VALUES ('last_seen_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn last_seen_ms(&self) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT value FROM inbound_meta WHERE key = 'last_seen_ms'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    fn bump_meta(&self, key: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO inbound_meta (key, value) VALUES (?1, 1)
                 ON CONFLICT(key) DO UPDATE SET value = value + 1",
                params![key],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn duplicates(&self) -> Result<i64, String> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM inbound_meta WHERE key = 'duplicates'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?
            .unwrap_or(0))
    }

    pub fn open_gap(&self, conversation: &str, from_ms: i64, to_ms: i64, now: i64) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO inbound_gaps
                   (conversation, from_ms, to_ms, status, reason, detail, recovered, opened_ms, updated_ms)
                 VALUES (?1, ?2, ?3, 'open', '', '', 0, ?4, ?4)",
                params![conversation, from_ms, to_ms, now],
            )
            .map_err(db_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 写入补收结果。`recovered_add` 为本次新补入的条数（累加）。
    pub fn set_gap_result(
        &self,
        id: i64,
        status: GapStatus,
        reason: &str,
        detail: &str,
        recovered_add: i64,
        now: i64,
    ) -> Result<(), String> {
        let attempts: i64 = self
            .conn
            .query_row("SELECT attempts FROM inbound_gaps WHERE id = ?1", params![id], |row| row.get(0))
            .optional()
            .map_err(db_err)?
            .unwrap_or(0)
            + 1;
        self.conn
            .execute(
                "UPDATE inbound_gaps
                 SET status = ?1, reason = ?2, detail = ?3, recovered = recovered + ?4, updated_ms = ?5,
                     attempts = ?7, next_retry_ms = ?8
                 WHERE id = ?6",
                params![status.as_str(), reason, detail, recovered_add, now, id, attempts,
                    if reason == "private-history-unverified" { 0 } else { now + gap_backoff_ms(attempts) }],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn read_gaps(&self, filter: &str) -> Result<Vec<Gap>, String> {
        let sql = format!(
            "SELECT id, conversation, from_ms, to_ms, status, reason, detail, recovered, opened_ms, updated_ms,
                    attempts, next_retry_ms, notified_ms, notice_state
             FROM inbound_gaps {filter} ORDER BY id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Gap {
                    id: row.get(0)?,
                    conversation: row.get(1)?,
                    from_ms: row.get(2)?,
                    to_ms: row.get(3)?,
                    status: row.get(4)?,
                    reason: row.get(5)?,
                    detail: row.get(6)?,
                    recovered: row.get(7)?,
                    opened_ms: row.get(8)?,
                    updated_ms: row.get(9)?,
                    attempts: row.get(10)?,
                    next_retry_ms: row.get(11)?,
                    notified_ms: row.get(12)?,
                    notice_state: row.get(13)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn gaps(&self) -> Result<Vec<Gap>, String> {
        self.read_gaps("")
    }

    /// 未解决的缺口：状态不是 `recovered` 的全部。重连成功不会把它们移出。
    pub fn unresolved_gaps(&self) -> Result<Vec<Gap>, String> {
        self.read_gaps("WHERE status <> 'recovered'")
    }

    /// Windows eligible for notice: a history result exists and no queued,
    /// unknown or confirmed send already represents the window.
    pub fn unnotified_gaps(&self) -> Result<Vec<Gap>, String> {
        self.read_gaps("WHERE status IN ('unknown', 'unrecovered', 'recovered') AND notified_ms = 0 AND notice_state = 'pending'")
    }

    /// Called once by the exclusive live owner at startup, before any notice worker.
    /// A queued job has not reached network I/O; unknown attempts must never be reset.
    pub fn recover_queued_gap_notices(&self) -> Result<(), String> {
        self.conn.execute("UPDATE inbound_gaps SET notice_state = 'pending' WHERE notice_state = 'queued' AND notified_ms = 0", []).map_err(db_err)?;
        Ok(())
    }

    /// All represented windows are claimed together or none are. Selection may race,
    /// but a stale selection cannot enqueue a second notice for any claimed window.
    pub fn claim_gap_notice(&self, ids: &[i64]) -> Result<bool, String> {
        self.transition_gap_notice(ids, "pending", "queued")
    }

    pub fn begin_gap_notice(&self, ids: &[i64]) -> Result<bool, String> {
        self.transition_gap_notice(ids, "queued", "unknown")
    }

    fn transition_gap_notice(&self, ids: &[i64], from: &str, to: &str) -> Result<bool, String> {
        if ids.is_empty() { return Ok(false); }
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for id in ids {
            let changed = tx.execute("UPDATE inbound_gaps SET notice_state = ?1 WHERE id = ?2 AND notice_state = ?3 AND notified_ms = 0", params![to, id, from]).map_err(db_err)?;
            if changed != 1 { return Ok(false); } // rollback the entire claim
        }
        tx.commit().map_err(db_err)?;
        Ok(true)
    }

    pub fn release_gap_notice(&self, ids: &[i64], attempted: bool) -> Result<(), String> {
        self.transition_gap_notice(ids, if attempted { "unknown" } else { "queued" }, "pending")?;
        Ok(())
    }

    /// Record confirmed delivery atomically for every represented window.
    pub fn mark_gaps_notified(&self, ids: &[i64], now: i64) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for id in ids {
            tx
                .execute("UPDATE inbound_gaps SET notified_ms = ?1, notice_state = 'sent' WHERE id = ?2 AND notified_ms = 0", params![now, id])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// 按文件头部的保留策略清理，返回 (删除的事件行数, 清空的原始报文数, 删除的缺口行数)。
    pub fn prune(&self, now: i64) -> Result<(usize, usize, usize), String> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        let blanked = tx
            .execute(
                "UPDATE inbound_events SET raw = ''
                 WHERE stage IN ('handled', 'backfilled') AND raw <> '' AND updated_ms < ?1",
                params![now - RAW_RETAIN_MS],
            )
            .map_err(db_err)?;
        let mut events = tx
            .execute(
                "DELETE FROM inbound_events
                 WHERE (stage IN ('handled', 'backfilled') AND updated_ms < ?1)
                    OR (stage = 'interrupted' AND updated_ms < ?2)",
                params![now - FINISHED_RETAIN_MS, now - INTERRUPTED_RETAIN_MS],
            )
            .map_err(db_err)?;
        events += tx
            .execute(
                "DELETE FROM inbound_events WHERE seq IN (
                   SELECT seq FROM inbound_events WHERE stage IN ('handled', 'backfilled')
                   ORDER BY seq DESC LIMIT -1 OFFSET ?1)",
                params![RETAIN_MAX_FINISHED_ROWS],
            )
            .map_err(db_err)?;
        let gaps = tx
            .execute(
                "DELETE FROM inbound_gaps WHERE status = 'recovered' AND updated_ms < ?1",
                params![now - RECOVERED_GAP_RETAIN_MS],
            )
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok((events, blanked, gaps))
    }

    /// 管理员手动重试：清零某会话可重试缺口的重试次数与退避，使其在下一次重连补收时重新尝试。
    /// 不改变缺口状态，返回受影响的缺口数。
    pub fn reset_gap_retries(&self, conversation: &str) -> Result<usize, String> {
        self.conn
            .execute(
                "UPDATE inbound_gaps SET attempts = 0, next_retry_ms = 0
                 WHERE conversation = ?1 AND status <> 'recovered' AND reason <> 'private-history-unverified'",
                params![conversation],
            )
            .map_err(db_err)
    }

    /// 此刻应当重试的缺口：未解决、未超过重试上限、已过退避时间。
    pub fn retryable_gaps(&self, now: i64) -> Result<Vec<Gap>, String> {
        self.read_gaps(&format!(
            "WHERE status <> 'recovered' AND reason <> 'private-history-unverified' AND attempts < {MAX_GAP_ATTEMPTS} AND next_retry_ms <= {now}"
        ))
    }

    /// 管理端摘要：各阶段条数、重复事件数、中断事件键与缺口列表。
    pub fn admin_view(&self) -> Result<Value, String> {
        let mut stages = serde_json::Map::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT stage, COUNT(*) FROM inbound_events GROUP BY stage")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(db_err)?;
            for row in rows {
                let (stage, count) = row.map_err(db_err)?;
                stages.insert(stage, json!(count));
            }
        }
        let interrupted = {
            let mut stmt = self
                .conn
                .prepare("SELECT event_key FROM inbound_events WHERE stage = 'interrupted' ORDER BY seq LIMIT 100")
                .map_err(db_err)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(db_err)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?
        };
        let gaps = self.gaps()?;
        let unresolved_gaps: Vec<Value> = gaps.iter().filter(|g| g.status != "recovered").map(Gap::to_json).collect();
        let unresolved = unresolved_gaps.len();
        Ok(json!({
            "stages": stages,
            "duplicates": self.duplicates()?,
            "interrupted": interrupted,
            "unresolvedGapCount": unresolved,
            "unresolvedGaps": unresolved_gaps,
            "gaps": gaps.iter().map(Gap::to_json).collect::<Vec<_>>(),
        }))
    }
}

/// 一条历史消息。`seq` 是网关的 `message_seq`，用作下一页游标。
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMessage {
    pub message_id: String,
    pub time_ms: i64,
    pub seq: Option<i64>,
    pub raw: Value,
}

/// 一页历史。`dropped` 是缺少 `message_id` 或 `time` 而无法使用的条目数；
/// 这些条目可能落在窗口内，因此只要有丢弃就不能证明窗口已补齐。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistoryPage {
    pub messages: Vec<HistoryMessage>,
    pub dropped: usize,
}

impl From<Vec<HistoryMessage>> for HistoryPage {
    fn from(messages: Vec<HistoryMessage>) -> Self {
        Self { messages, dropped: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 网关明确不支持或拒绝（404/405/501 或非零 retcode）。
    Rejected(String),
    /// 传输失败、超时或响应无法解析：结果未知。
    Transport(String),
    /// 网关明确报告了临时性执行错误（OneBot 11 的 1200/1300/15xx，OneBot 12 的 20002 与 3xxxx）：
    /// 重试可能成功，结果未知。
    Transient(String),
    /// 非零 retcode 但无法判断是永久还是临时：不声称不能提供，结果未知。
    Unclear(String),
}

/// 按 OneBot 约定对非零 `retcode` 分类。只有明确属于“请求本身不被接受”的才算永久拒绝。
pub fn classify_retcode(retcode: i64) -> FetchError {
    let detail = format!("retcode {retcode}");
    match retcode {
        100 | 1400 | 1401 | 1403 | 1404 | 10001..=10004 | 20001 => FetchError::Rejected(detail),
        1200 | 1300 | 1500..=1599 | 20002 | 31000..=36999 => FetchError::Transient(detail),
        _ => FetchError::Unclear(detail),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackfillOutcome {
    pub status: GapStatus,
    pub reason: String,
    pub detail: String,
    pub recovered: i64,
}

/// 按真实历史能力补收一个缺口窗口 `[from_ms, to_ms]`。
///
/// 从最新页开始向前翻页，直到某条消息的时间不晚于 `from_ms`（窗口已被完整覆盖）。
/// 只有覆盖证明成立才记为 `recovered`；其余情况按原因记为 `unknown` 或 `unrecovered`。
pub fn backfill_window(
    journal: &Journal,
    conversation: &str,
    from_ms: i64,
    to_ms: i64,
    now: i64,
    fetch: &mut dyn FnMut(Option<i64>) -> Result<HistoryPage, FetchError>,
) -> Result<BackfillOutcome, String> {
    let outcome = |status, reason: &str, detail: String, recovered| BackfillOutcome {
        status,
        reason: reason.to_string(),
        detail,
        recovered,
    };
    let mut cursor: Option<i64> = None;
    let mut seen_any = false;
    let mut recovered = 0i64;
    let mut dropped = 0usize;
    for _ in 0..MAX_HISTORY_PAGES {
        let page = match fetch(cursor) {
            Ok(page) => page,
            Err(FetchError::Rejected(detail)) => {
                return Ok(outcome(GapStatus::Unrecovered, "history-unsupported", detail, recovered))
            }
            Err(FetchError::Transport(detail)) => {
                return Ok(outcome(GapStatus::Unknown, "history-unreachable", detail, recovered))
            }
            Err(FetchError::Transient(detail)) => {
                return Ok(outcome(GapStatus::Unknown, "history-transient-retcode", detail, recovered))
            }
            Err(FetchError::Unclear(detail)) => {
                return Ok(outcome(GapStatus::Unknown, "history-retcode-unclear", detail, recovered))
            }
        };
        dropped += page.dropped;
        let page = page.messages;
        if page.is_empty() {
            if dropped > 0 {
                return Ok(outcome(
                    GapStatus::Unknown,
                    "history-incomplete",
                    format!("{dropped} history entries lacked message_id or time and were dropped"),
                    recovered,
                ));
            }
            let (reason, detail) = if seen_any {
                ("history-exhausted", "history ended before the window start".to_string())
            } else {
                ("history-empty", "gateway returned no messages".to_string())
            };
            return Ok(outcome(GapStatus::Unknown, reason, detail, recovered));
        }
        seen_any = true;
        for message in &page {
            if message.time_ms < from_ms || message.time_ms > to_ms {
                continue;
            }
            let key = event_key(conversation, "message", Some(&message.message_id), "");
            let raw = message.raw.to_string();
            if journal.record_backfilled(&key, conversation, &raw, message.time_ms, now)? == Receipt::New {
                recovered += 1;
            }
        }
        let oldest = page
            .iter()
            .min_by_key(|message| message.time_ms)
            .expect("page is non-empty");
        if oldest.time_ms <= from_ms {
            if dropped > 0 {
                return Ok(outcome(
                    GapStatus::Unknown,
                    "history-incomplete",
                    format!("{dropped} history entries lacked message_id or time and were dropped"),
                    recovered,
                ));
            }
            return Ok(outcome(GapStatus::Recovered, "covered", String::new(), recovered));
        }
        let Some(next) = oldest.seq else {
            return Ok(outcome(
                GapStatus::Unknown,
                "no-cursor",
                "history page carries no message_seq to continue from".to_string(),
                recovered,
            ));
        };
        if cursor == Some(next) {
            return Ok(outcome(
                GapStatus::Unknown,
                "no-progress",
                format!("history paging did not advance past seq {next}"),
                recovered,
            ));
        }
        cursor = Some(next);
    }
    Ok(outcome(
        GapStatus::Unknown,
        "page-limit",
        format!("stopped after {MAX_HISTORY_PAGES} pages before reaching the window start"),
        recovered,
    ))
}

/// 唤醒已触发：该会话等待中的事件转为 `processing`，之后由主循环在回合结束时标为 `handled`。
pub async fn start_processing(
    journal: &SharedJournal,
    awaiting: &mut std::collections::HashMap<String, Vec<String>>,
    in_flight: &mut Vec<String>,
    conversation: &str,
    now: i64,
) -> Result<(), String> {
    if let Some(keys) = awaiting.remove(conversation) {
        journal.mark_processing(keys.clone(), now).await?;
        in_flight.extend(keys);
    }
    Ok(())
}

/// 每次（重）连接后立即调用（快，仅写库）：以上次存活时刻为起点，为每个允许会话记录缺口。
/// 必须在读取新帧之前完成，否则 `last_seen_ms` 会被新帧刷新而丢掉缺口起点。
pub fn open_reconnect_gaps(
    journal: &Journal,
    groups: &[String],
    privates: &[String],
    now: i64,
) -> Result<usize, String> {
    let mut opened = 0;
    if let Some(last_seen) = journal.last_seen_ms()? {
        if last_seen < now {
            for group in groups {
                journal.open_gap(&format!("group:{group}"), last_seen, now, now)?;
                opened += 1;
            }
            for private in privates {
                let id = journal.open_gap(&format!("private:{private}"), last_seen, now, now)?;
                journal.set_gap_result(
                    id, GapStatus::Unrecovered, "private-history-unverified",
                    "no verified private-chat history capability in the gateway contract", 0, now,
                )?;
                opened += 1;
            }
        }
    }
    Ok(opened)
}

/// 补收到期的未解决缺口（含阻塞 HTTP）。必须在后台线程里运行，不能占用异步主循环。
pub fn retry_unresolved_gaps(
    journal: &Journal,
    fetch_for: &(dyn Fn(i64, Option<i64>) -> Result<HistoryPage, FetchError> + Sync),
    now: i64,
) -> Result<Vec<String>, String> {
    let mut report = Vec::new();
    for gap in journal.retryable_gaps(now)? {
        if let Some(group_id) = gap.conversation.strip_prefix("group:").and_then(|g| g.parse::<i64>().ok()) {
            let mut fetch = |seq: Option<i64>| fetch_for(group_id, seq);
            let outcome = backfill_window(journal, &gap.conversation, gap.from_ms, gap.to_ms, now, &mut fetch)?;
            journal.set_gap_result(gap.id, outcome.status, &outcome.reason, &outcome.detail, outcome.recovered, now)?;
            report.push(format!("{} {} {}", gap.conversation, outcome.status.as_str(), outcome.reason));
        } else {
            journal.set_gap_result(
                gap.id,
                GapStatus::Unrecovered,
                "private-history-unverified",
                "no verified private-chat history capability in the gateway contract",
                0,
                now,
            )?;
            report.push(format!("{} unrecovered private-history-unverified", gap.conversation));
        }
    }
    Ok(report)
}

/// 同一时刻只允许一个后台补收任务；已有任务在跑时新的重连只记缺口，缺口留待下一次。
static RECOVERY_RUNNING: AtomicBool = AtomicBool::new(false);

pub struct RecoveryGuard;

impl RecoveryGuard {
    pub fn try_acquire() -> Option<Self> {
        RECOVERY_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RecoveryGuard)
    }
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        RECOVERY_RUNNING.store(false, Ordering::Release);
    }
}

/// 在阻塞线程池里后台补收：用自己的数据库连接，主循环不等待它。返回的句柄只供测试等待。
pub fn spawn_gap_recovery(
    open: impl FnOnce() -> Result<Journal, String> + Send + 'static,
    fetch_for: impl Fn(i64, Option<i64>) -> Result<HistoryPage, FetchError> + Send + Sync + 'static,
    now: i64,
    on_report: impl FnOnce(Result<Vec<String>, String>) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let guard = RecoveryGuard::try_acquire()?;
    Some(tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let result = open().and_then(|journal| {
            // 保留清理失败不影响补收，只在报告里体现。
            let pruned = journal.prune(now);
            let mut report = retry_unresolved_gaps(&journal, &fetch_for, now)?;
            if let Err(error) = pruned {
                report.push(format!("retention prune failed: {error}"));
            }
            Ok(report)
        });
        on_report(result);
    }))
}

/// 调用网关的 OneBot HTTP 历史接口 `get_group_msg_history`。
pub fn fetch_group_history(
    host: &str,
    port: u16,
    token: &str,
    group_id: i64,
    message_seq: Option<i64>,
) -> Result<HistoryPage, FetchError> {
    let mut body = json!({ "group_id": group_id });
    if let Some(seq) = message_seq {
        body["message_seq"] = json!(seq);
    }
    let (status, text) = http_post(host, port, "/get_group_msg_history", token, &body.to_string())
        .map_err(FetchError::Transport)?;
    match status {
        200 => {}
        404 | 405 | 501 => return Err(FetchError::Rejected(format!("HTTP {status}"))),
        other => return Err(FetchError::Transport(format!("HTTP {other}"))),
    }
    let (value, _) = crate::aux_text::parse_json_replacing_lone_surrogates::<Value>(&text)
        .map_err(|e| FetchError::Transport(format!("history response is not JSON: {e}")))?;
    let Some(retcode) = value.get("retcode").and_then(Value::as_i64) else {
        return Err(FetchError::Unclear("history response has no retcode".to_string()));
    };
    if retcode != 0 {
        return Err(classify_retcode(retcode));
    }
    let messages = value
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| FetchError::Rejected("history response has no data.messages".to_string()))?;
    Ok(parse_history_page(messages))
}

fn parse_history_page(entries: &[Value]) -> HistoryPage {
    let mut page = HistoryPage::default();
    for entry in entries {
        match parse_history_message(entry) {
            Some(message) => page.messages.push(message),
            None => page.dropped += 1,
        }
    }
    page
}

fn parse_history_message(raw: &Value) -> Option<HistoryMessage> {
    let message_id = match raw.get("message_id")? {
        Value::String(id) if !id.is_empty() => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => return None,
    };
    let time_ms = raw.get("time")?.as_i64()?.checked_mul(1000)?;
    Some(HistoryMessage {
        message_id,
        time_ms,
        seq: raw.get("message_seq").and_then(Value::as_i64),
        raw: raw.clone(),
    })
}

fn http_post(host: &str, port: u16, path: &str, token: &str, body: &str) -> Result<(u16, String), String> {
    let address = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(8))))
        .map_err(|e| format!("set timeout: {e}"))?;
    let auth = if token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {token}\r\n")
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP status line".to_string())?;
    let chunked = head
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:") && line.to_ascii_lowercase().contains("chunked"));
    let body = if chunked { decode_chunked(payload)? } else { payload.to_string() };
    Ok((status, body))
}

/// 解码 `Transfer-Encoding: chunked` 的响应体。
fn decode_chunked(payload: &str) -> Result<String, String> {
    let mut rest = payload;
    let mut out = String::new();
    loop {
        let (size_line, after) = rest
            .split_once("\r\n")
            .ok_or_else(|| "truncated chunked body".to_string())?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size".to_string())?;
        if size == 0 {
            return Ok(out);
        }
        let chunk = after.get(..size).ok_or_else(|| "truncated chunk".to_string())?;
        out.push_str(chunk);
        rest = after
            .get(size..)
            .and_then(|tail| tail.strip_prefix("\r\n"))
            .ok_or_else(|| "bad chunk terminator".to_string())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs;

    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-inbound-{name}-{stamp}")));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn db(&self) -> std::path::PathBuf {
            self.0.join("inbound.db")
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn msg(id: &str, time_s: i64, seq: Option<i64>) -> HistoryMessage {
        HistoryMessage {
            message_id: id.to_string(),
            time_ms: time_s * 1000,
            seq,
            raw: json!({"message_id": id, "time": time_s, "message_seq": seq}),
        }
    }

    #[test]
    fn hotfix127_manual_retry_cannot_revive_a_terminal_private_gap() {
        let dir = TempDb::new("terminal-private-retry");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let private = journal.open_gap("private:1", 1, 2, 3).unwrap();
        journal.set_gap_result(private, GapStatus::Unrecovered, "private-history-unverified", "evidence", 0, 4).unwrap();
        assert_eq!(journal.reset_gap_retries("private:1").unwrap(), 0);
        let gap = &journal.gaps().unwrap()[0];
        assert_eq!((gap.attempts, gap.next_retry_ms, gap.updated_ms), (1, 0, 4));
        assert!(journal.retryable_gaps(i64::MAX).unwrap().is_empty());
        let group = journal.open_gap("group:1", 1, 2, 3).unwrap();
        journal.set_gap_result(group, GapStatus::Unknown, "history-empty", "evidence", 0, 4).unwrap();
        assert_eq!(journal.reset_gap_retries("group:1").unwrap(), 1);
        assert_eq!(journal.retryable_gaps(5).unwrap().iter().map(|g| g.id).collect::<Vec<_>>(), vec![group]);
    }

    #[test]
    fn hotfix127_fix3_migrated_open_reads_gaps_while_another_connection_writes() {
        let dir = TempDb::new("migrated-open-writer");
        let writer = Journal::open_at(&dir.db()).unwrap();
        let id = writer.open_gap("group:1", 1, 2, 3).unwrap();
        let tx = rusqlite::Transaction::new_unchecked(
            &writer.conn, rusqlite::TransactionBehavior::Immediate,
        ).unwrap();
        let path = dir.db();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = Journal::open_at(&path).and_then(|journal| journal.gaps());
            result_tx.send(result).unwrap();
        });
        // Keep the writer reserved until the console's real open/read finishes. The
        // bounded wait also releases it before open's five-second busy timeout on red.
        let result = result_rx.recv_timeout(Duration::from_secs(2));
        tx.commit().unwrap();
        reader.join().unwrap();
        let gaps = result.expect("an initialized console read must not wait for a writer").unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].id, id);
    }

    #[test]
    fn hotfix127_fix3_startup_recovery_waits_for_the_writer_before_reading() {
        use std::sync::mpsc::{channel, Receiver, Sender};
        // Observe SQLite's actual busy callback, rather than guessing when recovery
        // reached the lock with a sleep. Read-to-write promotion skips this callback.
        thread_local! {
            static BUSY_RELEASE: std::cell::RefCell<Option<(Sender<bool>, Receiver<()>)>> = const { std::cell::RefCell::new(None) };
        }
        fn wait_for_release(_: i32) -> bool {
            BUSY_RELEASE.with(|state| {
                let state = state.borrow();
                let (observed, release) = state.as_ref().unwrap();
                observed.send(true).unwrap();
                release.recv_timeout(Duration::from_secs(2)).is_ok()
            })
        }

        let dir = TempDb::new("recovery-writer");
        let startup = Journal::open_at(&dir.db()).unwrap();
        startup.record_received("before", "group:1", "raw", 1).unwrap();
        startup.record_received("during", "group:1", "raw", 2).unwrap();
        startup.mark_processing(&["before".into()], 3).unwrap();
        let writer = Journal::open_at(&dir.db()).unwrap();
        let tx = rusqlite::Transaction::new_unchecked(
            &writer.conn, rusqlite::TransactionBehavior::Immediate,
        ).unwrap();
        tx.execute("UPDATE inbound_events SET stage = 'processing' WHERE event_key = 'during'", []).unwrap();
        let (observed_tx, observed_rx) = channel();
        let (release_tx, release_rx) = channel();
        let recovery = std::thread::spawn(move || {
            BUSY_RELEASE.with(|state| *state.borrow_mut() = Some((observed_tx.clone(), release_rx)));
            startup.conn.busy_handler(Some(wait_for_release)).unwrap();
            let result = startup.recover_interrupted(4);
            // On red, signal completion instead of hanging while waiting for a busy
            // callback which SQLite never calls for the deferred promotion.
            observed_tx.send(false).unwrap();
            result
        });
        let waited = observed_rx.recv_timeout(Duration::from_secs(3));
        tx.commit().unwrap();
        let _ = release_tx.send(());
        let result = recovery.join().unwrap();
        assert_eq!(waited.unwrap(), true, "recovery must wait, not fail immediately: {result:?}");
        assert_eq!(result.unwrap(), vec!["before", "during"], "read the committed writer state");
        assert_eq!(writer.stage_of("before").unwrap().as_deref(), Some("interrupted"));
        assert_eq!(writer.stage_of("during").unwrap().as_deref(), Some("interrupted"));
        assert!(writer.recover_interrupted(5).unwrap().is_empty(), "report each event only once");
    }

    #[test]
    fn duplicate_event_identity_is_rejected_and_counted() {
        let dir = TempDb::new("dup");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let key = event_key("group:1", "message", Some("42"), "{}");
        assert_eq!(journal.record_received(&key, "group:1", "{}", 10).unwrap(), Receipt::New);
        assert_eq!(journal.record_received(&key, "group:1", "{}", 11).unwrap(), Receipt::Duplicate);
        assert_eq!(journal.duplicates().unwrap(), 1);
        assert_eq!(journal.stage_of(&key).unwrap().as_deref(), Some("received"));
    }

    #[test]
    fn message_identity_ignores_raw_text_but_notices_use_raw() {
        let by_id = event_key("group:1", "message", Some("7"), "a");
        assert_eq!(by_id, event_key("group:1", "message", Some("7"), "b"));
        let notice_a = event_key("group:1", "notice", None, "a");
        let notice_b = event_key("group:1", "notice", None, "b");
        assert_ne!(notice_a, notice_b);
        assert_ne!(by_id, event_key("private:1", "message", Some("7"), "a"));
    }

    #[test]
    fn saved_events_survive_reopen_and_replay_in_order() {
        let dir = TempDb::new("reopen");
        let first = event_key("group:1", "message", Some("1"), "");
        let second = event_key("group:1", "message", Some("2"), "");
        let third = event_key("group:1", "message", Some("3"), "");
        {
            let journal = Journal::open_at(&dir.db()).unwrap();
            journal.record_received(&first, "group:1", "r1", 1).unwrap();
            journal.record_received(&second, "group:1", "r2", 2).unwrap();
            journal.record_received(&third, "group:1", "r3", 3).unwrap();
            journal.mark_processing(std::slice::from_ref(&second), 4).unwrap();
            journal.mark_handled(std::slice::from_ref(&third), 5).unwrap();
        }
        let journal = Journal::open_at(&dir.db()).unwrap();
        let interrupted = journal.recover_interrupted(6).unwrap();
        assert_eq!(interrupted, vec![second.clone()]);
        assert_eq!(journal.stage_of(&second).unwrap().as_deref(), Some("interrupted"));
        let replay: Vec<String> = journal.replayable().unwrap().into_iter().map(|r| r.event_key).collect();
        assert_eq!(replay, vec![first]);
    }

    #[test]
    fn restart_after_save_before_processing_replays_without_duplicate_record() {
        let dir = TempDb::new("saved");
        let key = event_key("group:1", "message", Some("9"), "");
        Journal::open_at(&dir.db())
            .unwrap()
            .record_received(&key, "group:1", "raw", 1)
            .unwrap();
        let journal = Journal::open_at(&dir.db()).unwrap();
        assert_eq!(journal.replayable().unwrap().len(), 1);
        assert_eq!(journal.record_received(&key, "group:1", "raw", 2).unwrap(), Receipt::Duplicate);
        assert_eq!(journal.replayable().unwrap().len(), 1);
    }

    #[test]
    fn gap_stays_unresolved_until_recovered() {
        let dir = TempDb::new("gap");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let id = journal.open_gap("group:1", 0, 100, 100).unwrap();
        journal
            .set_gap_result(id, GapStatus::Unknown, "history-empty", "none", 0, 110)
            .unwrap();
        assert_eq!(journal.unresolved_gaps().unwrap().len(), 1);
        journal.touch_seen(120).unwrap();
        assert_eq!(journal.unresolved_gaps().unwrap().len(), 1, "reconnect alone must not clear the gap");
        journal
            .set_gap_result(id, GapStatus::Recovered, "covered", "", 3, 130)
            .unwrap();
        assert!(journal.unresolved_gaps().unwrap().is_empty());
        let gap = &journal.gaps().unwrap()[0];
        assert_eq!(gap.recovered, 3);
        assert_eq!(gap.status, "recovered");
    }

    #[test]
    fn backfill_pages_back_to_window_start_and_dedups_live_copy() {
        let dir = TempDb::new("backfill");
        let journal = Journal::open_at(&dir.db()).unwrap();
        // 断线前实时收到过 message 103，补收时必须识别为重复。
        let live_103 = event_key("group:1", "message", Some("103"), "");
        journal.record_received(&live_103, "group:1", "live", 103_000).unwrap();
        journal.mark_handled(std::slice::from_ref(&live_103), 103_001).unwrap();
        let pages: RefCell<VecDeque<Vec<HistoryMessage>>> = RefCell::new(VecDeque::from(vec![
            vec![msg("104", 104, Some(40)), msg("103", 103, Some(39))],
            vec![msg("102", 102, Some(38)), msg("101", 101, Some(37))],
        ]));
        let cursors = RefCell::new(Vec::new());
        let mut fetch = |cursor: Option<i64>| -> Result<HistoryPage, FetchError> {
            cursors.borrow_mut().push(cursor);
            Ok(pages.borrow_mut().pop_front().unwrap_or_default().into())
        };
        // 窗口 [101.5s, 104.5s]：101 在窗口前（不计入），102/104 补入，103 重复。
        let outcome = backfill_window(&journal, "group:1", 101_500, 104_500, 200_000, &mut fetch).unwrap();
        assert_eq!(outcome.status, GapStatus::Recovered, "{outcome:?}");
        assert_eq!(outcome.reason, "covered");
        assert_eq!(outcome.recovered, 2);
        assert_eq!(cursors.borrow().as_slice(), &[None, Some(39)]);
        let stage = journal.stage_of(&event_key("group:1", "message", Some("102"), "")).unwrap();
        assert_eq!(stage.as_deref(), Some("backfilled"));
        assert!(journal.replayable().unwrap().is_empty(), "backfilled events must not be replayed as wakes");
        assert_eq!(journal.duplicates().unwrap(), 1);
    }

    #[test]
    fn backfill_reports_unsupported_gateway_as_unrecovered() {
        let dir = TempDb::new("unsupported");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let mut fetch = |_: Option<i64>| -> Result<HistoryPage, FetchError> {
            Err(FetchError::Rejected("retcode 1404".into()))
        };
        let outcome = backfill_window(&journal, "group:1", 0, 10, 10, &mut fetch).unwrap();
        assert_eq!(outcome.status, GapStatus::Unrecovered);
        assert_eq!(outcome.reason, "history-unsupported");
    }

    #[test]
    fn backfill_reports_transport_failure_and_empty_history_as_unknown() {
        let dir = TempDb::new("unknown");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let mut down = |_: Option<i64>| -> Result<HistoryPage, FetchError> {
            Err(FetchError::Transport("connection refused".into()))
        };
        let outcome = backfill_window(&journal, "group:1", 0, 10, 10, &mut down).unwrap();
        assert_eq!((outcome.status, outcome.reason.as_str()), (GapStatus::Unknown, "history-unreachable"));
        let mut empty = |_: Option<i64>| -> Result<HistoryPage, FetchError> { Ok(HistoryPage::default()) };
        let outcome = backfill_window(&journal, "group:1", 0, 10, 10, &mut empty).unwrap();
        assert_eq!((outcome.status, outcome.reason.as_str()), (GapStatus::Unknown, "history-empty"));
    }

    #[test]
    fn backfill_refuses_to_claim_coverage_when_paging_stalls() {
        let dir = TempDb::new("stall");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let calls = Cell::new(0);
        let mut fetch = |_: Option<i64>| -> Result<HistoryPage, FetchError> {
            calls.set(calls.get() + 1);
            Ok(vec![msg("500", 500, Some(7))].into())
        };
        let outcome = backfill_window(&journal, "group:1", 0, 10_000_000, 10, &mut fetch).unwrap();
        assert_eq!(outcome.status, GapStatus::Unknown);
        assert_eq!(outcome.reason, "no-progress");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn chunked_bodies_decode_and_history_items_parse() {
        assert_eq!(decode_chunked("3\r\nabc\r\n0\r\n\r\n").unwrap(), "abc");
        assert!(decode_chunked("zz\r\nabc\r\n0\r\n\r\n").is_err());
        let parsed = parse_history_message(&json!({"message_id": 5, "time": 9, "message_seq": 3})).unwrap();
        assert_eq!((parsed.message_id.as_str(), parsed.time_ms, parsed.seq), ("5", 9000, Some(3)));
    }

    #[test]
    fn heartbeat_frames_are_not_persisted_but_flush_on_disconnect() {
        let mut seen = SeenTracker::default();
        assert_eq!(seen.observe(100, "message"), Some(100));
        assert_eq!(seen.pending(), None);
        assert_eq!(seen.observe(200, "meta_event"), None, "heartbeat must not touch the database");
        assert_eq!(seen.observe(300, "meta_event"), None);
        assert_eq!(seen.pending(), Some(300), "the real last-alive time is written once at disconnect");
        assert_eq!(seen.observe(400, "notice"), Some(400));
        assert_eq!(seen.pending(), None);
        let mut only_meta = SeenTracker::default();
        assert_eq!(only_meta.observe(5, "meta_event"), None);
        assert_eq!(only_meta.pending(), Some(5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_history_call_does_not_stop_frame_handling() {
        let dir = TempDb::new("slow-history");
        let path = dir.db();
        let shared = SharedJournal::new(Journal::open_at(&path).unwrap());
        shared.run(|j| j.open_gap("group:1", 0, 100, 100)).await.unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
        let open_path = path.clone();
        let handle = spawn_gap_recovery(
            move || Journal::open_at(&open_path),
            move |_, _| {
                let _ = started_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
                Ok(HistoryPage::default())
            },
            200,
            |_| {},
        )
        .expect("no other recovery is running in this test");
        started_rx.recv_timeout(Duration::from_secs(10)).expect("history call started");
        // 历史请求仍卡住时，同一个（单线程）异步运行时必须继续处理入站帧。
        let key = event_key("group:1", "message", Some("77"), "");
        let receipt = tokio::time::timeout(
            Duration::from_secs(3),
            shared.record_received(key.clone(), "group:1".into(), "raw".into(), 150),
        )
        .await
        .expect("frame handling must not wait for the slow history call")
        .unwrap();
        assert_eq!(receipt, Receipt::New);
        assert!(spawn_gap_recovery(|| Err("x".into()), |_, _| Ok(HistoryPage::default()), 0, |_| {}).is_none(), "only one recovery at a time");
        release_tx.send(()).unwrap();
        handle.await.unwrap();
        let gap = &Journal::open_at(&path).unwrap().gaps().unwrap()[0];
        assert_eq!((gap.status.as_str(), gap.reason.as_str()), ("unknown", "history-empty"));
    }

    #[test]
    fn dropped_history_entries_downgrade_a_covered_range_to_unknown() {
        let dir = TempDb::new("dropped");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let page = parse_history_page(&[
            json!({"message_id": 104, "time": 104, "message_seq": 40}),
            json!({"time": 103, "message_seq": 39}),
            json!({"message_id": 102, "message_seq": 38}),
            json!({"message_id": 101, "time": 101, "message_seq": 37}),
        ]);
        assert_eq!((page.messages.len(), page.dropped), (2, 2));
        let mut once = Some(page);
        let mut fetch = |_: Option<i64>| -> Result<HistoryPage, FetchError> { Ok(once.take().unwrap_or_default()) };
        let outcome = backfill_window(&journal, "group:1", 101_500, 104_500, 200_000, &mut fetch).unwrap();
        assert_eq!(outcome.status, GapStatus::Unknown, "{outcome:?}");
        assert_eq!(outcome.reason, "history-incomplete");
        assert!(outcome.detail.contains('2'), "detail must count dropped entries: {}", outcome.detail);
        assert_eq!(outcome.recovered, 1, "usable in-window entries are still saved");
    }

    #[test]
    fn a_page_of_only_dropped_entries_is_not_reported_as_empty_history() {
        let dir = TempDb::new("all-dropped");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let mut fetch = |_: Option<i64>| -> Result<HistoryPage, FetchError> {
            Ok(HistoryPage { messages: Vec::new(), dropped: 3 })
        };
        let outcome = backfill_window(&journal, "group:1", 0, 10, 10, &mut fetch).unwrap();
        assert_eq!((outcome.status, outcome.reason.as_str()), (GapStatus::Unknown, "history-incomplete"));
    }

    #[test]
    fn prune_is_bounded_and_never_drops_unfinished_work_or_unresolved_gaps() {
        let dir = TempDb::new("prune");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let now = 100 * DAY_MS;
        let old = now - 40 * DAY_MS;
        let mid = now - 3 * DAY_MS;
        let keys: Vec<String> = ["recv", "proc", "done-old", "done-mid", "done-new", "back-old", "intr-old", "intr-mid"]
            .iter()
            .map(|n| event_key("group:1", "message", Some(n), ""))
            .collect();
        let at = [old, old, old, mid, now, old, old, mid];
        for (key, t) in keys.iter().zip(at) {
            journal.record_received(key, "group:1", "raw-frame", t).unwrap();
        }
        journal.mark_processing(&[keys[1].clone()], old).unwrap();
        journal.mark_handled(&[keys[2].clone(), keys[3].clone(), keys[4].clone(), keys[5].clone()], 0).unwrap();
        // mark_handled stamps `now`; pin the update times explicitly.
        for (key, t) in [(&keys[2], old), (&keys[3], mid), (&keys[4], now), (&keys[5], old)] {
            journal.conn.execute("UPDATE inbound_events SET updated_ms = ?1 WHERE event_key = ?2", params![t, key]).unwrap();
        }
        journal.conn.execute("UPDATE inbound_events SET stage = 'interrupted', updated_ms = ?1 WHERE event_key = ?2", params![old, keys[6]]).unwrap();
        journal.conn.execute("UPDATE inbound_events SET stage = 'interrupted', updated_ms = ?1 WHERE event_key = ?2", params![mid, keys[7]]).unwrap();
        let unresolved = journal.open_gap("group:1", 0, 10, old).unwrap();
        journal.set_gap_result(unresolved, GapStatus::Unrecovered, "x", "y", 0, old).unwrap();
        let unknown = journal.open_gap("group:1", 0, 10, old).unwrap();
        journal.set_gap_result(unknown, GapStatus::Unknown, "x", "y", 0, old).unwrap();
        let open = journal.open_gap("group:1", 0, 10, old).unwrap();
        let recovered_old = journal.open_gap("group:1", 0, 10, old).unwrap();
        journal.set_gap_result(recovered_old, GapStatus::Recovered, "covered", "", 1, old).unwrap();
        let recovered_new = journal.open_gap("group:1", 0, 10, now).unwrap();
        journal.set_gap_result(recovered_new, GapStatus::Recovered, "covered", "", 1, now).unwrap();

        let (events, blanked, gaps) = journal.prune(now).unwrap();
        let stage = |i: usize| journal.stage_of(&keys[i]).unwrap();
        assert_eq!(stage(0).as_deref(), Some("received"), "received must survive");
        assert_eq!(stage(1).as_deref(), Some("processing"), "processing must survive");
        assert_eq!(stage(2), None, "old handled row is deleted");
        assert_eq!(stage(3).as_deref(), Some("handled"), "3-day-old handled row is kept for dedup");
        assert_eq!(stage(4).as_deref(), Some("handled"));
        assert_eq!(stage(5), None, "old backfilled row is deleted");
        assert_eq!(stage(6), None, "interrupted older than 30 days is deleted");
        assert_eq!(stage(7).as_deref(), Some("interrupted"));
        assert_eq!(events, 3);
        let raw_of = |i: usize| -> String {
            journal.conn.query_row("SELECT raw FROM inbound_events WHERE event_key = ?1", params![&keys[i]], |r| r.get(0)).unwrap()
        };
        assert_eq!(raw_of(3), "", "raw frame of a >1-day-old handled event is cleared");
        assert_eq!(raw_of(4), "raw-frame", "recent raw frame is kept");
        assert_eq!(raw_of(0), "raw-frame", "received raw frame is needed for replay");
        assert_eq!(blanked, 3, "raw is cleared before row deletion: done-old, done-mid, back-old");
        let ids: Vec<i64> = journal.gaps().unwrap().iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![unresolved, unknown, open, recovered_new]);
        assert_eq!(gaps, 1);
    }

    #[test]
    fn prune_caps_finished_rows_keeping_the_newest() {
        let dir = TempDb::new("prune-cap");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let now = 10 * DAY_MS;
        let total = RETAIN_MAX_FINISHED_ROWS + 5;
        let tx = journal.conn.unchecked_transaction().unwrap();
        for n in 0..total {
            tx.execute(
                "INSERT INTO inbound_events (event_key, conversation, origin, stage, received_ms, updated_ms, raw)
                 VALUES (?1, 'group:1', 'live', 'handled', ?2, ?2, '')",
                params![format!("k{n}"), now],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let (events, _, _) = journal.prune(now).unwrap();
        assert_eq!(events, 5);
        assert_eq!(journal.stage_of("k0").unwrap(), None);
        assert_eq!(journal.stage_of("k4").unwrap(), None);
        assert_eq!(journal.stage_of("k5").unwrap().as_deref(), Some("handled"));
        assert_eq!(journal.stage_of(&format!("k{}", total - 1)).unwrap().as_deref(), Some("handled"));
    }

    #[test]
    fn unresolved_gap_retries_back_off_and_stop_at_the_cap_without_clearing() {
        assert_eq!(gap_backoff_ms(1), 60_000);
        assert_eq!(gap_backoff_ms(2), 120_000);
        assert_eq!(gap_backoff_ms(30), GAP_BACKOFF_MAX_MS);
        let dir = TempDb::new("backoff");
        let journal = Journal::open_at(&dir.db()).unwrap();
        journal.open_gap("private:9", 0, 10, 1_000).unwrap();
        let group = journal.open_gap("group:1", 0, 10, 1_000).unwrap();
        let calls = std::sync::atomic::AtomicI32::new(0);
        let fetch_for = |_: i64, _: Option<i64>| -> Result<HistoryPage, FetchError> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(FetchError::Transport("down".into()))
        };
        let mut now = 1_000;
        let report = retry_unresolved_gaps(&journal, &fetch_for, now).unwrap();
        assert_eq!(report.len(), 2, "first attempt covers both gaps");
        // 退避窗口内的重连不重试。
        now += 30_000;
        assert!(retry_unresolved_gaps(&journal, &fetch_for, now).unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 之后每次都等到退避到期再试，直到次数封顶。
        for _ in 0..30 {
            now += GAP_BACKOFF_MAX_MS + 1;
            let _ = retry_unresolved_gaps(&journal, &fetch_for, now).unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), MAX_GAP_ATTEMPTS as i32, "group gap stops being retried at the cap");
        let gaps = journal.gaps().unwrap();
        assert_eq!(gaps[0].attempts, 1, "private gap is never retried: {gaps:?}");
        assert_eq!((gaps[0].status.as_str(), gaps[0].reason.as_str()), ("unrecovered", "private-history-unverified"));
        assert_eq!(gaps[1].status, "unknown");
        assert_eq!(journal.unresolved_gaps().unwrap().len(), 2, "capped gaps stay unresolved and visible");
        assert!(journal.retryable_gaps(now + GAP_BACKOFF_MAX_MS).unwrap().is_empty());
        let _ = group;
    }

    #[test]
    fn old_gap_table_without_retry_columns_is_migrated() {
        let dir = TempDb::new("migrate");
        {
            let conn = Connection::open(dir.db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE inbound_gaps (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL, from_ms INTEGER NOT NULL,
                    to_ms INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL, detail TEXT NOT NULL,
                    recovered INTEGER NOT NULL DEFAULT 0, opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
                 INSERT INTO inbound_gaps (conversation, from_ms, to_ms, status, reason, detail, opened_ms, updated_ms)
                 VALUES ('group:1', 0, 5, 'unknown', 'x', 'y', 1, 1);",
            )
            .unwrap();
        }
        let journal = Journal::open_at(&dir.db()).unwrap();
        let gaps = journal.gaps().unwrap();
        assert_eq!((gaps.len(), gaps[0].attempts, gaps[0].next_retry_ms), (1, 0, 0));
        assert_eq!(journal.retryable_gaps(10).unwrap().len(), 1);
    }

    #[test]
    fn a_gap_is_listed_for_notice_until_it_is_marked_and_then_never_again() {
        let dir = TempDb::new("notice");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let open = journal.open_gap("group:3", 10, 90, 90).unwrap();
        let private = journal.open_gap("private:7", 10, 90, 90).unwrap();
        let recovered = journal.open_gap("group:4", 10, 90, 90).unwrap();
        assert!(journal.unnotified_gaps().unwrap().is_empty(), "a gap with no result yet is undecided, not reportable");
        journal.set_gap_result(private, GapStatus::Unrecovered, "private-history-unverified", "d", 0, 100).unwrap();
        journal.set_gap_result(recovered, GapStatus::Recovered, "covered", "", 2, 100).unwrap();
        journal.set_gap_result(open, GapStatus::Unknown, "history-empty", "d", 0, 100).unwrap();
        let listed: Vec<(String, i64, i64)> =
            journal.unnotified_gaps().unwrap().into_iter().map(|g| (g.conversation, g.from_ms, g.to_ms)).collect();
        assert_eq!(listed, vec![("group:3".to_string(), 10, 90), ("private:7".to_string(), 10, 90), ("group:4".to_string(), 10, 90)]);

        journal.mark_gaps_notified(&[open, private, recovered], 120).unwrap();
        assert!(journal.unnotified_gaps().unwrap().is_empty(), "a notified gap is not listed again");
        // A later retry that changes the outcome does not make the same gap new.
        journal.set_gap_result(open, GapStatus::Unrecovered, "history-unsupported", "d", 0, 200).unwrap();
        assert!(journal.unnotified_gaps().unwrap().is_empty());
        // A gap opened by the next reconnect is a different gap.
        let next = journal.open_gap("private:7", 90, 400, 400).unwrap();
        journal.set_gap_result(next, GapStatus::Unrecovered, "private-history-unverified", "d", 0, 400).unwrap();
        let listed = journal.unnotified_gaps().unwrap();
        assert_eq!(listed.iter().map(|g| g.id).collect::<Vec<_>>(), vec![next]);
        // Still unresolved and unclaimed in the admin view, whether notified or not.
        let view = journal.admin_view().unwrap();
        assert_eq!(view["unresolvedGapCount"], json!(3));
        let unresolved = view["unresolvedGaps"].as_array().unwrap();
        assert_eq!(unresolved.len(), 3);
        assert!(unresolved.iter().all(|g| g["status"] != json!("recovered")));
        let shown = unresolved.iter().find(|g| g["id"] == json!(next)).unwrap();
        assert_eq!((shown["conversation"].clone(), shown["fromMs"].clone(), shown["toMs"].clone()), (json!("private:7"), json!(90), json!(400)));
        assert_eq!(shown["notifiedMs"], json!(0));
        assert_eq!(unresolved.iter().find(|g| g["id"] == json!(private)).unwrap()["notifiedMs"], json!(120));
    }

    #[test]
    fn gap_table_with_retry_columns_but_no_notice_column_is_migrated() {
        let dir = TempDb::new("migrate-notified");
        {
            let conn = Connection::open(dir.db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE inbound_gaps (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL, from_ms INTEGER NOT NULL,
                    to_ms INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL, detail TEXT NOT NULL,
                    recovered INTEGER NOT NULL DEFAULT 0, opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0, next_retry_ms INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO inbound_gaps (conversation, from_ms, to_ms, status, reason, detail, opened_ms, updated_ms, attempts)
                 VALUES ('private:1', 0, 5, 'unrecovered', 'private-history-unverified', 'y', 1, 1, 3);",
            )
            .unwrap();
        }
        let journal = Journal::open_at(&dir.db()).unwrap();
        let gaps = journal.gaps().unwrap();
        assert_eq!((gaps.len(), gaps[0].attempts, gaps[0].notified_ms), (1, 3, 0));
        assert!(journal.unnotified_gaps().unwrap().is_empty(), "legacy windows migrate silently");
        assert_eq!(gaps[0].notice_state, "unknown");
    }

    #[test]
    fn gap_table_without_retry_and_notice_columns_gains_them() {
        let dir = TempDb::new("migrate-all");
        {
            let conn = Connection::open(dir.db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE inbound_gaps (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL, from_ms INTEGER NOT NULL,
                    to_ms INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL, detail TEXT NOT NULL,
                    recovered INTEGER NOT NULL DEFAULT 0, opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
                 INSERT INTO inbound_gaps (conversation, from_ms, to_ms, status, reason, detail, opened_ms, updated_ms)
                 VALUES ('private:1', 0, 5, 'unrecovered', 'private-history-unverified', 'y', 1, 1);",
            )
            .unwrap();
        }
        let journal = Journal::open_at(&dir.db()).unwrap();
        let gaps = journal.gaps().unwrap();
        assert_eq!((gaps.len(), gaps[0].attempts, gaps[0].next_retry_ms, gaps[0].notified_ms), (1, 0, 0, 0));
        assert!(journal.unnotified_gaps().unwrap().is_empty(), "legacy windows migrate silently");
        assert_eq!(gaps[0].notice_state, "unknown");
        journal.mark_gaps_notified(&[gaps[0].id], 9).unwrap();
        assert!(journal.unnotified_gaps().unwrap().is_empty());
        drop(journal);
        Journal::open_at(&dir.db()).expect("a second open of the migrated table changes nothing");
    }

    #[test]
    fn nonzero_retcodes_are_classified_and_never_claim_recovery() {
        assert!(matches!(classify_retcode(1404), FetchError::Rejected(_)));
        assert!(matches!(classify_retcode(1400), FetchError::Rejected(_)));
        assert!(matches!(classify_retcode(1200), FetchError::Transient(_)));
        assert!(matches!(classify_retcode(33000), FetchError::Transient(_)));
        assert!(matches!(classify_retcode(98765), FetchError::Unclear(_)));
        let dir = TempDb::new("retcode");
        let journal = Journal::open_at(&dir.db()).unwrap();
        for (error, status, reason) in [
            (classify_retcode(1404), GapStatus::Unrecovered, "history-unsupported"),
            (classify_retcode(1200), GapStatus::Unknown, "history-transient-retcode"),
            (classify_retcode(98765), GapStatus::Unknown, "history-retcode-unclear"),
        ] {
            let mut fetch = |_: Option<i64>| -> Result<HistoryPage, FetchError> { Err(error.clone()) };
            let outcome = backfill_window(&journal, "group:1", 0, 10, 10, &mut fetch).unwrap();
            assert_eq!((outcome.status, outcome.reason.as_str()), (status, reason));
        }
    }

    #[test]
    fn manual_retry_resets_capped_gaps_without_changing_their_status() {
        let dir = TempDb::new("manual-retry");
        let journal = Journal::open_at(&dir.db()).unwrap();
        let capped = journal.open_gap("group:1", 0, 10, 1).unwrap();
        for _ in 0..MAX_GAP_ATTEMPTS {
            journal.set_gap_result(capped, GapStatus::Unknown, "history-unreachable", "down", 0, 1).unwrap();
        }
        let done = journal.open_gap("group:1", 0, 10, 1).unwrap();
        journal.set_gap_result(done, GapStatus::Recovered, "covered", "", 0, 1).unwrap();
        let other = journal.open_gap("group:2", 0, 10, 1).unwrap();
        journal.set_gap_result(other, GapStatus::Unknown, "x", "y", 0, 1).unwrap();
        assert!(journal.retryable_gaps(i64::MAX / 2).unwrap().iter().all(|g| g.id == other || g.id != capped));
        assert_eq!(journal.reset_gap_retries("group:1").unwrap(), 1, "recovered gaps are not touched");
        let ids: Vec<i64> = journal.retryable_gaps(2).unwrap().iter().map(|g| g.id).collect();
        assert!(ids.contains(&capped) && !ids.contains(&other), "{ids:?}");
        let gap = journal.gaps().unwrap().into_iter().find(|g| g.id == capped).unwrap();
        assert_eq!((gap.status.as_str(), gap.attempts, gap.next_retry_ms), ("unknown", 0, 0));
    }
}
