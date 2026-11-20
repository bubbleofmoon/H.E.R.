//! Offline session-level retrieval comparison on the pinned public LongMemEval release.
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::Connection;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::super::{self as memory, Hit, MemoryInput, FUSION_DEPTH, RECALL_MAX_ITEMS};
use super::{
    backfill, encode_query, ensure_cache, rank_cached, register_sqlite_vec, LocalModel,
    BACKFILL_BATCH, MODEL_DIR,
};
use crate::runtime::RuntimeContext;

const CEILINGS: [f64; 9] = [0.16, 0.18, 0.20, 0.22, 0.24, 0.26, 0.28, 0.30, 0.32];
const MIN_RECALL_GAIN: f64 = 0.02;
const RECALL_EPSILON: f64 = 1e-12;
const ABSTENTION_ALLOWANCE_DIVISOR: usize = 20;
const CONVERSATION: &str = "group:101";

#[derive(Deserialize)]
struct Case {
    question_id: String,
    question: String,
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<Turn>>,
    answer_session_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Turn {
    content: String,
}

impl Case {
    fn abstains(&self) -> bool {
        self.question_id.ends_with("_abs")
    }
}

#[derive(Clone, Copy, Default)]
struct Scores {
    evidence_recall_sum: f64,
    answerable: usize,
    abstention_retrieved: usize,
    abstentions: usize,
}

impl Scores {
    fn observe(&mut self, case: &Case, hits: &[Hit]) {
        if case.abstains() {
            self.abstentions += 1;
            self.abstention_retrieved += usize::from(!hits.is_empty());
            return;
        }

        let gold: HashSet<_> = case.answer_session_ids.iter().collect();
        let recalled: HashSet<_> = hits
            .iter()
            .map(|hit| &case.haystack_session_ids[hit.id as usize - 1])
            .collect();
        self.evidence_recall_sum += gold.intersection(&recalled).count() as f64 / gold.len() as f64;
        self.answerable += 1;
    }

    fn recall(self) -> f64 {
        self.evidence_recall_sum / self.answerable as f64
    }

    fn passes(self, baseline: Self) -> bool {
        self.recall() + RECALL_EPSILON >= baseline.recall() + MIN_RECALL_GAIN
            && self.abstention_retrieved * ABSTENTION_ALLOWANCE_DIVISOR
                <= baseline.abstention_retrieved * ABSTENTION_ALLOWANCE_DIVISOR + 2 * self.abstentions // RED-M3
    }

