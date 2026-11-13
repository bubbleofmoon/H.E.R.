//! 与 JS 账本（`qq-bridge/src/token-ledger.js`）逐字段对拍。
//!
//! 合成 DSH 事件流生成：`token-usage.jsonl` 是 JS 写出的账本文件，`expected.json` 是 JS 的
//! summary / series / conversation / recent / stats 输出。这里让 Rust 读同一份文件，逐值比较。
//! 时间相关字段（generatedAt、nowTier）与文件路径、落盘时刻等非账本字段不参与比较。

use super::*;
use crate::pricing::PriceTable;
use serde_json::Value;
use std::collections::BTreeSet;

struct Scenario {
    js_ledger: &'static str,
    sessions: &'static str,
    pricing: &'static str,
    expected: Value,
}

fn scenario(name: &str) -> Scenario {
    let (js_ledger, sessions, pricing, expected) = match name {
        "main" => (
            include_str!("../../tests/fixtures/token-ledger/main/token-usage.jsonl"),
            include_str!("../../tests/fixtures/token-ledger/main/sessions.json"),
            include_str!("../../tests/fixtures/token-ledger/main/pricing.json"),
            include_str!("../../tests/fixtures/token-ledger/main/expected.json"),
        ),
        "evict" => (
            include_str!("../../tests/fixtures/token-ledger/evict/token-usage.jsonl"),
            include_str!("../../tests/fixtures/token-ledger/evict/sessions.json"),
            include_str!("../../tests/fixtures/token-ledger/evict/pricing.json"),
            include_str!("../../tests/fixtures/token-ledger/evict/expected.json"),
        ),
        other => panic!("unknown fixture scenario {other}"),
    };
    Scenario {
        js_ledger,
        sessions,
        pricing,
        expected: serde_json::from_str(expected).expect("expected.json parses"),
    }
}

fn load(s: &Scenario) -> (Ledger, PriceTable) {
    let pricing: Value = serde_json::from_str(s.pricing).expect("pricing.json parses");
    let table = crate::pricing::resolve(Some(&pricing));
    let ledger = Ledger::load_all_from_sources(Some(s.js_ledger), None, Some(s.sessions));
    (ledger, table)
}

/// 逐值比较：对象键集合必须一致，数组长度与顺序必须一致，数值允许 1e-9 相对误差
/// （两边求和顺序不同会产生浮点尾差），其余类型必须完全相等。`skip` 按路径后缀忽略。
fn assert_matches(path: &str, actual: &Value, expected: &Value, skip: &[&str]) {
    let mut diffs = Vec::new();
    collect_diffs(path, actual, expected, skip, &mut diffs);
    assert!(diffs.is_empty(), "{} difference(s):\n{}", diffs.len(), diffs.join("\n"));
}

/// 收集全部差异（不在第一处差异停下），便于一次看全。
fn collect_diffs(path: &str, actual: &Value, expected: &Value, skip: &[&str], out: &mut Vec<String>) {
    if skip.iter().any(|s| path.ends_with(s)) {
        return;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            // 被跳过的字段既不比较值，也不比较是否存在。
            let keep = |k: &&String| !skip.iter().any(|s| format!("{path}.{k}").ends_with(s));
            let ak: BTreeSet<&String> = a.keys().filter(keep).collect();
            let ek: BTreeSet<&String> = e.keys().filter(keep).collect();
            if ak != ek {
                out.push(format!(
                    "object keys differ at {path}: rust-only={:?} js-only={:?}",
                    ak.difference(&ek).collect::<Vec<_>>(),
                    ek.difference(&ak).collect::<Vec<_>>()
                ));
            }
            for k in ak.intersection(&ek) {
                collect_diffs(&format!("{path}.{k}"), &a[*k], &e[*k], skip, out);
            }
        }
        (Value::Array(a), Value::Array(e)) if UNORDERED.iter().any(|s| path.ends_with(s)) => {
            // 出现次序不是持久化的账本内容：JS 按内存中首次见到的次序，压缩重写后文件次序会变。
            // 这些字段只比较多重集合（排序后相等）。
            let mut x = a.clone();
            let mut y = e.clone();
            x.sort_by_key(|v| v.to_string());
            y.sort_by_key(|v| v.to_string());
            if x != y {
                out.push(format!("unordered array differs at {path}: rust={} js={}", Value::Array(x), Value::Array(y)));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            if a.len() != e.len() {
                out.push(format!("array length differs at {path}: rust={} js={}", a.len(), e.len()));
            }
            for (i, (x, y)) in a.iter().zip(e.iter()).enumerate() {
                collect_diffs(&format!("{path}[{i}]"), x, y, skip, out);
            }
        }
        (Value::Number(_), Value::Number(_)) => {
            let x = actual.as_f64().unwrap();
            let y = expected.as_f64().unwrap();
            let tol = 1e-9 * y.abs().max(1.0);
            if (x - y).abs() > tol {
                out.push(format!("number differs at {path}: rust={x} js={y}"));
            }
        }
        _ => {
            if actual != expected {
                out.push(format!("value differs at {path}: rust={actual} js={expected}"));
            }
        }
    }
}

