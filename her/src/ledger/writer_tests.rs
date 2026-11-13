//! 写入侧的幂等规则（`plan_appends` / `harvest_candidates`）与占位记录的计数口径。

use super::*;
use crate::dsh::{BaseFact, TurnUsage, UsageHarvest};
use serde_json::json;

fn load(lines: &[&str]) -> Ledger {
    let text = lines.join("\n");
    Ledger::load_all_from_sources(Some(&text), None, None)
}

fn turn_line(seq: i64, turn: i64, step: i64, output: i64) -> String {
    format!(
        r#"{{"k":"turn","t":1700000000000,"sid":"s1","key":"group:1","seq":{seq},"turn":{turn},"step":{step},"retry":0,"model":"deepseek-flash","provider":"official","b":{{"cacheMiss":10,"output":{output}}}}}"#
    )
}

#[test]
fn null_usage_placeholder_is_evidence_and_not_a_billed_step() {
    let l = load(&[r#"{"k":"turn","t":1700000000000,"sid":"s1","key":"group:1","seq":4,"turn":1,"step":0,"retry":0,"b":null}"#]);
    assert!(l.samples.is_empty(), "a turn without usage detail must not create a sample");
    let table = crate::pricing::PriceTable::default();
    let summary = l.summary(&table, 20);
    assert_eq!(summary["totals"]["steps"], 0);
    assert_eq!(summary["totals"]["turns"], 0);
}

#[test]
fn plan_appends_keeps_only_new_or_newer_samples_and_bases() {
    let t1 = turn_line(5, 1, 1, 10);
    let base = r#"{"k":"base","t":1700000000000,"sid":"s1","key":"group:1","seq":10,"model":"deepseek-flash","provider":"official","b":{"cacheMiss":100}}"#;
    let l = load(&[t1.as_str(), base]);

    let candidates = vec![
        serde_json::from_str::<Value>(&turn_line(5, 1, 1, 10)).unwrap(), // replay of seq 5: dropped
        serde_json::from_str::<Value>(&turn_line(6, 1, 1, 11)).unwrap(), // newer seq of the same sample: kept
        serde_json::from_str::<Value>(&turn_line(3, 2, 1, 12)).unwrap(), // new sample: kept
        serde_json::from_str::<Value>(&turn_line(3, 2, 1, 12)).unwrap(), // same batch again: dropped
        json!({"k":"base","t":1,"sid":"s1","key":"group:1","seq":10,"model":"deepseek-flash","provider":"official","b":{"cacheMiss":100,"cacheHit":0,"cacheWrite":0,"output":0,"reasoning":0}}), // same base: dropped
        json!({"k":"base","t":1,"sid":"s1","key":"group:1","seq":9,"model":"deepseek-flash","provider":"official","b":{"cacheMiss":1}}), // older base: dropped
        json!({"k":"base","t":1,"sid":"s1","key":"group:1","seq":12,"model":"deepseek-flash","provider":"official","b":{"cacheMiss":200}}), // newer base: kept
    ];
    let planned = plan_appends(&l, &candidates);
    let shape: Vec<(String, i64)> = planned
        .iter()
        .map(|r| (r["k"].as_str().unwrap().to_string(), r["seq"].as_i64().unwrap()))
        .collect();
    assert_eq!(shape, vec![("turn".to_string(), 6), ("turn".to_string(), 3), ("base".to_string(), 12)]);
}

#[test]
fn harvest_candidates_map_history_samples_and_the_base_to_ledger_rows() {
    let harvest = UsageHarvest {
        samples: vec![TurnUsage {
            turn: 8,
            step: 1,
            retry: 1,
            seq: 30,
            t: 1_700_000_000_000,
            model: Some("m".into()),
            provider: None,
            usage: json!({"cacheMiss": 90, "cacheHit": 0, "cacheWrite": 0, "output": 1, "reasoning": 0}),
        }],
        base: Some(BaseFact {
            as_of_seq: 40,
            usage: json!({"cacheMiss": 9, "cacheHit": 1, "cacheWrite": 0, "output": 7, "reasoning": 0}),
            model: Some("m-sel".into()),
            provider: Some("p-sel".into()),
        }),
    };
    let rows = harvest_candidates(Some("group:1"), "s1", &harvest, 999);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["k"], "turn");
    assert_eq!(rows[0]["sid"], "s1");
    assert_eq!(rows[0]["key"], "group:1");
    assert_eq!(rows[0]["seq"], 30);
    assert_eq!(rows[0]["turn"], 8);
    assert_eq!(rows[0]["retry"], 1);
    assert_eq!(rows[0]["b"]["cacheMiss"], 90);
    assert_eq!(rows[1]["k"], "base");
    assert_eq!(rows[1]["seq"], 40);
    assert_eq!(rows[1]["t"], 999);
    assert_eq!(rows[1]["model"], "m-sel");
    assert_eq!(rows[1]["b"]["output"], 7);
}

struct ScratchRoot(std::path::PathBuf);

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 恢复场景：一轮在 DSH 里已经跑完、桥接却不在线，它的 usage 只能从开场快照里拿到。
/// 恢复核对时走的写入路径（`harvest_candidates` + `append_planned`）必须：补记一次；重放不再写；
/// 基线覆盖了这条采样，所以它不会在总量里被算两遍。
#[test]
fn recovered_turn_usage_is_written_once_and_replays_add_nothing() {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ledger-recovery-{}-{stamp}", std::process::id())));
    let _guard = ScratchRoot(dir.clone());
    for sub in ["config", "data", "resources"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("config/config.json"), br#"{"consoleToken":"fixture-console-token-abcdefghij"}"#).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), b"{}").unwrap();
    std::fs::write(
        dir.join("dependencies.json"),
        br#"{"dsh":"http://127.0.0.1:49101","model":"http://127.0.0.1:49102","onebot":"http://127.0.0.1:49103"}"#,
    )
    .unwrap();
    let context = crate::runtime::RuntimeContext::from_root(&dir).expect("synthetic runtime root");

    let snapshot = json!({
        "projections": {
            "asOfSeq": 12,
            "values": {
                "tokenUsage": {"cacheMiss": 50, "cacheHit": 10, "cacheWrite": 0, "output": 5},
                "modelSelection": {"lastUsed": {"model": "deepseek-flash", "provider": "official"}}
            }
        },
        "records": [
            {"type": "event", "event": {"seq": 11, "type": "assistant/message", "time": 1_700_000_000_000i64,
                "data": {"turn": 4, "step": 1, "usage": {"cacheMiss": 50, "cacheHit": 10, "cacheWrite": 0, "output": 5}}}}
        ]
    });
    let now = 1_700_000_100_000;
    let rows = harvest_candidates(Some("group:1"), "s1", &crate::dsh::harvest_of_snapshot(&snapshot, now), now);
    assert_eq!(append_planned(&context, &rows).unwrap(), 2, "the turn sample and the base are written once");
    assert_eq!(append_planned(&context, &rows).unwrap(), 0, "replaying the same snapshot writes nothing");

    let ledger = Ledger::load_all_for_runtime(&context).unwrap();
    let summary = ledger.summary(&crate::pricing::PriceTable::default(), 20);
    assert_eq!(summary["totals"]["steps"], 1, "one billed request");
    assert_eq!(summary["totals"]["buckets"]["cacheMiss"], 50.0, "covered by the base: counted once");
    assert_eq!(summary["totals"]["buckets"]["output"], 5.0, "covered by the base: counted once");
}