    fn print(self, fold: &str, method: &str) {
        println!(
            "{fold} {method} recall@5={:.4} answerable={} abstention_retrieved={}/{}",
            self.recall(),
            self.answerable,
            self.abstention_retrieved,
            self.abstentions,
        );
    }
}

#[derive(Default)]
struct Comparison {
    baseline: Scores,
    hybrid: [Scores; CEILINGS.len()],
}

impl Comparison {
    fn observe(
        &mut self,
        case: &Case,
        model: &[u8],
        embed: &mut impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
        candidates: std::ops::Range<usize>,
    ) {
        assert_eq!(
            case.haystack_sessions.len(),
            case.haystack_session_ids.len()
        );
        if !case.abstains() {
            assert!(!case.answer_session_ids.is_empty());
            assert!(case
                .answer_session_ids
                .iter()
                .all(|id| case.haystack_session_ids.contains(id)));
        }
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../spec/memory-schema.sql"))
            .unwrap();
        for session in &case.haystack_sessions {
            let content = session
                .iter()
                .map(|turn| turn.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            memory::write_memory(
                &conn,
                &MemoryInput {
                    owner_type: "session",
                    owner_id: CONVERSATION,
                    scope: "group",
                    kind: "fact",
                    subject: None,
                    content: &content,
                    tags: None,
                    source: "longmemeval",
                    importance: 0.5,
                },
            )
            .expect("index benchmark session");
        }
        // Give each session the same metadata boost, independent of wall time during indexing.
        conn.execute("UPDATE memory SET created_at = 0", [])
            .unwrap();
        let store = memory::MemoryStore::from_connection(conn, None);
        let reader = store.for_reader(CONVERSATION);
        let query: String = case.question.chars().take(200).collect();
        let baseline = memory::recall(&reader, &query, None, RECALL_MAX_ITEMS, &[])
            .expect("run sparse baseline");
        self.baseline.observe(case, &baseline);

        if query.chars().filter(|&c| memory::recall_counts(c)).count()
            < memory::RECALL_MIN_SHARED_CHARS
        {
            for index in candidates {
                self.hybrid[index].observe(case, &baseline);
            }
            return;
        }

        register_sqlite_vec(reader.conn).expect("register sqlite-vec");
        ensure_cache(reader.conn).expect("create benchmark vector cache");
        for _ in case.haystack_sessions.chunks(BACKFILL_BATCH) {
            backfill(&reader, model, embed)
                .expect("backfill benchmark session vectors")
                .expect("embed benchmark sessions");
        }

        let vector = encode_query(embed, &query).expect("embed benchmark query");
        let sparse = memory::sparse_recall(&reader, &query, "", FUSION_DEPTH)
            .expect("run sparse candidate search");
        for index in candidates {
            let dense = rank_cached(&reader, &vector, "", FUSION_DEPTH, model, CEILINGS[index])
                .expect("rank benchmark session vectors");
            if dense.is_empty() {
                self.hybrid[index].observe(case, &baseline);
            } else {
                let hits = memory::fuse_recall(sparse.clone(), dense, RECALL_MAX_ITEMS);
                self.hybrid[index].observe(case, &hits);
            }
        }
    }

    fn select_ceiling(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, score) in self.hybrid.iter().enumerate() {
            if score.passes(self.baseline)
                && best.is_none_or(|previous| {
                    score.recall() > self.hybrid[previous].recall() + RECALL_EPSILON
                })
            {
                best = Some(index);
            }
        }
        best
    }
}

fn public_file(dir: &Path, name: &str, sha256: &str) -> Vec<u8> {
    let path = dir.join(name);
    assert_eq!(
        path.canonicalize().expect("resolve public benchmark input"),
        path,
        "use ordinary files under the benchmark input directory"
    );
    assert!(path.is_file(), "public benchmark input must be a file");
    let bytes = fs::read(path).expect("read pinned public benchmark input");
    assert!(
        STANDARD.encode(Sha256::digest(&bytes)) == sha256,
        "public benchmark artifact checksum mismatch"
    );
    bytes
}

#[test]
#[ignore = "requires owner-approved thresholds and staged public benchmark inputs"]
fn longmemeval_recall_calibration() {
    let inputs = PathBuf::from(
        std::env::var_os("HER_LONGMEMEVAL_DIR")
            .expect("set HER_LONGMEMEVAL_DIR to the public inputs"),
    )
    .canonicalize()
    .expect("resolve public benchmark directory");
    let dataset = public_file(
        &inputs,
        "longmemeval_s_cleaned.json",
        "1vIeqdYKDVbzSgW2CceciKRR0q4DWXgh6j1alnjDpEI=",
    );
    let mut cases: Vec<Case> = serde_json::from_slice(&dataset).expect("parse pinned LongMemEval");
    drop(dataset);
    // Inside each stratum, reserve every fifth question by hash for calibration.
    cases.sort_by_cached_key(|case| (case.abstains(), Sha256::digest(case.question_id.as_bytes())));
    let split = cases.partition_point(|case| !case.abstains());
    let (answerable, abstentions) = cases.split_at(split);
    let strata = [answerable, abstentions];
    assert_eq!((answerable.len(), abstentions.len()), (470, 30));

    let fixture = crate::runtime::tests::runtime_fixture(
        std::env::temp_dir().join(format!("her-longmemeval-{}", std::process::id())),
        &serde_json::json!({ "consoleToken": "fixture" }),
        &serde_json::json!({
            "dsh": "http://127.0.0.1:49101",
            "model": "http://127.0.0.1:49102",
            "onebot": "http://127.0.0.1:49103"
        }),
    );
    let context = RuntimeContext::from_root(&fixture.0).expect("open throwaway benchmark runtime");
    let model_dir = fixture.0.join("resources").join(MODEL_DIR);
    fs::create_dir_all(model_dir.join("onnx")).expect("create fixture model directory");
    for (name, sha256) in [
        (
            "onnx/model.onnx",
            "aaC4RvTxFrXmqr+VRupnVNAiZPMhGhOhvWmzG4BAdJo=",
        ),
        (
            "tokenizer.json",
            "SM6l1EQkkSpv0epke/T+ULVauLHlh5wydfgOM56PriY=",
        ),
        (
            "config.json",
            "1Bk+rTqBD9aU+oox1/xy+668Bmi2A+OYc0vy9lOP9C8=",
        ),
        (
            "special_tokens_map.json",
            "ttNGvjZqfR1IMy28n987+JYLXYeVIrd5ndulnnYjfuM=",
        ),
        (
            "tokenizer_config.json",
            "5vO5bbkmo31AOZlfv1rRfeFY37j2ND1gfk26rRjXX1o=",
        ),
    ] {
        fs::write(model_dir.join(name), public_file(&inputs, name, sha256))
            .expect("stage verified benchmark artifact");
    }
    let LocalModel {
        fingerprint,
        mut encoder,
        ..
    } = LocalModel::load(&context, model_dir.join("onnx/model.onnx"))
        .expect("load local CPU benchmark model");
    let mut embed = |texts: &[String]| encoder.embed(texts, Some(4)).ok();

    let mut calibration = Comparison::default();
    for case in strata.iter().flat_map(|cases| cases.iter().step_by(5)) {
        calibration.observe(case, &fingerprint, &mut embed, 0..CEILINGS.len());
    }
    calibration.baseline.print("calibration", "slice-1");
    for (ceiling, score) in CEILINGS.iter().zip(calibration.hybrid) {
        score.print("calibration", &format!("hybrid-{ceiling:.2}"));
    }

    let selected = calibration
        .select_ceiling()
        .expect("no ceiling passed the calibration criteria");
    let ceiling = CEILINGS[selected];
    println!("selected ceiling {ceiling:.2}");

    let mut holdout = Comparison::default();
    for cases in strata {
        for (index, case) in cases.iter().enumerate() {
            if index % 5 != 0 {
                holdout.observe(case, &fingerprint, &mut embed, selected..selected + 1);
            }
        }
    }
    holdout.baseline.print("holdout", "slice-1");
    holdout.hybrid[selected].print("holdout", &format!("hybrid-{ceiling:.2}"));
    assert!(
        holdout.hybrid[selected].passes(holdout.baseline),
        "selected ceiling must meet both holdout criteria"
    );
}

#[test]
fn recall_calibration_compares_sparse_with_the_cosine_sweep() {
    use super::DIMENSIONS;

    let case = Case {
        question_id: "synthetic".into(),
        question: "小孩害怕打雷该如何安抚".into(),
        haystack_session_ids: vec!["evidence".into(), "decoy".into()],
        haystack_sessions: vec![
            vec![Turn {
                content: "幼童遇到风暴时需要拥抱".into(),
            }],
            vec![Turn {
                content: "周末整理书柜".into(),
            }],
        ],
        answer_session_ids: vec!["evidence".into()],
    };
    let mut comparison = Comparison::default();
    comparison.observe(
        &case,
        &[0; 32],
        &mut |texts| {
            Some(
                texts
                    .iter()
                    .map(|text| {
                        let cosine: f32 = if text.contains(&case.question) {
                            1.0
                        } else if text.contains("幼童") {
                            0.79
                        } else {
                            0.60
                        };
                        let mut vector = vec![0.0; DIMENSIONS];
                        vector[0] = cosine;
                        vector[1] = (1.0 - cosine * cosine).sqrt();
                        vector
                    })
                    .collect(),
            )
        },
        0..CEILINGS.len(),
    );
    assert_eq!(
        (
            comparison.baseline.recall(),
            comparison.hybrid[2].recall(),
            comparison.hybrid[3].recall()
        ),
        (0.0, 0.0, 1.0),
        "only the wider cosine ceiling should add the otherwise missed evidence"
    );
}

#[test]
fn recall_calibration_requires_recall_gain_and_limits_abstention_noise() {
    let baseline = Scores {
        evidence_recall_sum: 0.40,
        answerable: 1,
        abstention_retrieved: 2,
        abstentions: 20,
    };
    let pass = Scores {
        evidence_recall_sum: 0.42,
        abstention_retrieved: 3,
        ..baseline
    };
    assert!(
        pass.passes(baseline),
        "both acceptance boundaries are inclusive"
    );
    let weak = Scores {
        evidence_recall_sum: 0.41,
        ..pass
    };
    assert!(
        !weak.passes(baseline),
        "reject a gain below two percentage points"
    );
    let noisy = Scores {
        abstention_retrieved: 4,
        ..pass
    };
    assert!(
        !noisy.passes(baseline),
        "reject excess retrieval on abstention questions"
    );
}