const TIME_SKIP: &[&str] = &["generatedAt", "nowTier"];

/// 仅比较多重集合的数组字段（会话出现次序、模型集合、走势桶内的 QQ key 集合）。
const UNORDERED: &[&str] = &[".sessionIds", ".models", ".keys"];
/// 写入重放的比较口径：`estimated` 不比较。JS 对历史采样（快照里的 usage 没有 model）按兜底价标为估算，
/// Rust 历史采样回落到快照里的最近模型选择（见 `dsh::harvest_of_snapshot`），单价相同但不再标估算；
/// 这是有意的归属改进，由 writer 测试末尾的断言单独确认 Rust 侧没有估算标记。
const WRITER_SKIP: &[&str] = &["generatedAt", "nowTier", "estimated"];
/// 写入重放里逐轮明细的口径：历史轮次在 Rust 侧带快照选择的 model/provider，JS 历史轮次没有（undefined，JSON 中省略）。
const WRITER_CONV_SKIP: &[&str] = &["generatedAt", "nowTier", "estimated", ".model", ".provider"];

#[test]
fn oracle_main_summary_matches_js() {
    let s = scenario("main");
    let (ledger, table) = load(&s);
    let actual = ledger.summary(&table, 20);
    assert_matches("summary", &actual, &s.expected["summary"], TIME_SKIP);
}

#[test]
fn oracle_main_series_matches_js() {
    let s = scenario("main");
    let (ledger, table) = load(&s);
    for (key, hours, buckets) in [("series24", 24, 4), ("series48", 48, 12), ("series1", 1, 12)] {
        let at = s.expected[key]["generatedAt"].as_i64().expect("pinned clock");
        let actual = ledger.series(&table, hours, buckets, Some(at));
        assert_matches(key, &actual, &s.expected[key], &[]);
    }
}

#[test]
fn oracle_main_conversation_recent_and_stats_match_js() {
    let s = scenario("main");
    let (ledger, table) = load(&s);
    for (key, expected) in s.expected["conversation"].as_object().unwrap() {
        let actual = ledger.conversation(&table, key, 50);
        match (actual, expected.is_null()) {
            (None, true) => {}
            (Some(actual), false) => {
                assert_matches(&format!("conversation[{key}]"), &actual, expected, TIME_SKIP)
            }
            (actual, _) => panic!("conversation presence differs for {key}: rust={actual:?}"),
        }
    }
    let recent = Value::Array(ledger.recent(10, &table));
    assert_matches("recent", &recent, &s.expected["recent"], &[]);
    let stats = ledger.stats(&table, s.expected["stats"]["maxLines"].as_u64().unwrap() as usize);
    assert_matches("stats", &stats, &s.expected["stats"], &["file", "lastIngestAt", "lastCompactedAt"]);
}

