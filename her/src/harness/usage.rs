//! Usage facts (#173): one token-ledger sample (`ledger.rs`, the selected runtime's `data/token-usage-rust.jsonl`) for
//! each model request through the route module that returned a response end. Harness turns record each attempt with
//! its route and its place in the turn (`harness.rs`, `ChainState::record_usage`); one-shot jobs record each request
//! (the digest, `digest/model.rs`). The console's token views read these samples with every other sample and price
//! them by model at read time; `/api/usage/attribution` leaves them out of its comparison with DSH's own total, which
//! never saw these requests (`Ledger::dsh_comparable_samples`).
#![allow(dead_code)]
// #173 step 2 wires `record` into harness.rs and the digest; remove both lines then.
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::RuntimeContext;
use genai::chat::Usage;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

/// The `engine` and the `provider` of every sample written here.
pub(crate) const ENGINE: &str = "harness";
/// The `job` of a harness turn's samples.
pub(crate) const TURN: &str = "turn";
/// The `job` of the digest's samples.
pub(crate) const DIGEST: &str = "digest";
/// The ledger key the digest's samples are filed under.
pub(crate) const DIGEST_KEY: &str = "internal:digest";

/// Where one request is filed in the token ledger.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Place {
    /// The conversation key, or a job's internal key (`DIGEST_KEY`).
    pub key: String,
    /// The turn number (`next_number`): one per harness turn, one per digest request.
    pub turn: i64,
    /// The request's step in its turn (0, 1, ...): every request of a turn, retries and failovers included, has its own.
    pub step: i64,
    /// Requests already sent for the same unchanged request (its retries and failovers); 0 for a fresh request.
    pub retry: i64,
}

/// The ledger session of `key` in this process: `<process id>/<key>`. The process id is minted once per process
/// (`turn_ledger::mint_id`), so the samples of two processes never share a session, and the turn numbers of one process
/// (`next_number`) never meet a number another process gave.
pub(crate) fn session(key: &str) -> String {
    unimplemented!("#173 red");
    static PROCESS: OnceLock<String> = OnceLock::new();
    format!("{}/{key}", PROCESS.get_or_init(|| crate::turn_ledger::mint_id("harness-")))
}

