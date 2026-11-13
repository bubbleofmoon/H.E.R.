//! Where the dependency/recovery run record lives and how the bridge writes and serves it (ticket 25, AC1).
//!
//! `run_record.rs` owns the format. This module binds one writer per selected data directory
//! (`<data>/run-record.jsonl`, appended through the runtime's contained file gate, so the live bridge and the console
//! process can both write it), gives call sites a one-line best-effort `record`, and serves the read-only page that the
//! console route and ticket 27 render. A no-root context has no run record (nothing is written outside a
//! selected root). A failed write is printed, never swallowed silently and never fails the action it describes.

use crate::dependency::SystemClock;
use crate::run_record::{
    read_page, Component, FileIo, Outcome, Page, Query, RecordInput, RecordIo, RecordLimits, RunRecordWriter, Trigger,
    FILE_NAME,
};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// The most the reader looks at: the compaction cap of the file.
const READ_BYTES: usize = 600 * 1024;

/// Storage through the selected runtime's file gate.
struct ContextIo {
    context: RuntimeContext,
}

impl ContextIo {
    fn path(&self) -> io::Result<PathBuf> {
        self.context
            .resolve_path(RuntimeArea::Data, FILE_NAME, PathIntent::ReadOptionalFile)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
    }
}

impl RecordIo for ContextIo {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        self.context
            .append_bytes(RuntimeArea::Data, FILE_NAME, line)
            .map_err(io::Error::other)
    }
    fn read_tail(&self, max_bytes: usize) -> io::Result<Vec<u8>> {
        FileIo::new(self.path()?).read_tail(max_bytes)
    }
    fn len(&self) -> io::Result<u64> {
        FileIo::new(self.path()?).len()
    }
    fn replace(&self, content: &[u8]) -> io::Result<()> {
        self.context
            .write_atomic(RuntimeArea::Data, FILE_NAME, content)
            .map_err(io::Error::other)
    }
}

/// A context that has a run record: a selected (isolated) root.
pub fn enabled(context: &RuntimeContext) -> bool {
    context.is_isolated() && context.runtime_root().is_some()
}

fn writers() -> &'static Mutex<Vec<(PathBuf, Arc<RunRecordWriter>)>> {
    static W: OnceLock<Mutex<Vec<(PathBuf, Arc<RunRecordWriter>)>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(Vec::new()))
}

fn default_context() -> &'static Mutex<Option<RuntimeContext>> {
    static D: OnceLock<Mutex<Option<RuntimeContext>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(None))
}

/// The writer of this context's record (created on first use), `None` when the context has none.
pub fn writer(context: &RuntimeContext) -> Option<Arc<RunRecordWriter>> {
    if !enabled(context) {
        return None;
    }
    let root = context.runtime_root()?.to_path_buf();
    let mut all = writers().lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, w)) = all.iter().find(|(r, _)| *r == root) {
        return Some(w.clone());
    }
    let redact_context = context.clone();
    let w = Arc::new(RunRecordWriter::new(
        Box::new(ContextIo { context: context.clone() }),
        Box::new(SystemClock),
        Box::new(move |text| crate::activity::redact(&redact_context, text)),
        RecordLimits::default(),
    ));
    all.push((root, w.clone()));
    Some(w)
}

/// Remember the process's selected runtime so call sites without a context at hand (a low-level RPC call, a panic
/// hook) can still record. The first bind wins for the process; a later one replaces it only when it differs.
pub fn bind_default(context: &RuntimeContext) {
    *default_context().lock().unwrap_or_else(|p| p.into_inner()) = Some(context.clone());
}

/// Record one event for `context`. Best effort: a missing record (legacy context) is a no-op, a failed write is printed.
pub fn record(context: &RuntimeContext, trigger: Trigger, component: Component, event: &str, outcome: Outcome, reason: &str) {
    let Some(w) = writer(context) else { return };
    if let Err(error) = w.record(&RecordInput { run_id: None, trigger, component, event, outcome, reason }) {
        eprintln!("[run-record] 写运行记录失败：{error}");
    }
}

/// `record` for the bound default context.
pub fn record_default(trigger: Trigger, component: Component, event: &str, outcome: Outcome, reason: &str) {
    let context = default_context().lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(context) = context {
        record(&context, trigger, component, event, outcome, reason);
    }
}

/// Publish the pending `repeat` summaries (periodic tick, shutdown).
pub fn flush(context: &RuntimeContext) {
    if let Some(w) = writer(context) {
        if let Err(error) = w.flush() {
            eprintln!("[run-record] 写运行记录失败：{error}");
        }
    }
}

/// One page of the record, newest first (bounded read). A context without a record has an empty one.
pub fn page(context: &RuntimeContext, query: &Query) -> Result<Page, String> {
    if !enabled(context) {
        return Ok(Page { records: Vec::new(), next_before: None, skipped: 0 });
    }
    // The writer's collapse counters are only in this process; publish them so a reader sees the whole story.
    flush(context);
    read_page(&ContextIo { context: context.clone() }, query, READ_BYTES).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-recovery-log-{tag}-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn a_selected_root_gets_a_redacted_record_under_its_data_dir_and_a_page_reads_it_back() {
        let root = root("page");
        let context = RuntimeContext::from_root(&root).unwrap();
        record(
            &context,
            Trigger::Startup,
            Component::Dsh,
            "start-check",
            Outcome::Failed,
            "connect refused for token=SECRET-VALUE-1234 at C:\\secret\\place\\dsh.yaml",
        );
        record(&context, Trigger::Recovery, Component::Dsh, "recovered", Outcome::Ok, "settings/describe answered");
        let page = page(&context, &Query::default()).unwrap();
        let file = std::fs::read_to_string(root.join("data/run-record.jsonl")).unwrap();
        assert_eq!(page.records.len(), 2, "{file}");
        assert_eq!(page.records[0].event, "recovered", "newest first");
        assert!(!file.contains("SECRET-VALUE-1234") && !file.contains("secret\\\\place"), "credential and path must not be stored: {file}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_legacy_context_has_no_record_and_writes_nothing() {
        let legacy = RuntimeContext::legacy();
        assert!(!enabled(&legacy));
        record(&legacy, Trigger::Startup, Component::Process, "x", Outcome::Ok, "x");
        assert!(page(&legacy, &Query::default()).unwrap().records.is_empty());
    }
}
