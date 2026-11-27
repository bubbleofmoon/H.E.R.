//! Token ledger JSONL file storage, locking, compaction I/O, reset, and reconciliation
//! watermark persistence (`LedgerStorage`), separated from pure ledger pricing/attribution/
//! compaction math in `ledger.rs`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{compact_jsonl, now_ms, order_merged, plan_appends, read_rows, Ledger};
use crate::pricing::PriceTable;
use crate::runtime::{
    append_io, lock_append_file, runtime_io_error, write_append_all, PathIntent, RuntimeArea,
    RuntimeContext, APPEND_CONFLICT_WAIT,
};

pub const DEFAULT_MAX_LINES: usize = 20_000;
const DEFAULT_KEEP_SAMPLES: usize = 10_000;

pub(super) const RUST_LEDGER_NAME: &str = "token-usage-rust.jsonl";
pub(super) const JS_LEDGER_NAME: &str = "token-usage.jsonl";
const COMPACT_SIDECAR: &str = "token-usage-rust.jsonl.compact";
const RECONCILE_FILE: &str = "harness-usage-reconcile.json";
/// Leaf lock protecting `RECONCILE_FILE` read-modify-write only. Lock order: `RECONCILE_LOCK` and
/// ledger file locks (`JS_LEDGER_NAME`, `RUST_LEDGER_NAME`) are strictly disjoint leaf gates;
/// neither is ever acquired while holding the other or while opening `conversations.db`.
const RECONCILE_LOCK: &str = "locks/harness-reconcile.lock";

/// I/O adapter for token ledger JSONL persistence, locking, compaction, and reset.
pub struct LedgerStorage;

impl LedgerStorage {
    /// Append one usage record to the selected runtime's Rust ledger.
    pub fn append_record_for_runtime(
        context: &RuntimeContext,
        record: &Value,
    ) -> Result<(), String> {
        let mut line =
            serde_json::to_vec(record).map_err(|e| format!("序列化账本记录失败：{e}"))?;
        line.push(b'\n');
        context.append_bytes(RuntimeArea::Data, RUST_LEDGER_NAME, &line)
    }

    /// Append one usage record to an explicit ledger path.
    pub fn append_record_to(path: &str, record: &Value) -> Result<(), String> {
        append_record_at(Path::new(path), record, false, None)
    }

    /// Append and sync one usage record under lock (guarded files).
    pub(crate) fn append_record_to_durable(path: &str, record: &Value) -> Result<(), String> {
        append_record_at(Path::new(path), record, true, None)
    }

    /// Read the merged runtime ledger, filter `candidates` via pure `plan_appends`, and append
    /// only new records.
    pub fn append_planned(context: &RuntimeContext, candidates: &[Value]) -> Result<usize, String> {
        let existing = Self::load_all_for_runtime(context)?;
        let fresh = plan_appends(&existing, candidates);
        for row in &fresh {
            Self::append_record_for_runtime(context, row)?;
        }
        Ok(fresh.len())
    }

    /// Load a single file into a `Ledger` (test helper).
    #[cfg(test)]
    pub fn load_single(path: &str) -> Ledger {
        let mut ledger = Ledger::empty(path, None);
        let rows = read_rows(&[path]);
        ledger.ingest(rows);
        ledger
    }

    /// Load and merge explicit ledger file paths.
    pub fn load_paths(paths: &[&str]) -> Ledger {
        let mut ledger = Ledger::empty(
            paths.first().copied().unwrap_or(JS_LEDGER_NAME),
            None,
        );
        let mut rows = read_rows(paths);
        order_merged(&mut rows);
        ledger.ingest(rows);
        ledger
    }

    /// Load the selected runtime's ledgers (falling back to any crash-interrupted compaction
    /// sidecar when the Rust ledger was truncated before rewrite completed) and session map.
    pub fn load_all_for_runtime(context: &RuntimeContext) -> Result<Ledger, String> {
        let js_text = context
            .read_optional_text(RuntimeArea::Data, JS_LEDGER_NAME)
            .map_err(|error| error.to_string())?;
        let mut rust_text = context
            .read_optional_text(RuntimeArea::Data, RUST_LEDGER_NAME)
            .map_err(|error| error.to_string())?;
        if rust_text.as_deref().is_none_or(str::is_empty) {
            if let Some(sidecar) = context
                .read_optional_text(RuntimeArea::Data, COMPACT_SIDECAR)
                .map_err(|error| error.to_string())?
                .filter(|s| !s.is_empty())
            {
                rust_text = Some(sidecar);
            }
        }
        let sessions_json = context
            .read_optional_text(RuntimeArea::Data, "sessions.json")
            .map_err(|error| error.to_string())?;
        Ok(Ledger::load_all_from_sources(
            js_text.as_deref(),
            rust_text.as_deref(),
            sessions_json.as_deref(),
        ))
    }