#[test]
fn oracle_evict_summary_series_and_stats_match_js() {
    let s = scenario("evict");
    let (ledger, table) = load(&s);
    let actual = ledger.summary(&table, 20);
    assert_matches("summary", &actual, &s.expected["summary"], TIME_SKIP);
    let at = s.expected["series24"]["generatedAt"].as_i64().unwrap();
    assert_matches("series24", &ledger.series(&table, 24, 4, Some(at)), &s.expected["series24"], &[]);
    let stats = ledger.stats(&table, s.expected["stats"]["maxLines"].as_u64().unwrap() as usize);
    assert_matches("stats", &stats, &s.expected["stats"], &["file", "lastIngestAt", "lastCompactedAt"]);
}

/// 写入侧重放：把 JS 实际收到的 DSH 事件流（`events.json`，由 make-fixtures.mjs 从 bridge.js 同款接线录下）
/// 逐条喂给 Rust 的折叠与写入规则（`UsageFold` / `harvest_of_snapshot` / `plan_appends`），
/// 每写一批就重载账本，最终结果必须与 JS 账本的输出逐值一致。
#[test]
fn writer_replay_of_the_dsh_stream_reproduces_the_js_ledger() {
    use crate::dsh::{harvest_of_snapshot, UsageFold, UsageHarvest};
    use std::collections::HashMap;

    let s = scenario("main");
    let pricing: Value = serde_json::from_str(s.pricing).unwrap();
    let table = crate::pricing::resolve(Some(&pricing));
    let ops: Vec<Value> =
        serde_json::from_str(include_str!("../../tests/fixtures/token-ledger/main/events.json")).expect("events.json parses");
    let preseed = include_str!("../../tests/fixtures/token-ledger/main/preseed.jsonl");
    let now = s.expected["series24"]["generatedAt"].as_i64().expect("pinned clock");

    // 活会话表：key -> sessionId（sessions.json）。未在表里的会话沿用历史记录里的 key（JS keyOf 的回落）。
    let sessions: Value = serde_json::from_str(s.sessions).unwrap();
    let mut live: HashMap<String, String> = HashMap::new();
    for (key, sid) in sessions["sessions"].as_object().unwrap() {
        live.insert(sid.as_str().unwrap().to_string(), key.clone());
    }
    let mut remembered: HashMap<String, String> = HashMap::new();
    for line in preseed.lines().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_str(line).unwrap();
        if let (Some(sid), Some(key)) = (v["sid"].as_str(), v["key"].as_str()) {
            remembered.insert(sid.to_string(), key.to_string());
        }
    }

    let mut text = preseed.to_string();
    let mut ledger = Ledger::load_all_from_sources(Some(&text), None, Some(s.sessions));
    let mut folds: HashMap<String, UsageFold> = HashMap::new();
    for op in &ops {
        let sid = op["sid"].as_str().unwrap().to_string();
        let key = live.get(&sid).cloned().or_else(|| remembered.get(&sid).cloned());
        let rows: Vec<Value> = match op["op"].as_str().unwrap() {
            "event" => {
                let fold = folds.entry(sid.clone()).or_insert_with(|| UsageFold::new(None, None));
                match fold.observe(&op["event"], now) {
                    Some(sample) => crate::ledger::harvest_candidates(
                        key.as_deref(),
                        &sid,
                        &UsageHarvest { samples: vec![sample], base: None },
                        now,
                    ),
                    None => Vec::new(),
                }
            }
            "snapshot" => {
                let snap = &op["snapshot"];
                let selection = &snap["projections"]["values"]["modelSelection"];
                let last = selection.get("lastUsed").filter(|v| !v.is_null()).or_else(|| selection.get("pending"));
                let model = last.and_then(|x| x["model"].as_str()).map(str::to_string);
                let provider = last.and_then(|x| x["provider"].as_str()).map(str::to_string);
                folds
                    .entry(sid.clone())
                    .or_insert_with(|| UsageFold::new(None, None))
                    .seed_selection(model, provider);
                crate::ledger::harvest_candidates(key.as_deref(), &sid, &harvest_of_snapshot(snap, now), now)
            }
            other => panic!("unknown replay op {other}"),
        };
        for row in crate::ledger::plan_appends(&ledger, &rows) {
            text.push_str(&format!("{row}\n"));
            if let (Some(k), Some(sid)) = (row["key"].as_str(), row["sid"].as_str()) {
                remembered.insert(sid.to_string(), k.to_string());
            }
        }
        ledger = Ledger::load_all_from_sources(Some(&text), None, Some(s.sessions));
    }

    let actual = ledger.summary(&table, 20);
    assert_matches("summary", &actual, &s.expected["summary"], WRITER_SKIP);
    // 历史采样归属到快照的模型选择（已知模型）：该会话不应被标为估算。
    let private = actual["conversations"].as_array().unwrap().iter().find(|c| c["key"] == "private:7001").unwrap();
    assert_eq!(private["estimated"], false, "history samples are attributed to the snapshot selection");
    for (key, hours, buckets) in [("series24", 24, 4), ("series48", 48, 12), ("series1", 1, 12)] {
        let at = s.expected[key]["generatedAt"].as_i64().unwrap();
        assert_matches(key, &ledger.series(&table, hours, buckets, Some(at)), &s.expected[key], WRITER_SKIP);
    }
    for (key, expected) in s.expected["conversation"].as_object().unwrap() {
        match (ledger.conversation(&table, key, 50), expected.is_null()) {
            (None, true) => {}
            (Some(actual), false) => assert_matches(&format!("conversation[{key}]"), &actual, expected, WRITER_CONV_SKIP),
            (actual, _) => panic!("conversation presence differs for {key}: rust={actual:?}"),
        }
    }
    assert_matches("recent", &Value::Array(ledger.recent(10, &table)), &s.expected["recent"], WRITER_SKIP);
    let stats = ledger.stats(&table, s.expected["stats"]["maxLines"].as_u64().unwrap() as usize);
    assert_matches("stats", &stats, &s.expected["stats"], &["file", "lastIngestAt", "lastCompactedAt"]);
}