/// A turn number: the wall clock in milliseconds, strictly after every number this process gave before. The console
/// lists turns in time order, and no two turns of one process share a number, even within one millisecond, across
/// threads or after the clock is set back.
pub(crate) fn next_number() -> i64 {
    unimplemented!("#173 red");
    static LAST: AtomicI64 = AtomicI64::new(i64::MIN);
    let now = crate::parity_clock::now_ms();
    let mut last = LAST.load(Ordering::SeqCst);
    loop {
        let next = now.max(last.saturating_add(1));
        match LAST.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// A response's reported usage as ledger buckets (`pricing::normalize_usage` reads them back): cache hits are the
/// prompt's cached tokens, cache writes its cache-creation tokens, misses the rest of the prompt (never below zero),
/// output the completion tokens (reasoning included) and reasoning the completion's reasoning tokens (shown, never
/// priced). A count that is absent, zero or negative counts as zero. `None` when the response reported neither prompt
/// nor completion tokens.
pub(crate) fn buckets(usage: &Usage) -> Option<Value> {
    unimplemented!("#173 red");
    let positive = |count: Option<i32>| count.filter(|count| *count > 0).map(i64::from);
    let prompt = positive(usage.prompt_tokens);
    let output = positive(usage.completion_tokens);
    if prompt.is_none() && output.is_none() {
        return None;
    }
    let details = usage.prompt_tokens_details.as_ref();
    let hit = positive(details.and_then(|details| details.cached_tokens)).unwrap_or(0);
    let write = positive(details.and_then(|details| details.cache_creation_tokens)).unwrap_or(0);
    let miss = (prompt.unwrap_or(0) - hit - write).max(0);
    let reasoning = positive(
        usage
            .completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens),
    )
    .unwrap_or(0);
    Some(json!({
        "cacheMiss": miss,
        "cacheHit": hit,
        "cacheWrite": write,
        "output": output.unwrap_or(0),
        "reasoning": reasoning,
    }))
}

/// The ledger record of one request on route `route` (model `model`) of job `job`, at `at` ms. Its `seq` is 0, so the
/// ledger keeps the first record of a place and ignores a repeat (`Ledger::apply`). A response without reported prompt
/// or completion tokens is recorded as evidence without billing (`b: null`).
pub(crate) fn row(place: &Place, route: &str, model: &str, job: &str, usage: Option<&Usage>, at: i64) -> Value {
    unimplemented!("#173 red");
    json!({
        "k": "turn",
        "t": at,
        "sid": session(&place.key),
        "seq": 0,
        "turn": place.turn,
        "step": place.step,
        "retry": place.retry,
        "key": place.key,
        "model": model,
        "provider": ENGINE,
        "engine": ENGINE,
        "route": route,
        "job": job,
        "b": usage.and_then(buckets).unwrap_or(Value::Null),
    })
}

/// Records one usage fact (`row`) in the selected runtime's token ledger. A runtime that is not selected records
/// nothing; a failed write goes to the run record (component `model-chain`, event `usage-unrecorded`) and never fails
/// the request.
pub(crate) fn record(context: &RuntimeContext, place: &Place, route: &str, model: &str, job: &str, usage: Option<&Usage>) {
    unimplemented!("#173 red");
    if !context.is_isolated() {
        return;
    }
    let row = row(place, route, model, job, usage, crate::parity_clock::now_ms());
    if let Err(error) = crate::ledger::append_record_for_runtime(context, &row) {
        crate::recovery_log::record(
            context,
            Trigger::Turn,
            Component::ModelChain,
            "usage-unrecorded",
            Outcome::Failed,
            &format!("{}: route {route} step {}: a usage fact was not written ({error})", place.key, place.step),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    fn usage(value: Value) -> Usage {
        serde_json::from_value(value).unwrap()
    }

    fn place(key: &str, turn: i64, step: i64, retry: i64) -> Place {
        Place {
            key: key.into(),
            turn,
            step,
            retry,
        }
    }

    /// The ledger the console reads from these rows, as the Rust ledger file holds them.
    fn ledger(rows: &[Value]) -> Ledger {
        let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        Ledger::load_all_from_sources(None, Some(text.as_str()), None)
    }

    struct Fixture {
        root: std::path::PathBuf,
        context: RuntimeContext,
        cfg: Value,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        /// A selected runtime whose model relay listens on `model_port`.
        fn new(tag: &str, harness: Value, model_port: u16) -> Self {
            let root = crate::install_root::development_root(
                std::env::temp_dir().join(format!("her-173-{tag}-{}", crate::turn_ledger::mint_id(""))),
            );
            for dir in ["config", "data", "resources"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            let cfg = json!({"ownerQQ": 8808, "consoleToken": "synthetic-console-token-173", "harness": harness});
            std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-173").unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(
                root.join("dependencies.json"),
                json!({"dsh": "http://127.0.0.1:1", "model": format!("http://127.0.0.1:{model_port}"), "onebot": "http://127.0.0.1:1"})
                    .to_string(),
            )
            .unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            Self { root, context, cfg }
        }

        /// The rows of the selected runtime's Rust ledger, in order.
        fn rows(&self) -> Vec<Value> {
            std::fs::read_to_string(self.root.join("data/token-usage-rust.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn harness_usage_buckets_split_the_prompt_into_hits_writes_and_misses() {
        let full = usage(json!({"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
            "prompt_tokens_details": {"cached_tokens": 30, "cache_creation_tokens": 10},
            "completion_tokens_details": {"reasoning_tokens": 5}}));
        assert_eq!(
            buckets(&full),
            Some(json!({"cacheMiss": 60, "cacheHit": 30, "cacheWrite": 10, "output": 20, "reasoning": 5}))
        );
        let plain = usage(json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}));
        assert_eq!(
            buckets(&plain),
            Some(json!({"cacheMiss": 12, "cacheHit": 0, "cacheWrite": 0, "output": 3, "reasoning": 0}))
        );
        let over = usage(json!({"prompt_tokens": 10, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 40}}));
        assert_eq!(buckets(&over).unwrap()["cacheMiss"], 0, "more cached than prompt tokens never makes a negative miss");
        let output_only = usage(json!({"completion_tokens": 7}));
        assert_eq!(
            buckets(&output_only),
            Some(json!({"cacheMiss": 0, "cacheHit": 0, "cacheWrite": 0, "output": 7, "reasoning": 0}))
        );
        let negative = usage(json!({"prompt_tokens": -5, "completion_tokens": 4}));
        assert_eq!(buckets(&negative).unwrap()["cacheMiss"], 0, "a negative count counts as zero");
        assert_eq!(buckets(&usage(json!({"total_tokens": 15}))), None, "a total alone is no bucket");
        assert_eq!(buckets(&usage(json!({"prompt_tokens": 0, "completion_tokens": 0}))), None);
        assert_eq!(buckets(&Usage::default()), None);
        let huge = usage(json!({"prompt_tokens": i32::MAX, "completion_tokens": i32::MAX}));
        assert_eq!(buckets(&huge).unwrap()["output"], i64::from(i32::MAX), "the largest count stays exact");
    }

    #[test]
    fn harness_usage_rows_read_back_as_ledger_samples_once_per_place() {
        let reported = usage(json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}));
        let first = row(&place("group:1", 1000, 0, 0), "a", "model-a", TURN, Some(&reported), 2000);
        let repeat = row(&place("group:1", 1000, 0, 0), "a", "model-a", TURN, Some(&reported), 2001);
        let retry = row(&place("group:1", 1000, 1, 1), "b", "model-b", TURN, Some(&reported), 2002);
        for field in ["k", "sid", "seq", "turn", "step", "retry", "key", "model", "provider", "engine", "route", "job", "b"] {
            assert!(first.get(field).is_some(), "{field}: {first}");
        }
        assert_eq!((first["seq"].as_i64(), first["engine"].as_str(), first["provider"].as_str()), (Some(0), Some("harness"), Some("harness")));
        let dsh = json!({"k": "turn", "t": 2003, "sid": "dsh-session-1", "seq": 4, "turn": 1, "step": 0, "retry": 0,
            "key": "group:1", "model": "model-d", "provider": "relay", "b": {"cacheMiss": 5, "output": 1}});
        let ledger = ledger(&[first, repeat, retry, dsh]);
        assert_eq!(ledger.samples.len(), 3, "a repeated place is ignored, as a replayed DSH sample is");
        let ours: Vec<&str> = ledger.dsh_comparable_samples().map(|sample| sample.session_id.as_str()).collect();
        assert_eq!(ours, ["dsh-session-1"], "only DSH's own samples are compared with its total");
        let sample = ledger.samples.values().find(|sample| sample.step == 1).unwrap();
        assert_eq!((sample.engine.as_str(), sample.model.as_str(), sample.retry), ("harness", "model-b", 1));
        let summary = ledger.summary(&crate::pricing::resolve(None), 50);
        assert_eq!(summary["conversations"].as_array().unwrap().len(), 1, "harness and DSH usage of one conversation share its row");
        assert_eq!(summary["totals"]["steps"], 3);
        assert_eq!(summary["totals"]["turns"], 2, "one harness turn and one DSH turn");
        assert_eq!(summary["totals"]["totalTokens"], 36.0);
    }

    #[test]
    fn harness_usage_a_response_without_reported_tokens_is_evidence_without_billing() {
        let unreported = row(&place("group:1", 1000, 0, 0), "a", "model-a", TURN, None, 2000);
        assert_eq!(unreported["b"], Value::Null);
        let total_only = row(&place("group:1", 1000, 1, 0), "a", "model-a", TURN, Some(&usage(json!({"total_tokens": 9}))), 2001);
        assert_eq!(total_only["b"], Value::Null);
        let ledger = ledger(&[unreported, total_only]);
        assert_eq!((ledger.samples.len(), ledger.journal_lines, ledger.dropped_lines), (0, 2, 0));
    }

    #[test]
    fn harness_usage_turn_numbers_strictly_increase_within_the_process() {
        let before = crate::parity_clock::now_ms();
        let mut numbers: Vec<i64> = (0..1000).map(|_| next_number()).collect();
        assert!(numbers[0] >= before, "a number is never before the wall clock");
        assert!(numbers.windows(2).all(|pair| pair[0] < pair[1]), "a thousand numbers in one millisecond stay distinct and ordered");
        let threads: Vec<_> = (0..4).map(|_| std::thread::spawn(|| (0..500).map(|_| next_number()).collect::<Vec<i64>>())).collect();
        for thread in threads {
            numbers.extend(thread.join().unwrap());
        }
        let count = numbers.len();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), count, "concurrent turns never share a number");
    }

    #[test]
    fn harness_usage_each_process_files_a_key_under_its_own_session() {
        let group = session("group:1");
        assert_eq!(group, session("group:1"), "one session per key in a process");
        assert_ne!(group, session("group:2"));
        assert!(group.starts_with("harness-") && group.ends_with("/group:1"), "{group}");
        // Another process (a restart) mints another id: its numbers may repeat without meeting these.
        let other = format!("harness-other/{}", "group:1");
        let mut earlier = row(&place("group:1", 1000, 0, 0), "a", "model-a", TURN, Some(&usage(json!({"completion_tokens": 3}))), 2000);
        earlier["sid"] = json!(other);
        let later = row(&place("group:1", 1000, 0, 0), "a", "model-a", TURN, Some(&usage(json!({"completion_tokens": 3}))), 2001);
        let ledger = ledger(&[earlier, later]);
        assert_eq!(ledger.samples.len(), 2, "two processes never overwrite each other's samples");
        let summary = ledger.summary(&crate::pricing::resolve(None), 50);
        assert_eq!(summary["conversations"].as_array().unwrap().len(), 1, "both stay one conversation");
        assert_eq!(summary["conversations"][0]["turns"], 2);
    }

    #[test]
    fn harness_usage_many_sessions_of_one_conversation_stay_one_conversation_row() {
        let rows: Vec<Value> = (0..3000)
            .map(|n| {
                let mut row = row(&place("group:1", 1000 + n, 0, 0), "a", "model-a", TURN, Some(&usage(json!({"completion_tokens": 1}))), 2000 + n);
                row["sid"] = json!(format!("harness-process-{n}/group:1"));
                row
            })
            .collect();
        let ledger = ledger(&rows);
        let ids = ledger.all_session_ids();
        assert_eq!(ids.len(), 3000);
        assert_eq!((ids[0].as_str(), ids[2999].as_str()), ("harness-process-0/group:1", "harness-process-2999/group:1"), "ledger order is kept");
        let summary = ledger.summary(&crate::pricing::resolve(None), 50);
        assert_eq!(summary["conversations"].as_array().unwrap().len(), 1);
        assert_eq!((summary["conversations"][0]["sessions"].as_i64(), summary["totals"]["turns"].as_i64()), (Some(3000), Some(3000)));
        assert_eq!(ledger.dsh_comparable_samples().count(), 0);
    }

    #[test]
    fn harness_usage_record_appends_to_the_selected_ledger_and_a_failed_write_never_fails_the_request() {
        let fixture = Fixture::new("record", json!({"model": "model-a"}), 1);
        let reported = usage(json!({"prompt_tokens": 12, "completion_tokens": 3}));
        record(&fixture.context, &place("private:8808", 1000, 0, 0), "model-a", "model-a", TURN, Some(&reported));
        record(&fixture.context, &place("private:8808", 1000, 0, 0), "model-a", "model-a", TURN, Some(&reported));
        let rows = fixture.rows();
        assert_eq!(rows.len(), 2, "each call appends");
        assert_eq!(rows[0]["sid"], json!(session("private:8808")));
        let ledger = Ledger::load_all_for_runtime(&fixture.context).unwrap();
        assert_eq!(ledger.samples.len(), 1, "the repeat of one place is read once");
        let broken = Fixture::new("record-broken", json!({"model": "model-a"}), 1);
        std::fs::create_dir_all(broken.root.join("data/token-usage-rust.jsonl")).unwrap();
        record(&broken.context, &place("private:8808", 1000, 0, 0), "model-a", "model-a", TURN, Some(&reported));
        let unrecorded: Vec<String> = crate::recovery_log::page(&broken.context, &crate::run_record::Query::default())
            .unwrap()
            .records
            .into_iter()
            .filter(|record| record.event == "usage-unrecorded")
            .map(|record| record.reason)
            .collect();
        assert_eq!(unrecorded.len(), 1, "{unrecorded:?}");
        assert!(unrecorded[0].contains("private:8808") && unrecorded[0].contains("route model-a"), "{unrecorded:?}");
    }
}