    /// Compact the isolated runtime's Rust token ledger with default thresholds (`20_000` ->
    /// `10_000` samples).
    pub fn compact_if_needed(
        context: &RuntimeContext,
        table: &PriceTable,
    ) -> Result<bool, String> {
        Self::compact_for_runtime(context, table, DEFAULT_MAX_LINES, DEFAULT_KEEP_SAMPLES)
    }

    /// Compact the selected runtime's `token-usage-rust.jsonl` when it has at least `max_lines`
    /// non-empty lines, keeping the newest `keep_samples` samples and folding older live samples
    /// into per-session `"evict"` summaries. Crash-safe via locked in-place rewrite backed by a
    /// staged `.compact` sidecar.
    pub fn compact_for_runtime(
        context: &RuntimeContext,
        table: &PriceTable,
        max_lines: usize,
        keep_samples: usize,
    ) -> Result<bool, String> {
        let rust_len = context
            .optional_file_len(RuntimeArea::Data, RUST_LEDGER_NAME)
            .map_err(|e| e.to_string())?;
        let sidecar_len = context
            .optional_file_len(RuntimeArea::Data, COMPACT_SIDECAR)
            .map_err(|e| e.to_string())?;
        if rust_len == 0 && sidecar_len == 0 {
            return Ok(false);
        }
        let sessions_json = context
            .read_optional_text(RuntimeArea::Data, "sessions.json")
            .map_err(|e| e.to_string())?;
        let evicted_attempt_ids = {
            let mut file = open_locked_file_retry(
                context,
                RuntimeArea::Data,
                RUST_LEDGER_NAME,
                Instant::now() + APPEND_CONFLICT_WAIT,
            )?;
            let mut raw = String::new();
            file.read_to_string(&mut raw)
                .map_err(|e| format!("读取账本失败：{}", runtime_io_error("compact/read", &e)))?;
            if raw.is_empty() {
                if let Some(sidecar) = context
                    .read_optional_text(RuntimeArea::Data, COMPACT_SIDECAR)
                    .map_err(|e| e.to_string())?
                    .filter(|s| !s.is_empty())
                {
                    write_locked_handle(&mut file, sidecar.as_bytes())?;
                    raw = sidecar;
                }
            }
            let line_count = raw.split('\n').filter(|l| !l.is_empty()).count();
            if line_count < max_lines {
                remove_sidecar(context);
                return Ok(false);
            }
            let plan = compact_jsonl(
                &raw,
                sessions_json.as_deref(),
                table,
                keep_samples,
                now_ms(),
            );
            if !plan.compacted {
                remove_sidecar(context);
                return Ok(false);
            }
            context.write_atomic(RuntimeArea::Data, COMPACT_SIDECAR, plan.jsonl.as_bytes())?;
            write_locked_handle(&mut file, plan.jsonl.as_bytes())?;
            remove_sidecar(context);
            plan.evicted_attempt_ids
        };
        // Ledger lock is released before querying `conversations.db` or taking `RECONCILE_LOCK`.
        if !evicted_attempt_ids.is_empty() {
            let max_seq = crate::harness::store::max_attempt_event_seq(
                context,
                Some(&evicted_attempt_ids),
            )?;
            if max_seq > 0 {
                Self::advance_reconcile_seq(context, max_seq)?;
            }
        }
        Ok(true)
    }