/// 多进程读取：同一份账本的记录被拆到两个文件（JS 写一份、Rust 写一份）时，读出的结果与单文件一致。
#[test]
fn ledger_split_across_js_and_rust_files_reads_like_one_file() {
    let s = scenario("main");
    let (_, table) = load(&s);
    let lines: Vec<&str> = s.js_ledger.lines().filter(|l| !l.is_empty()).collect();
    let js: String = lines.iter().step_by(2).map(|l| format!("{l}\n")).collect();
    let rust: String = lines.iter().skip(1).step_by(2).map(|l| format!("{l}\n")).collect();
    let ledger = Ledger::load_all_from_sources(Some(&js), Some(&rust), Some(s.sessions));
    assert_matches("summary", &ledger.summary(&table, 20), &s.expected["summary"], TIME_SKIP);
    let at = s.expected["series24"]["generatedAt"].as_i64().unwrap();
    assert_matches("series24", &ledger.series(&table, 24, 4, Some(at)), &s.expected["series24"], &[]);
}

/// 流式事件/快照重放：账本里每一行都重复出现两次（重连时快照整块重发），结果必须与单次读取一致。
#[test]
fn replayed_ledger_lines_are_idempotent() {
    let s = scenario("main");
    let (_, table) = load(&s);
    let doubled = format!("{}{}", s.js_ledger, s.js_ledger);
    let ledger = Ledger::load_all_from_sources(Some(&doubled), None, Some(s.sessions));
    assert_matches("summary", &ledger.summary(&table, 20), &s.expected["summary"], TIME_SKIP);
    let at = s.expected["series24"]["generatedAt"].as_i64().unwrap();
    assert_matches("series24", &ledger.series(&table, 24, 4, Some(at)), &s.expected["series24"], &[]);
}