    /// Compact an explicit ledger file under an OS file lock (test helper).
    #[cfg(test)]
    pub(crate) fn compact_file(
        path: &Path,
        table: &PriceTable,
        max_lines: usize,
        keep_samples: usize,
    ) -> Result<bool, String> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("打开账本失败：{}", runtime_io_error("compact/open", &e)))?;
        lock_append_file(&file, None)
            .map_err(|e| format!("锁账本失败：{}", runtime_io_error("compact/lock", &e)))?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|e| format!("读取账本失败：{}", runtime_io_error("compact/read", &e)))?;
        if raw.split('\n').filter(|l| !l.is_empty()).count() < max_lines {
            return Ok(false);
        }
        let plan = compact_jsonl(&raw, None, table, keep_samples, now_ms());
        if !plan.compacted {
            return Ok(false);
        }
        write_locked_handle(&mut file, plan.jsonl.as_bytes())?;
        Ok(true)
    }

    /// Reset both JS and Rust token ledgers (`POST /api/tokens/reset`), clearing any compaction
    /// sidecar and advancing the reconciliation watermark so previously completed attempts are not
    /// re-added at next startup.
    pub fn reset(context: &RuntimeContext) -> Result<(), String> {
        let err = || "重置失败：所选用量账本不可用".to_string();
        let js_path = context
            .resolve_path(RuntimeArea::Data, JS_LEDGER_NAME, PathIntent::WriteFile)
            .map_err(|_| err())?;
        let rust_path = context
            .resolve_path(RuntimeArea::Data, RUST_LEDGER_NAME, PathIntent::WriteFile)
            .map_err(|_| err())?;
        let exists = |path: &Path| match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(err()),
        };
        let js_exists = exists(&js_path)?;
        let rust_exists = exists(&rust_path)?;
        {
            let (js_ledger, rust_ledger) = if !js_exists && rust_exists {
                let rust = context
                    .open_locked_file(RuntimeArea::Data, RUST_LEDGER_NAME)
                    .map_err(|_| err())?;
                let js = context
                    .open_locked_file(RuntimeArea::Data, JS_LEDGER_NAME)
                    .map_err(|_| err())?;
                (js, rust)
            } else {
                let js = context
                    .open_locked_file(RuntimeArea::Data, JS_LEDGER_NAME)
                    .map_err(|_| err())?;
                let rust = context
                    .open_locked_file(RuntimeArea::Data, RUST_LEDGER_NAME)
                    .map_err(|_| err())?;
                (js, rust)
            };
            js_ledger
                .set_len(0)
                .and_then(|_| js_ledger.sync_all())
                .map_err(|_| err())?;
            rust_ledger
                .set_len(0)
                .and_then(|_| rust_ledger.sync_all())
                .map_err(|_| err())?;
            remove_sidecar(context);
        }
        // Release ledger locks before querying `conversations.db` or taking `RECONCILE_LOCK`.
        if let Ok(max_seq) = crate::harness::store::max_attempt_event_seq(context, None) {
            if max_seq > 0 {
                let _ = Self::advance_reconcile_seq(context, max_seq);
            }
        }
        Ok(())
    }

    /// Read the highest `model_attempt_ended` sequence watermark recorded as reconciled/evicted.
    pub(crate) fn read_reconcile_seq(context: &RuntimeContext) -> Result<i64, String> {
        let Some(text) = context
            .read_optional_text(RuntimeArea::Data, RECONCILE_FILE)
            .map_err(|e| e.to_string())?
        else {
            return Ok(0);
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| "harness usage reconciliation state is corrupt".to_string())?;
        Ok(value["lastSeq"].as_i64().unwrap_or(0))
    }

    /// Monotonically raise the reconciliation sequence watermark to at least `seq`.
    pub(crate) fn advance_reconcile_seq(context: &RuntimeContext, seq: i64) -> Result<(), String> {
        if seq <= 0 {
            return Ok(());
        }
        let _lock = open_locked_file_retry(
            context,
            RuntimeArea::Data,
            RECONCILE_LOCK,
            Instant::now() + APPEND_CONFLICT_WAIT,
        )?;
        let current = Self::read_reconcile_seq(context).unwrap_or(0);
        if seq <= current {
            return Ok(());
        }
        let bytes = json!({"v": 1, "lastSeq": seq}).to_string();
        context.write_atomic(RuntimeArea::Data, RECONCILE_FILE, bytes.as_bytes())
    }
}

fn write_locked_handle(file: &mut File, bytes: &[u8]) -> Result<(), String> {
    file.set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)))
        .and_then(|_| file.write_all(bytes))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("写账本失败：{}", runtime_io_error("compact/write", &e)))
}

fn remove_sidecar(context: &RuntimeContext) {
    if let Ok(path) =
        context.resolve_path(RuntimeArea::Data, COMPACT_SIDECAR, PathIntent::WriteFile)
    {
        let _ = std::fs::remove_file(path);
    }
}

fn open_locked_file_retry(
    context: &RuntimeContext,
    area: RuntimeArea,
    relative: &str,
    deadline: Instant,
) -> Result<File, String> {
    loop {
        match context.open_locked_file(area, relative) {
            Ok(file) => return Ok(file),
            Err(error)
                if error == "runtime file is already locked" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn append_record_at(
    path: &Path,
    record: &Value,
    durable: bool,
    deadline: Option<Instant>,
) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建账本目录失败：{e}"))?;
        }
    }
    let mut line =
        serde_json::to_string(record).map_err(|e| format!("序列化账本记录失败：{e}"))?;
    line.push('\n');
    let mut file = append_io(deadline, || {
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
    })
    .map_err(|e| format!("打开账本失败：{}", runtime_io_error("append/open", &e)))?;
    // The check and the write must not interleave with another appender.
    lock_append_file(&file, deadline)
        .map_err(|e| format!("锁账本失败：{}", runtime_io_error("append/lock", &e)))?;
    if append_io(deadline, || crate::runtime::ends_mid_line(&mut file))
        .map_err(|e| format!("检查账本失败：{}", runtime_io_error("append/tail", &e)))?
    {
        line.insert(0, '\n');
    }
    write_append_all(&mut file, line.as_bytes(), deadline)
        .map_err(|e| format!("写账本失败：{}", runtime_io_error("append/write", &e)))?;
    append_io(deadline, || file.flush())
        .map_err(|e| format!("刷新账本失败：{}", runtime_io_error("append/flush", &e)))?;
    if durable {
        #[cfg(test)]
        crate::runtime::append_test_hook("sync", line.as_bytes())?;
        append_io(deadline, || file.sync_all())
            .map_err(|e| format!("同步账本失败：{}", runtime_io_error("append/sync", &e)))?;
    }
    Ok(())
}
