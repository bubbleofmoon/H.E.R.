//! Token 账本（对应 JS 的 `token-ledger.js` 的载入与 recent 视图）。
//!
//! 数据源是**两份**文件（TD-1：双写无锁 → 各写各的，读时合并）：
//!   · `qq-bridge/state/token-usage.jsonl`      —— JS 写，Rust 只读
//!   · `state/token-usage-rust.jsonl`           —— Rust 写，JS 只读
//!
//! 分开写的原因：Windows 的 append 不保证原子，两个进程往同一个文件追加会交错出半行
//! （决策日志已经实测坏过 24 行）。跨进程文件锁在 Windows 上更脆，所以照抄决策日志的
//! 成功做法 —— 分文件，读取方合并。合并时**按 `t`（ts）排序**再重放，保证下面的
//! seq 幂等规则跨文件也按时间生效。
//!
//! 每行一条记录，`k` 区分类型：
//!   · `base`  —— 压缩后的基线（某会话截至某 seq 的累计桶）
//!   · `turn`  —— 单轮采样（turn/step/retry 定位）
//!   · `evict` —— 被淘汰的旧采样汇总（只保累计，不再拆到某一轮）
//!
//! 两个必须守住的幂等规则（照抄 JS 的注释，因为都是**会算错钱**的坑）：
//!   1. `turn` 只有 seq **严格变大**才允许覆盖。相等 seq 只可能是重连时的快照重放，
//!      放行「相等覆盖」会让旧快照把新数据改小，花费凭空缩水。
//!   2. `base` 旧基线不得覆盖新基线（按 seq 比较）。
//!
//! 这一版只做 `load` + `recent` + `stats`；`summary` / `series` / `conversation`
//! 是下一批（它们都建立在同一份载入结果上）。

mod storage;

pub use storage::{LedgerStorage, DEFAULT_MAX_LINES};
#[cfg(test)]
pub use storage::{LEDGER_FILE, LEDGER_FILES, RUST_LEDGER_FILE};

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::pricing::{normalize_usage, price_usage, Buckets, PriceTable};
use crate::runtime::RuntimeContext;

pub fn append_record(record: &Value) -> Result<(), String> {
    LedgerStorage::append_record(record)
}

pub fn append_record_for_runtime(
    context: &RuntimeContext,
    record: &Value,
) -> Result<(), String> {
    LedgerStorage::append_record_for_runtime(context, record)
}

pub fn append_record_to(path: &str, record: &Value) -> Result<(), String> {
    LedgerStorage::append_record_to(path, record)
}

pub(crate) fn append_record_to_durable(path: &str, record: &Value) -> Result<(), String> {
    LedgerStorage::append_record_to_durable(path, record)
}

pub(crate) fn append_turn_record_to(
    path: &str,
    record: &Value,
    durable: bool,
    deadline: Option<std::time::Instant>,
) -> Result<(), String> {
    LedgerStorage::append_turn_record_to(path, record, durable, deadline)
}

/// 一条单轮采样。
#[derive(Debug, Clone)]
pub struct Sample {
    pub session_id: String,
    pub key: Option<String>,
    pub seq: i64,
    pub t: i64,
    pub turn: i64,
    pub step: i64,
    pub retry: i64,
    pub model: String,
    /// provider 目前只落盘、不参与计价（计价按 model 查表）。
    /// 留着是因为 summary 要按 provider 分列（R3 明确要求官方 vs opencode go）。
    #[allow(dead_code)]
    pub provider: String,
    /// `harness` for a sample of the harness route module (`harness::usage`), which DSH never saw; empty for DSH's.
    pub engine: String,
    pub route: Option<String>,
    pub job: Option<String>,
    pub attempt_id: Option<String>,
    pub buckets: Buckets,
}

/// 载入后的账本状态。
#[derive(Debug, Default)]
pub struct Ledger {
    pub samples: HashMap<(String, i64, i64, i64), Sample>,
    pub baselines: HashMap<String, Value>,
    pub evicted: HashMap<String, Value>,
    pub session_keys: HashMap<String, String>,
    pub session_seen: HashMap<String, i64>,
    pub journal_lines: usize,
    pub dropped_lines: usize,
    /// 账本文件路径（stats 回给控制台排障用）。
    ///
    /// 合并读取时固定报 JS 的那一份（控制台与 JS 对拍的是同一个字段）；
    /// Rust 自己写的那份见 [`LEDGER_FILES`]。
    #[allow(dead_code)]
    pub file: String,
    // ── 插入顺序 ─────────────────────────────────────────────────────────
    // JS 用的是 Map/Set，天然保留插入顺序，`allSessionIds()` 和 `models`
    // 的输出顺序都依赖它。Rust 的 HashMap 没有顺序，所以单独记一份 ——
    // 少了这个，字段值全对但数组顺序不同，对拍会报一堆「不一致」。
    pub base_order: Vec<String>,
    pub seen_order: Vec<String>,
    pub evicted_order: Vec<String>,
    pub sample_order: Vec<(String, i64, i64, i64)>,
    pub resolved: HashMap<String, String>,
    /// Attempt ids already present in ingested `"turn"` records (`attemptId`).
    pub recorded_attempts: HashSet<String>,
}

impl Ledger {
    /// 空账本 + 活会话表反查（sessionId → key）。
    pub(super) fn empty(path: &str, sessions_json: Option<&str>) -> Ledger {
        let mut ledger = Ledger {
            file: path.to_string(),
            ..Default::default()
        };
        // 先建活会话表反查（sessionId → key），journal 里的 key 只是历史快照
        if let Some(store) = sessions_json.and_then(|t| serde_json::from_str::<Value>(t).ok()) {
            if let Some(map) = store.get("sessions").and_then(|s| s.as_object()) {
                for (key, v) in map {
                    let sid = v
                        .as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.get("sessionId").and_then(|s| s.as_str()).map(|s| s.to_string()));
                    if let Some(sid) = sid.filter(|s| !s.is_empty()) {
                        ledger.resolved.insert(sid, key.clone());
                    }
                }
            }
        }
        ledger
    }

    #[cfg(test)]
    pub fn load(path: &str) -> Ledger {
        LedgerStorage::load_single(path)
    }

    pub fn load_all() -> Ledger {
        LedgerStorage::load_all()
    }

    /// Rebuild the merged ledger from already-selected source contents. This is the shared seam
    /// for callers that read through a `RuntimeContext`; it has no implicit filesystem fallback.
    pub fn load_all_from_sources(
        js_text: Option<&str>,
        rust_text: Option<&str>,
        sessions_json: Option<&str>,
    ) -> Ledger {
        let mut ledger = Ledger::empty("data/token-usage.jsonl", sessions_json);
        let mut rows = read_rows_from_sources(&[js_text, rust_text]);
        order_merged(&mut rows);
        ledger.ingest(rows);
        ledger
    }

    /// Load both selected-runtime ledgers and the selected-runtime session map. Missing candidate
    /// files are treated as an empty source, while invalid paths and non-UTF-8 files fail closed.
    pub fn load_all_for_runtime(context: &RuntimeContext) -> Result<Ledger, String> {
        LedgerStorage::load_all_for_runtime(context)
    }

    /// 合并任意多份账本（单测用临时文件走这条）。
    pub fn load_paths(paths: &[&str]) -> Ledger {
        LedgerStorage::load_paths(paths)
    }

    /// 按给定顺序重放行（坏行计数跳过，不影响整份读取）。
    pub(super) fn ingest(&mut self, rows: Vec<(i64, usize, usize, String)>) {
        for (_, _, _, line) in rows {
            self.journal_lines += 1;
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                // 进程被杀留下的半行：跳过并计数，不要因为一行坏掉整份读不出来
                self.dropped_lines += 1;
                continue;
            };
            // 与 JS 一致：只有解析失败计入 droppedLines；被幂等规则拒绝的旧记录是正常情况，不算坏行。
            self.apply(&record);
        }
    }

    /// Apply one already-parsed record; reports stream projected records through the same deduplication rules.
    pub(crate) fn apply(&mut self, record: &Value) -> bool {
        let Some(sid) = record.get("sid").and_then(|v| v.as_str()) else {
            return false;
        };
        if sid.is_empty() {
            return false;
        }
        let sid = sid.to_string();
        if let Some(k) = record.get("key").and_then(|v| v.as_str()) {
            if !k.is_empty() {
                self.session_keys.insert(sid.clone(), k.to_string());
            }
        }
        let seq = record.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
        let t = record.get("t").and_then(|v| v.as_i64()).unwrap_or(0);
        let buckets = normalize_usage(record.get("b"));
        let kind = record.get("k").and_then(|v| v.as_str()).unwrap_or("");

        match kind {
            "evict" => {
                let dropped = record.get("dropped").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(prev) = self.evicted.get(&sid) {
                    if prev.get("dropped").and_then(|v| v.as_i64()).unwrap_or(0) >= dropped {
                        return false;
                    }
                }
                if !self.evicted.contains_key(&sid) {
                    self.evicted_order.push(sid.clone());
                }
                self.evicted.insert(
                    sid.clone(),
                    json!({
                        "sessionId": sid,
                        "key": record.get("key").cloned().unwrap_or(Value::Null),
                        "buckets": buckets_json(&buckets),
                        "cost": record.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        "byTier": {
                            "peak": record.get("peak").and_then(|v| v.as_f64()).unwrap_or(0.0),
                            "off": record.get("off").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        },
                        "estimated": record.get("estimated").and_then(|v| v.as_bool()).unwrap_or(false),
                        "dropped": dropped,
                        "maxSeq": record.get("maxSeq").and_then(|v| v.as_i64()).unwrap_or(-1),
                    }),
                );
                self.touch_seen(&sid, t);
                true
            }
            "base" => {
                if let Some(prev) = self.baselines.get(&sid) {
                    if prev.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1) > seq {
                        return false; // 旧基线不得覆盖新基线
                    }
                }
                if !self.baselines.contains_key(&sid) {
                    self.base_order.push(sid.clone());
                }
                self.baselines.insert(
                    sid.clone(),
                    json!({
                        "sessionId": sid,
                        "key": record.get("key").cloned().unwrap_or(Value::Null),
                        "seq": seq,
                        "t": t,
                        "model": record.get("model").cloned().unwrap_or(Value::Null),
                        "provider": record.get("provider").cloned().unwrap_or(Value::Null),
                        "buckets": buckets_json(&buckets),
                    }),
                );
                self.touch_seen(&sid, t);
                true
            }
            "turn" => {
                if let Some(id) = record
                    .get("attemptId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.recorded_attempts.insert(id.to_string());
                }
                let Some(turn) = record.get("turn").and_then(|v| v.as_i64()) else {
                    return false;
                };
                let Some(step) = record.get("step").and_then(|v| v.as_i64()) else {
                    return false;
                };
                // `b: null` 是「回合跑过但没有 usage 明细」的占位证据：不是计费请求，不产生采样（不计 steps）。
                if record.get("b").map_or(false, Value::is_null) {
                    return false;
                }
                let retry = record.get("retry").and_then(|v| v.as_i64()).unwrap_or(0);
                let sk = (sid.clone(), turn, step, retry);
                // 幂等：只有 seq 严格变大才允许覆盖
                if let Some(prev) = self.samples.get(&sk) {
                    if seq >= 0 && prev.seq >= seq {
                        return false;
                    }
                }
                if !self.samples.contains_key(&sk) {
                    self.sample_order.push(sk.clone());
                }
                self.samples.insert(
                    sk,
                    Sample {
                        session_id: sid.clone(),
                        key: record
                            .get("key")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        seq,
                        t,
                        turn,
                        step,
                        retry,
                        model: record
                            .get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        provider: record
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        engine: record
                            .get("engine")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        route: record
                            .get("route")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        job: record
                            .get("job")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        attempt_id: record
                            .get("attemptId")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        buckets,
                    },
                );
                self.touch_seen(&sid, t);
                true
            }
            _ => false,
        }
    }

    fn touch_seen(&mut self, sid: &str, t: i64) {
        if !self.session_seen.contains_key(sid) {
            self.seen_order.push(sid.to_string());
        }
        let entry = self.session_seen.entry(sid.to_string()).or_insert(0);
        if t > *entry {
            *entry = t;
        }
    }

    /// 会话对应的 QQ key：活会话表优先，其次才是日志里的历史 key（与 JS 同序）。
    pub fn key_of(&self, session_id: &str) -> Option<String> {
        self.resolved
            .get(session_id)
            .cloned()
            .or_else(|| self.session_keys.get(session_id).cloned())
    }

    /// The samples DSH's own usage total can contain, in ledger order: every sample except the harness route module's
    /// (`harness::usage`), whose requests never went through DSH.
    pub fn dsh_comparable_samples(&self) -> impl Iterator<Item = &Sample> {
        self.sample_order
            .iter()
            .filter_map(|key| self.samples.get(key))
            .filter(|sample| sample.engine != crate::harness::usage::ENGINE)
    }

    /// 最近的采样（按时间倒序，取前 N 条），每条带金额与峰谷标记。
    pub fn recent(&self, limit: usize, table: &PriceTable) -> Vec<Value> {
        // 按插入顺序遍历再稳定排序：同一时刻的采样保持 JS Map 的先后次序（HashMap 顺序不确定）。
        let mut all: Vec<&Sample> = self.sample_order.iter().filter_map(|k| self.samples.get(k)).collect();
        all.sort_by_key(|s| std::cmp::Reverse(s.t));
        all.iter()
            .take(limit)
            .map(|s| {
                let (rates, cost) = price_usage(&s.buckets, &s.model, s.t, table);
                let mut out = json!({
                    "time": s.t,
                    "key": s.key.clone().or_else(|| self.key_of(&s.session_id)),
                    "sessionId": s.session_id,
                    "turn": s.turn,
                    "step": s.step,
                    "retry": s.retry,
                    "model": if s.model.is_empty() { Value::Null } else { json!(s.model) },
                    "tokens": s.buckets.total(),
                });
                // 桶字段直接摊平在条目上（JS 是 ...s.buckets）
                if let Some(obj) = out.as_object_mut() {
                    for (k, v) in buckets_json(&s.buckets).as_object().cloned().unwrap_or_default() {
                        obj.insert(k, v);
                    }
                    obj.insert("cost".into(), json!(cost.total));
                    obj.insert("tier".into(), json!(rates.tier));
                    obj.insert("estimated".into(), json!(rates.estimated));
                }
                out
            })
            .collect()
    }

    /// 账本自身的健康指标（控制台排障用）。
    /// /api/tokens/summary 会把这份一起回（JS 是 ledger: tokenLedger.stats()）。
    #[allow(dead_code)]
    pub fn stats(&self, table: &PriceTable, max_lines: usize) -> Value {
        let mut sessions: Vec<&String> = self.baselines.keys().collect();
        for k in self.session_seen.keys() {
            if !sessions.contains(&k) {
                sessions.push(k);
            }
        }
        json!({
            // 路径用反斜杠形式：桥接那边是 Windows 原生路径，前端会原样展示
            "file": self.file.replace('/', "\\"),
            "journalLines": self.journal_lines,
            "maxLines": max_lines,
            "droppedLines": self.dropped_lines,
            "sessions": sessions.len(),
            "baselines": self.baselines.len(),
            "samples": self.samples.len(),
            "lastIngestAt": 0,
            "lastCompactedAt": 0,
            "priceVersion": table.version,
        })
    }
}

/// 一个会话的逐轮索引项（`buildIndex` 的产物）。
#[derive(Debug, Default, Clone)]
pub struct IdxEntry {
    pub live_buckets: Buckets,
    pub live_cost: f64,
    pub live_by_tier: (f64, f64),
    pub all_cost: f64,
    pub all_by_tier: (f64, f64),
    pub estimated: bool,
    pub turns: HashMap<i64, TurnRow>,
    pub models: Vec<String>,
}

/// 某一轮的汇总行。
///
/// `session_id` / `key` / `turn` 是 `/api/tokens/turns`（逐轮明细）要用的字段，
/// 那一批还没接；先留着，别为了过 clippy 把数据丢掉。
#[derive(Debug, Clone)]
pub struct TurnRow {
    #[allow(dead_code)]
    pub session_id: String,
    #[allow(dead_code)]
    pub key: Option<String>,
    #[allow(dead_code)]
    pub turn: i64,
    pub steps: i64,
    pub attempts: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub model: String,
    pub provider: String,
    pub buckets: Buckets,
    pub cost: f64,
    pub tier: String,
}

fn tier_slot(tier: &str) -> usize {
    if tier == "peak" {
        0
    } else {
        1
    }
}

impl Ledger {
    /// 把采样按会话聚合成索引（对应 `buildIndex`）。
    ///
    /// 「活数据」的定义：该采样的 seq 比基线更新的（或压根没有基线）。
    /// 基线之前的采样已经算进基线累计了，再算一遍就是重复计费。
    pub fn build_index(&self, table: &PriceTable) -> HashMap<String, IdxEntry> {
        let mut index: HashMap<String, IdxEntry> = HashMap::new();
        for sk in &self.sample_order {
            let Some(s) = self.samples.get(sk) else { continue };
            let entry = index.entry(s.session_id.clone()).or_default();
            let is_live = match self.baselines.get(&s.session_id) {
                Some(b) => {
                    let base_seq = b.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
                    s.seq > base_seq
                }
                None => true,
            };
            let at = if s.t != 0 { s.t } else { now_ms() };
            let (rates, cost) = price_usage(&s.buckets, &s.model, at, table);
            entry.estimated |= rates.estimated;
            entry.all_cost += cost.total;
            let slot = tier_slot(&rates.tier);
            if slot == 0 {
                entry.all_by_tier.0 += cost.total;
            } else {
                entry.all_by_tier.1 += cost.total;
            }
            if is_live {
                entry.live_buckets = entry.live_buckets.add(&s.buckets);
                entry.live_cost += cost.total;
                if slot == 0 {
                    entry.live_by_tier.0 += cost.total;
                } else {
                    entry.live_by_tier.1 += cost.total;
                }
            }

            let row = entry.turns.entry(s.turn).or_insert_with(|| TurnRow {
                session_id: s.session_id.clone(),
                key: s.key.clone(),
                turn: s.turn,
                steps: 0,
                attempts: 0,
                started_at: s.t,
                ended_at: s.t,
                model: s.model.clone(),
                provider: s.provider.clone(),
                buckets: Buckets::default(),
                cost: 0.0,
                tier: String::new(),
            });
            row.buckets = row.buckets.add(&s.buckets);
            row.steps += 1;
            if s.retry > 0 {
                row.attempts += 1;
            }
            if s.t < row.started_at {
                row.started_at = s.t;
            }
            if s.t > row.ended_at {
                row.ended_at = s.t;
            }
            if !s.model.is_empty() {
                row.model = s.model.clone();
                if !entry.models.contains(&s.model) {
                    entry.models.push(s.model.clone());
                }
            }
            if !s.provider.is_empty() {
                row.provider = s.provider.clone();
            }
            row.cost += cost.total;
            // 同一轮里峰谷混用就标 mixed，避免展示成单一档位
            row.tier = if row.tier.is_empty() || row.tier == rates.tier {
                rates.tier
            } else {
                "mixed".to_string()
            };
        }
        index
    }

    /// 被淘汰的汇总是否已被基线覆盖（覆盖了就不能再计一次）。
    fn eviction_covered(&self, sid: &str, gone: &Value) -> bool {
        let Some(base) = self.baselines.get(sid) else {
            return false;
        };
        let max_seq = gone.get("maxSeq").and_then(|v| v.as_i64()).unwrap_or(-1);
        if max_seq < 0 {
            return false;
        }
        base.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1) >= max_seq
    }

    /// 账本里出现过的全部 sessionId（基线 / 采样 / 淘汰 / 活动记录四处并集）。
    pub fn all_session_ids(&self) -> Vec<String> {
        // 顺序照抄 JS：baselines → sessionSeen → evicted → samples. Membership is a set, so the cost stays linear in the
        // number of sessions (harness usage adds one session per process and conversation).
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut ids: Vec<String> = Vec::new();
        let samples = self.sample_order.iter().map(|sk| &sk.0);
        for k in self.base_order.iter().chain(&self.seen_order).chain(&self.evicted_order).chain(samples) {
            if seen.insert(k.as_str()) {
                ids.push(k.clone());
            }
        }
        ids
    }

    /// 汇总：全局 + 每个会话（对应 `summary`）。
    /// **按 provider 分列**（R3 要求：官方 DeepSeek 与 opencode go 分开看）。
    ///
    /// 只看**活数据**：基线之前的采样已经算进基线累计，再算一遍就是重复计费
    /// —— 和 `build_index` 用的是同一条判据。
    ///
    /// 峰谷分开统计：同一份 token 在高峰/低峰的单价不同，混在一起看不出"贵在哪"。
    pub fn by_provider(&self, table: &PriceTable) -> Value {
        struct Agg {
            samples: i64,
            buckets: Buckets,
            cost: f64,
            peak: f64,
            off: f64,
            estimated: bool,
            models: Vec<(String, i64, f64)>,
        }
        let mut map: std::collections::BTreeMap<String, Agg> = std::collections::BTreeMap::new();
        for sk in &self.sample_order {
            let Some(s) = self.samples.get(sk) else { continue };
            if let Some(b) = self.baselines.get(&s.session_id) {
                let base_seq = b.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
                if s.seq <= base_seq {
                    continue;
                }
            }
            let at = if s.t != 0 { s.t } else { now_ms() };
            let (rates, cost) = price_usage(&s.buckets, &s.model, at, table);
            let provider = if s.provider.is_empty() { "unknown" } else { s.provider.as_str() };
            let agg = map.entry(provider.to_string()).or_insert_with(|| Agg {
                samples: 0,
                buckets: Buckets::default(),
                cost: 0.0,
                peak: 0.0,
                off: 0.0,
                estimated: false,
                models: Vec::new(),
            });
            agg.samples += 1;
            agg.buckets = agg.buckets.add(&s.buckets);
            agg.cost += cost.total;
            agg.estimated |= rates.estimated;
            if rates.tier == "peak" {
                agg.peak += cost.total;
            } else {
                agg.off += cost.total;
            }
            match agg.models.iter_mut().find(|(m, _, _)| *m == s.model) {
                Some((_, n, c)) => {
                    *n += 1;
                    *c += cost.total;
                }
                None => agg.models.push((s.model.clone(), 1, cost.total)),
            }
        }
        let mut rows: Vec<Value> = Vec::new();
        let mut total_cost = 0.0;
        let mut total_samples = 0i64;
        for (provider, a) in &map {
            total_cost += a.cost;
            total_samples += a.samples;
            let mut models: Vec<Value> = a
                .models
                .iter()
                .map(|(m, n, c)| json!({ "model": m, "samples": n, "cost": c }))
                .collect();
            models.sort_by(|x, y| {
                y.get("cost")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    .partial_cmp(&x.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            rows.push(json!({
                "provider": provider,
                "samples": a.samples,
                "tokens": {
                    "in": a.buckets.cache_miss + a.buckets.cache_hit + a.buckets.cache_write,
                    "out": a.buckets.output,
                    "reasoning": a.buckets.reasoning,
                    "cacheHit": a.buckets.cache_hit,
                    "cacheMiss": a.buckets.cache_miss,
                    "cacheWrite": a.buckets.cache_write,
                    "total": a.buckets.total(),
                },
                "cost": a.cost,
                "byTier": { "peak": a.peak, "off": a.off },
                "estimated": a.estimated,
                "models": models,
            }));
        }
        // 花钱多的排前面：面板第一眼要看到"谁最贵"
        rows.sort_by(|x, y| {
            y.get("cost")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(&x.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        json!({
            "providers": rows,
            "totalCost": total_cost,
            "totalSamples": total_samples,
        })
    }
    pub fn summary(&self, table: &PriceTable, limit: usize) -> Value {
        let index = self.build_index(table);

        // 按 QQ 会话 key 分组；没有 key 的用「未归属:前8位」兜底
        let mut by_key: Vec<(String, Vec<String>)> = Vec::new();
        for sid in self.all_session_ids() {
            let key = self
                .key_of(&sid)
                .or_else(|| {
                    self.baselines
                        .get(&sid)
                        .and_then(|b| b.get("key"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| {
                    let head: String = sid.chars().take(8).collect();
                    format!("未归属:{head}")
                });
            match by_key.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(sid),
                None => by_key.push((key, vec![sid])),
            }
        }

        let mut conversations: Vec<Value> = by_key
            .iter()
            .map(|(key, sids)| self.session_view(key, sids, &index, table))
            .collect();
        conversations.sort_by(|a, b| {
            let ca = a.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let cb = b.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
            cb.partial_cmp(&ca)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let ta = a.get("buckets").map(value_to_buckets).map(|b| b.total()).unwrap_or(0.0);
                    let tb = b.get("buckets").map(value_to_buckets).map(|b| b.total()).unwrap_or(0.0);
                    tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let mut totals = Buckets::default();
        let mut cost = 0.0;
        let mut attributed = 0.0;
        let mut unattributed = 0.0;
        let mut peak = 0.0;
        let mut off = 0.0;
        let mut turns = 0i64;
        let mut steps = 0i64;
        let mut estimated = false;
        for c in &conversations {
            let cb = c.get("buckets").map(value_to_buckets).unwrap_or_default();
            totals = totals.add(&cb);
            cost += c.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
            attributed += c.get("attributedCost").and_then(|v| v.as_f64()).unwrap_or(0.0);
            unattributed += c.get("unattributedCost").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if let Some(t) = c.get("byTier") {
                peak += t.get("peak").and_then(|v| v.as_f64()).unwrap_or(0.0);
                off += t.get("off").and_then(|v| v.as_f64()).unwrap_or(0.0);
            }
            turns += c.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
            steps += c.get("steps").and_then(|v| v.as_i64()).unwrap_or(0);
            estimated |= c.get("estimated").and_then(|v| v.as_bool()).unwrap_or(false);
        }

        let total_tokens = totals.total();
        let input_tokens = totals.cache_miss + totals.cache_hit + totals.cache_write;
        let now = now_ms();
        let now_rates = crate::pricing::rates_at("", now, table);
        let count = conversations.len();

        json!({
            "ok": true,
            "generatedAt": now,
            "priceVersion": table.version,
            "currency": table.currency,
            "symbol": table.symbol,
            "nowTier": rates_json(&now_rates),
            "priceTable": price_table_json(table),
            "totals": {
                "buckets": buckets_json(&totals),
                "cost": cost,
                "attributedCost": attributed,
                "unattributedCost": unattributed,
                "byTier": { "peak": peak, "off": off },
                "turns": turns,
                "steps": steps,
                "conversations": count,
                "estimated": estimated,
                "totalTokens": total_tokens,
                "inputTokens": input_tokens,
                "cacheHitRate": if input_tokens > 0.0 { totals.cache_hit / input_tokens } else { 0.0 },
                "avgCostPerTurn": if turns > 0 { cost / turns as f64 } else { 0.0 },
                "avgCostPerStep": if steps > 0 { cost / steps as f64 } else { 0.0 },
            },
            "conversations": conversations.into_iter().take(limit.clamp(1, 500)).collect::<Vec<_>>(),
        })
    }

    /// 单会话视图（对应 `sessionView`）。
    fn session_view(
        &self,
        key: &str,
        session_ids: &[String],
        index: &HashMap<String, IdxEntry>,
        table: &PriceTable,
    ) -> Value {
        let mut buckets = Buckets::default();
        let mut cost = 0.0;
        let mut attributed = 0.0;
        let mut unattributed = 0.0;
        let mut by_tier = (0.0_f64, 0.0_f64);
        let mut turns = 0i64;
        let mut steps = 0i64;
        let mut estimated = false;
        let mut first_at = 0i64;
        let mut last_at = 0i64;
        let mut models: Vec<String> = Vec::new();

        for sid in session_ids {
            let base = self.baselines.get(sid);
            let entry = index.get(sid);
            let gone = self.evicted.get(sid).filter(|g| !self.eviction_covered(sid, g));

            let live = entry.map(|e| e.live_buckets).unwrap_or_default();
            let mut cumulative = base
                .map(|b| b.get("buckets").map(value_to_buckets).unwrap_or_default())
                .unwrap_or_default();
            if let Some(g) = gone {
                cumulative =
                    cumulative.add(&g.get("buckets").map(value_to_buckets).unwrap_or_default());
            }
            buckets = buckets.add(&cumulative).add(&live);

            let mut c = entry.map(|e| e.live_cost).unwrap_or(0.0)
                + gone
                    .and_then(|g| g.get("cost"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
            if let Some(g) = gone {
                if let Some(t) = g.get("byTier") {
                    by_tier.0 += t.get("peak").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    by_tier.1 += t.get("off").and_then(|v| v.as_f64()).unwrap_or(0.0);
                }
                estimated |= g.get("estimated").and_then(|v| v.as_bool()).unwrap_or(false);
            }
            if let Some(e) = entry {
                by_tier.0 += e.live_by_tier.0;
                by_tier.1 += e.live_by_tier.1;
                estimated |= e.estimated;
            }
            if let Some(b) = base {
                let bb = b.get("buckets").map(value_to_buckets).unwrap_or_default();
                let model = b.get("model").and_then(|v| v.as_str()).unwrap_or("");
                let at = b
                    .get("t")
                    .and_then(|v| v.as_i64())
                    .filter(|t| *t != 0)
                    .unwrap_or_else(now_ms);
                let (rates, bcost) = price_usage(&bb, model, at, table);
                c += bcost.total;
                if tier_slot(&rates.tier) == 0 {
                    by_tier.0 += bcost.total;
                } else {
                    by_tier.1 += bcost.total;
                }
                estimated |= rates.estimated;
            }
            cost += c;

            if let Some(e) = entry {
                for m in &e.models {
                    if !models.contains(m) {
                        models.push(m.clone());
                    }
                }
                for row in e.turns.values() {
                    steps += row.steps;
                    if first_at == 0 || row.started_at < first_at {
                        first_at = row.started_at;
                    }
                    if row.ended_at > last_at {
                        last_at = row.ended_at;
                    }
                }
                turns += e.turns.len() as i64;
            }
            // 能拆到具体某一轮的花费 = 所有采样花费之和；差额是只剩累计值的历史
            let attr = entry.map(|e| e.all_cost).unwrap_or(0.0);
            attributed += attr;
            unattributed += (c - attr).max(0.0);
        }
        for sid in session_ids {
            if let Some(t) = self.session_seen.get(sid) {
                if *t > last_at {
                    last_at = *t;
                }
            }
        }

        json!({
            "key": key,
            "sessionIds": session_ids,
            "sessions": session_ids.len(),
            "buckets": buckets_json(&buckets),
            "cost": cost,
            "unattributedCost": unattributed,
            "attributedCost": attributed,
            "byTier": { "peak": by_tier.0, "off": by_tier.1 },
            "turns": turns,
            "steps": steps,
            "estimated": estimated,
            "lastAt": last_at,
            "firstAt": first_at,
            "models": models,
        })
    }
}

/// 桶宽阶梯（与 JS 的 `BUCKET_LADDER_MS` 一致）。
pub const BUCKET_LADDER_MS: &[i64] = &[
    5 * 60 * 1000,
    15 * 60 * 1000,
    30 * 60 * 1000,
    60 * 60 * 1000,
    2 * 60 * 60 * 1000,
    3 * 60 * 60 * 1000,
    6 * 60 * 60 * 1000,
    12 * 60 * 60 * 1000,
    24 * 60 * 60 * 1000,
    2 * 24 * 60 * 60 * 1000,
    7 * 24 * 60 * 60 * 1000,
];

/// 在不超过 maxBuckets 的前提下选最细的桶。
pub fn pick_bucket_ms(window_ms: i64, max_buckets: i64) -> i64 {
    for c in BUCKET_LADDER_MS {
        if window_ms / c <= max_buckets {
            return *c;
        }
    }
    *BUCKET_LADDER_MS.last().unwrap()
}

impl Ledger {
/// 按时间分桶的走势（对应 `series`）。控制台每 3 秒拉一次，所以刻意做成**单遍扫描**。
///
/// 三个不能省的细节（JS 注释里写死了，照抄）：
///   1. 空桶必须补零出现 —— 否则「那段时间很安静」会被画成「那两小时之间什么都没有」，图会撒谎。
///   2. 空桶也要有峰谷档位，按该时刻真实归属标 —— 否则看不出「这段时间本来就是 2 倍价」。
///   3. `offChartCost` = 权威总额 − 窗口内合计。图上加起来不等于总花费时，
///      前端要能解释差额（更早的回合 / 基线覆盖的历史），否则用户会以为丢钱了。
pub fn series(&self, table: &PriceTable, hours: i64, max_buckets: i64, at: Option<i64>) -> Value {
    let end_at = at.unwrap_or_else(now_ms);
    let window_hours = hours.clamp(1, 24 * 90);
    let window_ms = window_hours * 60 * 60 * 1000;
    let bucket_ms = pick_bucket_ms(window_ms, max_buckets.clamp(6, 120));
    // 桶边界对齐到 bucket_ms 整数倍，刷新之间桶不漂移
    let end = end_at.div_euclid(bucket_ms) * bucket_ms + bucket_ms;
    let start = end - (window_ms + bucket_ms - 1) / bucket_ms * bucket_ms;
    let count = ((end - start) / bucket_ms) as usize;

    #[derive(Clone)]
    struct Row {
        t: i64,
        cost: f64,
        tokens: f64,
        cache_miss: f64,
        cache_hit: f64,
        cache_write: f64,
        output: f64,
        turns: i64,
        steps: i64,
        tier: Option<String>,
        keys: Vec<String>,
        turn_ids: std::collections::HashSet<String>,
    }
    let mut rows: Vec<Row> = (0..count)
        .map(|i| Row {
            t: start + i as i64 * bucket_ms,
            cost: 0.0,
            tokens: 0.0,
            cache_miss: 0.0,
            cache_hit: 0.0,
            cache_write: 0.0,
            output: 0.0,
            turns: 0,
            steps: 0,
            tier: None,
            keys: Vec::new(),
            turn_ids: std::collections::HashSet::new(),
        })
        .collect();

    let mut totals_cost = 0.0;
    let mut totals_tokens = 0.0;
    let mut totals_turns = 0i64;
    let mut totals_steps = 0i64;
    let mut estimated = false;
    let mut counted = 0i64;
    let mut turn_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 权威总额（含窗口外）：淘汰汇总 + 基线 + 未被基线覆盖的采样，与 summary 同口径
    let mut ledger_cost = 0.0;
    let mut ledger_buckets = Buckets::default();
    for (sid, e) in &self.evicted {
        if self.eviction_covered(sid, e) {
            continue; // 已被基线覆盖，再加一次就是重复计费
        }
        ledger_cost += e.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
        ledger_buckets =
            ledger_buckets.add(&e.get("buckets").map(value_to_buckets).unwrap_or_default());
    }
    for b in self.baselines.values() {
        let bb = b.get("buckets").map(value_to_buckets).unwrap_or_default();
        let model = b.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let at = b
            .get("t")
            .and_then(|v| v.as_i64())
            .filter(|t| *t != 0)
            .unwrap_or_else(now_ms);
        let (_, cost) = price_usage(&bb, model, at, table);
        ledger_cost += cost.total;
        ledger_buckets = ledger_buckets.add(&bb);
    }

    for sk in &self.sample_order {
        let Some(s) = self.samples.get(sk) else { continue };
        let at = if s.t != 0 { s.t } else { end_at };
        let (rates, cost) = price_usage(&s.buckets, &s.model, at, table);
        let tokens = s.buckets.total();
        let is_live = match self.baselines.get(&s.session_id) {
            Some(b) => s.seq > b.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1),
            None => true,
        };
        if is_live {
            ledger_cost += cost.total;
            ledger_buckets = ledger_buckets.add(&s.buckets);
        }
        if s.t < start || s.t >= end {
            continue;
        }
        let idx = (((s.t - start) / bucket_ms) as usize).min(count.saturating_sub(1));
        if count == 0 {
            continue;
        }
        let row = &mut rows[idx];
        row.cost += cost.total;
        row.tokens += tokens;
        row.cache_miss += s.buckets.cache_miss;
        row.cache_hit += s.buckets.cache_hit;
        row.cache_write += s.buckets.cache_write;
        row.output += s.buckets.output;
        row.steps += 1;
        row.tier = Some(match &row.tier {
            None => rates.tier.clone(),
            Some(t) if t == &rates.tier => rates.tier.clone(),
            Some(_) => "mixed".to_string(),
        });
        let k = self
            .key_of(&s.session_id)
            .or_else(|| s.key.clone())
            .filter(|x| !x.is_empty());
        if let Some(k) = k {
            if !row.keys.contains(&k) {
                row.keys.push(k);
            }
        }
        let turn_id = format!("{}|{}", s.session_id, s.turn);
        if row.turn_ids.insert(turn_id.clone()) {
            row.turns += 1;
        }
        if turn_seen.insert(turn_id) {
            totals_turns += 1;
        }
        totals_cost += cost.total;
        totals_tokens += tokens;
        totals_steps += 1;
        estimated |= rates.estimated;
        counted += 1;
    }

    for row in rows.iter_mut() {
        if row.tier.is_none() {
            row.tier = Some(if crate::pricing::is_peak_at(row.t, table) {
                "peak".into()
            } else {
                "off".into()
            });
        }
    }

    // 与 JS 一致：并列最高时取第一个桶（max_by 会取最后一个）。
    let mut busiest_row: Option<&Row> = None;
    for r in &rows {
        if r.cost > 0.0 && busiest_row.map_or(true, |b| r.cost > b.cost) {
            busiest_row = Some(r);
        }
    }
    let busiest = busiest_row.map(|r| json!({ "t": r.t, "cost": r.cost, "tokens": r.tokens }));

    let ledger_tokens = ledger_buckets.total();
    let buckets: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "t": r.t, "cost": r.cost, "tokens": r.tokens,
                "cacheMiss": r.cache_miss, "cacheHit": r.cache_hit,
                "cacheWrite": r.cache_write, "output": r.output,
                "turns": r.turns, "steps": r.steps, "tier": r.tier, "keys": r.keys,
            })
        })
        .collect();

    json!({
        "ok": true,
        "generatedAt": end_at,
        "currency": table.currency,
        "symbol": table.symbol,
        "hours": window_hours,
        "bucketMs": bucket_ms,
        "from": start,
        "to": end,
        "buckets": buckets,
        "totals": {
            "cost": totals_cost, "tokens": totals_tokens,
            "turns": totals_turns, "steps": totals_steps,
        },
        "busiest": busiest,
        "estimated": estimated,
        "countedSamples": counted,
        "offChartCost": (ledger_cost - totals_cost).max(0.0),
        "offChartTokens": (ledger_tokens - totals_tokens).max(0.0),
    })
}

/// 某个会话的逐轮明细（新的在前）。
///
/// 注意 `turns` 是**数组**（覆盖掉视图里的同名计数），与 JS 的 `...view, turns: rows` 一致。
pub fn conversation(&self, table: &PriceTable, key: &str, turn_limit: i64) -> Option<Value> {
    let sids: Vec<String> = self.all_session_ids().into_iter().filter(|sid| self.key_of(sid).as_deref() == Some(key)).collect();
    if sids.is_empty() {
        return None;
    }
    let index = self.build_index(table);
    let mut view = self.session_view(key, &sids, &index, table);
    let mut rows: Vec<&TurnRow> = Vec::new();
    for sid in &sids {
        if let Some(e) = index.get(sid) {
            rows.extend(e.turns.values());
        }
    }
    rows.sort_by(|a, b| b.turn.cmp(&a.turn).then(b.ended_at.cmp(&a.ended_at)));
    let limit = turn_limit.clamp(1, 1000) as usize;
    let turns: Vec<Value> = rows
        .iter()
        .take(limit)
        .map(|r| {
            let mut row = json!({
                "sessionId": r.session_id, "turn": r.turn,
                "steps": r.steps, "attempts": r.attempts,
                "startedAt": r.started_at, "endedAt": r.ended_at,
                "buckets": buckets_json(&r.buckets),
                "cost": r.cost, "tier": r.tier,
            });
            // JS 的 undefined 字段在 JSON 里不出现：没有 key/model/provider 时就省略，而不是输出 null 或空串。
            if let Some(obj) = row.as_object_mut() {
                if let Some(k) = &r.key {
                    obj.insert("key".into(), json!(k));
                }
                if !r.model.is_empty() {
                    obj.insert("model".into(), json!(r.model));
                }
                if !r.provider.is_empty() {
                    obj.insert("provider".into(), json!(r.provider));
                }
            }
            row
        })
        .collect();
    if let Some(obj) = view.as_object_mut() {
        obj.insert("ok".into(), json!(true));
        obj.insert("generatedAt".into(), json!(now_ms()));
        obj.insert("currency".into(), json!(table.currency));
        obj.insert("symbol".into(), json!(table.symbol));
        obj.insert("turnLimit".into(), json!(turn_limit));
        obj.insert("turns".into(), json!(turns));
    }
    Some(view)
}
}

pub(super) fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
}

pub(super) fn read_rows(paths: &[&str]) -> Vec<(i64, usize, usize, String)> {
    let mut rows: Vec<(i64, usize, usize, String)> = Vec::new();
    for (file_idx, path) in paths.iter().enumerate() {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        push_source_rows(&mut rows, file_idx, &raw);
    }
    rows
}

pub(super) fn order_merged(rows: &mut [(i64, usize, usize, String)]) {
    let files: std::collections::BTreeSet<usize> = rows.iter().map(|r| r.1).collect();
    if files.len() > 1 {
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    }
}

fn read_rows_from_sources(sources: &[Option<&str>]) -> Vec<(i64, usize, usize, String)> {
    let mut rows = Vec::new();
    for (file_idx, raw) in sources.iter().enumerate() {
        if let Some(raw) = raw {
            push_source_rows(&mut rows, file_idx, raw);
        }
    }
    rows
}

fn push_source_rows(rows: &mut Vec<(i64, usize, usize, String)>, file_idx: usize, raw: &str) {
    for (line_idx, line) in raw.split('\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let t = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("t").and_then(|x| x.as_i64()))
                .unwrap_or(0);
            rows.push((t, file_idx, line_idx, line.to_string()));
    }
}

fn value_to_buckets(v: &Value) -> Buckets {
    let g = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    Buckets {
        cache_miss: g("cacheMiss"),
        cache_hit: g("cacheHit"),
        cache_write: g("cacheWrite"),
        output: g("output"),
        reasoning: g("reasoning"),
    }
}

fn rates_json(r: &crate::pricing::Rates) -> Value {
    json!({
        "model": r.model, "label": r.label, "tier": r.tier, "tierLabel": r.tier_label,
        "currency": r.currency, "symbol": r.symbol, "unit": r.unit, "estimated": r.estimated,
        "cacheHit": r.cache_hit, "cacheMiss": r.cache_miss,
        "cacheWrite": r.cache_write, "output": r.output,
    })
}

fn price_table_json(t: &PriceTable) -> Value {
    let models: serde_json::Map<String, Value> = t
        .models
        .iter()
        .map(|(name, m)| {
            (
                name.clone(),
                json!({
                    "label": m.label,
                    "off": {
                        "cacheHit": m.cache_hit, "cacheMiss": m.cache_miss,
                        "cacheWrite": m.cache_write, "output": m.output,
                    }
                }),
            )
        })
        .collect();
    json!({
        "version": t.version,
        "currency": t.currency,
        "symbol": t.symbol,
        "unit": t.unit,
        "peakMultiplier": t.peak_multiplier,
        "peakWindows": t.peak_windows.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
        "peakWeekdays": t.peak_weekdays,
        "holidayCount": t.holidays.len(),
        "models": models,
    })
}

fn buckets_json(b: &Buckets) -> Value {
    json!({
        "cacheMiss": b.cache_miss,
        "cacheHit": b.cache_hit,
        "cacheWrite": b.cache_write,
        "output": b.output,
        "reasoning": b.reasoning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PriceTable;

    #[test]
    fn by_provider_splits_official_and_free() {
        // 两条不同 provider 的采样：官方 DeepSeek 与 opencode 免费模型
        let l = ledger_from(&[
            r#"{"k":"turn","sid":"s1","key":"group:1","seq":1,"t":1700000000000,"turn":1,"step":1,"retry":0,"model":"deepseek-v4.1-flash","provider":"deepseek","b":{"cacheMiss":1000,"output":500}}"#,
            r#"{"k":"turn","sid":"s2","key":"group:1","seq":1,"t":1700000000000,"turn":1,"step":1,"retry":0,"model":"deepseek-v4.1-flash-free","provider":"opencode","b":{"cacheMiss":2000,"output":1000}}"#,
        ]);
        let table = PriceTable::default();
        let out = l.by_provider(&table);
        assert_eq!(out["totalSamples"], 2);
        let rows = out["providers"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "两个 provider 要分成两行");
        let providers: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("provider").and_then(|p| p.as_str()))
            .collect();
        assert!(providers.contains(&"deepseek"));
        assert!(providers.contains(&"opencode"));
        // tokens.in 是三类输入之和（miss + hit + write）
        let ds = rows
            .iter()
            .find(|r| r["provider"] == "deepseek")
            .unwrap();
        assert_eq!(ds["tokens"]["in"], 1000.0);
        assert_eq!(ds["tokens"]["out"], 500.0);
        assert_eq!(ds["samples"], 1);
        // models 里要能看出是哪个模型花的钱
        assert_eq!(ds["models"][0]["model"], "deepseek-v4.1-flash");
        // 峰谷两个字段都要在（具体金额取决于价目表，不在这里断言数值）
        assert!(ds["byTier"]["peak"].is_number());
        assert!(ds["byTier"]["off"].is_number());
    }
    fn ledger_from(lines: &[&str]) -> Ledger {
        let mut l = Ledger {
            file: "test".into(),
            ..Default::default()
        };
        for line in lines {
            l.journal_lines += 1;
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if !l.apply(&v) {
                    l.dropped_lines += 1;
                }
            } else {
                l.dropped_lines += 1;
            }
        }
        l
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let l = ledger_from(&[
            r#"{"k":"turn","t":1,"sid":"s1","turn":1,"step":1,"retry":0,"seq":10,"b":{"output":5}}"#,
            r#"{"k":"turn","t":2,"sid":"s1","tu"#,
        ]);
        assert_eq!(l.samples.len(), 1, "好行必须留下");
        assert_eq!(l.dropped_lines, 1, "坏行要计数但不影响整体");
    }

    #[test]
    fn equal_seq_must_not_overwrite() {
        // 重连时快照重放同样的 seq；如果放行覆盖，旧快照能把新数据改小
        let l = ledger_from(&[
            r#"{"k":"turn","t":1,"sid":"s1","turn":1,"step":1,"retry":0,"seq":10,"b":{"output":100}}"#,
            r#"{"k":"turn","t":1,"sid":"s1","turn":1,"step":1,"retry":0,"seq":10,"b":{"output":1}}"#,
        ]);
        let s = l.samples.values().next().unwrap();
        assert_eq!(s.buckets.output, 100.0, "相等 seq 不得覆盖");
    }

    #[test]
    fn newer_seq_does_overwrite() {
        let l = ledger_from(&[
            r#"{"k":"turn","t":1,"sid":"s1","turn":1,"step":1,"retry":0,"seq":10,"b":{"output":100}}"#,
            r#"{"k":"turn","t":2,"sid":"s1","turn":1,"step":1,"retry":0,"seq":11,"b":{"output":250}}"#,
        ]);
        let s = l.samples.values().next().unwrap();
        assert_eq!(s.buckets.output, 250.0);
    }

    #[test]
    fn retry_is_part_of_the_sample_key() {
        // 同一个 turn/step，重试是不同的采样，不能互相覆盖
        let l = ledger_from(&[
            r#"{"k":"turn","t":1,"sid":"s1","turn":1,"step":1,"retry":0,"seq":10,"b":{"output":100}}"#,
            r#"{"k":"turn","t":2,"sid":"s1","turn":1,"step":1,"retry":1,"seq":11,"b":{"output":7}}"#,
        ]);
        assert_eq!(l.samples.len(), 2);
    }

    #[test]
    fn old_baseline_must_not_override_newer() {
        let l = ledger_from(&[
            r#"{"k":"base","t":1,"sid":"s1","seq":50,"b":{"cacheMiss":1000}}"#,
            r#"{"k":"base","t":2,"sid":"s1","seq":10,"b":{"cacheMiss":1}}"#,
        ]);
        let b = l.baselines.get("s1").unwrap();
        assert_eq!(b["seq"], 50);
        assert_eq!(b["buckets"]["cacheMiss"], 1000.0);
    }

    #[test]
    fn recent_is_sorted_desc_and_priced() {
        let l = ledger_from(&[
            r#"{"k":"turn","t":1000,"sid":"s1","turn":1,"step":1,"retry":0,"seq":1,"model":"deepseek-flash","b":{"cacheMiss":1000000}}"#,
            r#"{"k":"turn","t":3000,"sid":"s1","turn":2,"step":1,"retry":0,"seq":2,"model":"deepseek-flash","b":{"output":1000000}}"#,
        ]);
        let table = PriceTable::default();
        let r = l.recent(10, &table);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["time"], 3000, "最新的在最前");
        assert!(r[0]["cost"].as_f64().unwrap() > 0.0);
        assert!(r[0]["tier"].is_string());
    }

    #[test]
    fn recent_respects_limit() {
        let lines: Vec<String> = (0..5)
            .map(|i| {
                format!(
                    r#"{{"k":"turn","t":{},"sid":"s1","turn":{},"step":1,"retry":0,"seq":{},"b":{{"output":1}}}}"#,
                    i * 10,
                    i,
                    i + 1
                )
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let l = ledger_from(&refs);
        assert_eq!(l.recent(2, &PriceTable::default()).len(), 2);
    }

    #[test]
    fn explicit_source_loader_is_pure_and_uses_the_supplied_session_map() {
        let js = r#"{"k":"turn","t":1000,"sid":"sid-js","key":"group:1","turn":1,"step":1,"retry":0,"seq":1,"b":{"output":5}}"#;
        let rust = r#"{"k":"turn","t":1500,"sid":"sid-rust","key":"internal:historical","turn":2,"step":1,"retry":0,"seq":2,"b":{"output":11}}"#;
        let sessions = r#"{"sessions":{"group:2":{"sessionId":"sid-rust"}}}"#;
        let ledger = Ledger::load_all_from_sources(Some(js), Some(rust), Some(sessions));

        assert_eq!(ledger.journal_lines, 2);
        assert_eq!(ledger.dropped_lines, 0);
        assert_eq!(ledger.samples.len(), 2);
        assert_eq!(ledger.session_keys.get("sid-rust").map(String::as_str), Some("internal:historical"));
        assert_eq!(ledger.key_of("sid-rust").as_deref(), Some("group:2"));
        assert_eq!(ledger.file, "data/token-usage.jsonl");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-ledger-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        )));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct RuntimeLedgerRoot(std::path::PathBuf);

    impl Drop for RuntimeLedgerRoot {
        fn drop(&mut self) {
            let Ok(temp_root) = std::fs::canonicalize(std::env::temp_dir()) else {
                return;
            };
            let Ok(root) = std::fs::canonicalize(&self.0) else {
                return;
            };
            if root.parent() != Some(temp_root.as_path())
                || !root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("her-ledger-"))
            {
                return;
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn runtime_ledger_root(tag: &str) -> (RuntimeLedgerRoot, RuntimeContext) {
        let root = RuntimeLedgerRoot(temp_dir(tag));
        std::fs::create_dir_all(root.0.join("config")).unwrap();
        std::fs::create_dir_all(root.0.join("data")).unwrap();
        std::fs::create_dir_all(root.0.join("resources")).unwrap();
        std::fs::write(
            root.0.join("config/config.json"),
            br#"{"consoleToken":"fixture-console-token-abcdefghij"}"#,
        )
        .unwrap();
        std::fs::write(root.0.join("data/social-v2.json"), b"{}")
            .unwrap();
        std::fs::write(
            root.0.join("dependencies.json"),
            br#"{"dsh":"http://127.0.0.1:49101","model":"http://127.0.0.1:49102","onebot":"http://127.0.0.1:49103"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).expect("synthetic runtime root");
        (root, context)
    }

    #[test]
    fn isolated_runtime_loader_merges_selected_sources_and_maps_selected_sessions() {
        let (root_a, context_a) = runtime_ledger_root("selected A 中文 space");
        let (root_b, context_b) = runtime_ledger_root("decoy B 中文 space");

        let a_js = root_a.0.join("data/token-usage.jsonl");
        let a_rust = root_a.0.join("data/token-usage-rust.jsonl");
        let a_sessions = root_a.0.join("data/sessions.json");
        let b_js = root_b.0.join("data/token-usage.jsonl");
        let b_rust = root_b.0.join("data/token-usage-rust.jsonl");
        let b_sessions = root_b.0.join("data/sessions.json");

        std::fs::write(
            &a_js,
            concat!(
                r#"{"k":"turn","t":100,"sid":"sid-shared","key":"internal:historical","turn":1,"step":1,"retry":0,"seq":1,"model":"fixture-model","b":{"output":5}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &a_rust,
            concat!(
                r#"{"k":"turn","t":200,"sid":"sid-shared","turn":2,"step":1,"retry":0,"seq":2,"model":"fixture-model","b":{"output":11}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &a_sessions,
            br#"{"sessions":{"group:101":{"sessionId":"sid-shared"}}}"#,
        )
        .unwrap();

        std::fs::write(
            &b_js,
            concat!(
                r#"{"k":"turn","t":900,"sid":"sid-shared","key":"internal:decoy","turn":11,"step":1,"retry":0,"seq":11,"model":"decoy-model","b":{"output":91}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &b_rust,
            concat!(
                r#"{"k":"turn","t":1000,"sid":"sid-shared","turn":12,"step":1,"retry":0,"seq":12,"model":"decoy-model","b":{"output":92}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &b_sessions,
            br#"{"sessions":{"private:202":{"sessionId":"sid-shared"}}}"#,
        )
        .unwrap();

        let a_before = [
            std::fs::read(&a_js).unwrap(),
            std::fs::read(&a_rust).unwrap(),
            std::fs::read(&a_sessions).unwrap(),
        ];
        let b_before = [
            std::fs::read(&b_js).unwrap(),
            std::fs::read(&b_rust).unwrap(),
            std::fs::read(&b_sessions).unwrap(),
        ];

        let ledger_a = Ledger::load_all_for_runtime(&context_a).unwrap();
        assert_eq!(ledger_a.journal_lines, 2);
        assert_eq!(ledger_a.dropped_lines, 0);
        assert_eq!(ledger_a.samples.len(), 2, "A merges exactly its JS and Rust rows");
        assert!(ledger_a.samples.values().all(|sample| sample.model == "fixture-model"));
        assert!(ledger_a
            .samples
            .values()
            .any(|sample| sample.turn == 1 && sample.buckets.output == 5.0));
        assert!(ledger_a
            .samples
            .values()
            .any(|sample| sample.turn == 2 && sample.buckets.output == 11.0));
        assert_eq!(
            ledger_a.session_keys.get("sid-shared").map(String::as_str),
            Some("internal:historical"),
            "historical journal key remains available as a fallback"
        );
        assert_eq!(ledger_a.key_of("sid-shared").as_deref(), Some("group:101"));
        assert_eq!(
            ledger_a.recent(1, &PriceTable::default())[0]["key"],
            "group:101",
            "the newest row without an embedded key uses the selected active session map"
        );
        assert_eq!(
            [
                std::fs::read(&b_js).unwrap(),
                std::fs::read(&b_rust).unwrap(),
                std::fs::read(&b_sessions).unwrap(),
            ],
            b_before,
            "loading A must not mutate B's selected inputs"
        );

        let ledger_b = Ledger::load_all_for_runtime(&context_b).unwrap();
        assert_eq!(ledger_b.journal_lines, 2);
        assert_eq!(ledger_b.dropped_lines, 0);
        assert_eq!(ledger_b.samples.len(), 2, "B merges exactly its own JS and Rust rows");
        assert!(ledger_b.samples.values().all(|sample| sample.model == "decoy-model"));
        assert!(ledger_b
            .samples
            .values()
            .any(|sample| sample.turn == 11 && sample.buckets.output == 91.0));
        assert!(ledger_b
            .samples
            .values()
            .any(|sample| sample.turn == 12 && sample.buckets.output == 92.0));
        assert_eq!(ledger_b.key_of("sid-shared").as_deref(), Some("private:202"));
        assert_eq!(
            ledger_b.recent(1, &PriceTable::default())[0]["key"],
            "private:202"
        );
        assert_eq!(
            [
                std::fs::read(&a_js).unwrap(),
                std::fs::read(&a_rust).unwrap(),
                std::fs::read(&a_sessions).unwrap(),
            ],
            a_before,
            "loading B must leave A's selected inputs unchanged"
        );
        assert_eq!(
            [
                std::fs::read(&b_js).unwrap(),
                std::fs::read(&b_rust).unwrap(),
                std::fs::read(&b_sessions).unwrap(),
            ],
            b_before,
            "ledger loading is read-only for selected inputs"
        );
    }

    #[test]
    fn rust_writes_its_own_ledger_not_the_js_one() {
        // TD-1：两个进程各写各的文件，不能被改回同一个路径（否则 append 又会交错）
        assert_ne!(RUST_LEDGER_FILE, LEDGER_FILE);
        assert!(
            RUST_LEDGER_FILE.ends_with("/token-usage-rust.jsonl"),
            "Rust 必须写自己的 token-usage-rust.jsonl"
        );
        assert_eq!(LEDGER_FILES, [LEDGER_FILE, RUST_LEDGER_FILE], "读账本必须同时覆盖两份");
    }

    #[test]
    fn merged_load_reads_both_ledgers_in_ts_order() {
        let dir = temp_dir("merged");
        let js = dir.join("token-usage.jsonl");
        let rust = dir.join("token-usage-rust.jsonl");
        // 同一个采样键 (s1, turn2, step1, retry0)、seq 都等于 7（重连快照重放）。
        // JS 那份 ts 更晚（2000），Rust 那份 ts 更早（1500）。
        // 按 ts 排序后重放 → Rust 的先到并留下；如果直接"先读 JS 再读 Rust"拼接，
        // JS 的会先占位，结果反过来。这样断言能守住"按 ts 排序"这条要求。
        std::fs::write(
            &js,
            concat!(
                r#"{"k":"turn","t":1000,"sid":"s1","key":"group:1","turn":1,"step":1,"retry":0,"seq":1,"b":{"output":5}}"#,
                "\n",
                r#"{"k":"turn","t":2000,"sid":"s1","key":"group:1","turn":2,"step":1,"retry":0,"seq":7,"b":{"output":99}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &rust,
            concat!(
                r#"{"k":"turn","t":1500,"sid":"s1","key":"group:1","turn":2,"step":1,"retry":0,"seq":7,"b":{"output":11}}"#,
                "\n"
            ),
        )
        .unwrap();

        let l = Ledger::load_paths(&[js.to_str().unwrap(), rust.to_str().unwrap()]);
        assert_eq!(l.journal_lines, 3, "两份文件的行都要读进来");
        assert_eq!(l.samples.len(), 2, "turn1 + 合并后的 turn2");
        let turn2 = l.samples.values().find(|s| s.turn == 2).unwrap();
        assert_eq!(turn2.buckets.output, 11.0, "ts 更早的记录先重放（按 ts 排序）");

        // 只读单份时仍然只看那一份（不能让合并逻辑污染既有单文件语义）
        let only_js = Ledger::load(js.to_str().unwrap());
        assert_eq!(only_js.samples.len(), 2);
        let turn2_js = only_js.samples.values().find(|s| s.turn == 2).unwrap();
        assert_eq!(turn2_js.buckets.output, 99.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hf84_legacy_notice_sync_errors_are_returned_without_syncing_ordinary_appends() {
        let dir = temp_dir("notice-sync");
        let path = dir.join("notices.jsonl");
        let path = path.to_str().unwrap();
        crate::runtime::APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(|stage, _| {
            assert_eq!(stage, "sync");
            Err("injected notification sync failure".into())
        })));
        let ordinary = append_record_to(path, &json!({"k":"turn"}));
        let claims: Vec<_> = ["pause_notified", "resolution_notified"].iter()
            .map(|kind| append_record_to_durable(path, &json!({"k":kind}))).collect();
        crate::runtime::APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        ordinary.expect("ordinary ledger appends retain their existing durability");
        for claim in claims {
            assert_eq!(claim, Err("injected notification sync failure".into()));
        }
        append_record_to_durable(path, &json!({"k":"pause_notified"})).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_200_appends_are_parseable_and_counted() {
        let dir = temp_dir("concurrent");
        let path = dir.join("token-usage-rust.jsonl");
        let path = path.to_string_lossy().to_string();

        let mut handles = Vec::with_capacity(200);
        for i in 0..200i64 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                let record = json!({
                    "k": "turn",
                    "t": 1_700_000_000_000i64 + i,
                    "sid": format!("sid-{i}"),
                    "key": "group:1",
                    "seq": i + 1,
                    "turn": i,
                    "step": 1,
                    "retry": 0,
                    "model": "deepseek-flash",
                    "provider": "deepseek",
                    "b": { "cacheMiss": 10, "cacheHit": 0, "cacheWrite": 0, "output": 5, "reasoning": 0 },
                });
                append_record_to(&p, &record).expect("append 不能失败");
            }));
        }
        for h in handles {
            h.join().expect("append 线程不允许 panic");
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 200, "200 次并发 append 必须正好 200 行（无交错覆盖）");
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("第 {} 行不可解析：{e}；原文：{line}", i + 1));
        }

        let ledger = Ledger::load(&path);
        assert_eq!(ledger.journal_lines, 200);
        assert_eq!(ledger.dropped_lines, 0, "写出去的每一行都要能被账本读回");
        assert_eq!(ledger.samples.len(), 200, "200 条采样一条都不能丢");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pure_compaction_folds_oldest_samples_into_evict_preserving_totals_and_base_coverage() {
        let table = PriceTable::default();
        let b = json!({"cacheMiss": 50, "cacheHit": 0, "cacheWrite": 0, "output": 5});
        let b10 = json!({"cacheMiss": 100, "cacheHit": 0, "cacheWrite": 0, "output": 10});
        let b40 = json!({"cacheMiss": 40, "cacheHit": 10, "cacheWrite": 0, "output": 8});
        let b20 = json!({"cacheMiss": 20, "cacheHit": 0, "cacheWrite": 0, "output": 4});
        let b30 = json!({"cacheMiss": 30, "cacheHit": 0, "cacheWrite": 0, "output": 6});
        let rows = vec![
            json!({"k":"base","t":100,"sid":"s1","seq":2,"key":"group:1",
                "model":"deepseek-flash","provider":"official","b":b})
            .to_string(),
            json!({"k":"turn","t":110,"sid":"s1","seq":1,"turn":1,"step":0,"retry":0,
                "key":"group:1","model":"deepseek-flash","provider":"official","b":b})
            .to_string(),
            json!({"k":"turn","t":200,"sid":"s1","seq":3,"turn":2,"step":0,"retry":0,
                "key":"group:1","model":"deepseek-flash","provider":"official",
                "engine":"harness","route":"r-a","job":"turn","attemptId":"att-1","b":b10})
            .to_string(),
            json!({"k":"turn","t":300,"sid":"s2","seq":0,"turn":1,"step":0,"retry":0,
                "key":"group:2","model":"deepseek-flash","provider":"harness",
                "engine":"harness","route":"r-b","job":"summary","b":b40})
            .to_string(),
            json!({"k":"turn","t":400,"sid":"s1","seq":4,"turn":3,"step":0,"retry":0,
                "key":"group:1","model":"deepseek-flash","provider":"official",
                "engine":"harness","route":"r-a","job":"turn","attemptId":"att-2","b":b20})
            .to_string(),
            json!({"k":"turn","t":500,"sid":"s2","seq":0,"turn":2,"step":0,"retry":0,
                "key":"group:2","model":"deepseek-flash","provider":"harness",
                "engine":"harness","route":"r-b","job":"turn","attemptId":"att-3","b":b30})
            .to_string(),
        ];
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let ledger = ledger_from(&refs);
        let before = ledger.summary(&table, 50);
        // Keep newest 2 samples (t=400, t=500); evict t=110 (covered), t=200 (live), t=300 (live).
        let compacted = ledger.compact_in_memory(&table, 2, 1_000);
        let after = compacted.summary(&table, 50);

        assert_eq!(compacted.samples.len(), 2);
        assert_eq!(after["totals"]["buckets"], before["totals"]["buckets"]);
        assert!(
            (after["totals"]["cost"].as_f64().unwrap()
                - before["totals"]["cost"].as_f64().unwrap())
            .abs()
                < 1e-12
        );
        assert_eq!(
            compacted.samples[&("s1".into(), 3, 0, 0)].attempt_id.as_deref(),
            Some("att-2")
        );
        assert_eq!(
            compacted.samples[&("s2".into(), 2, 0, 0)].route.as_deref(),
            Some("r-b")
        );
        // Re-compacting again preserves totals identically.
        let again = compacted.compact_in_memory(&table, 1, 2_000);
        let after_again = again.summary(&table, 50);
        assert_eq!(again.samples.len(), 1);
        assert_eq!(after_again["totals"]["buckets"], before["totals"]["buckets"]);
        assert!(
            (after_again["totals"]["cost"].as_f64().unwrap()
                - before["totals"]["cost"].as_f64().unwrap())
            .abs()
                < 1e-12
        );
    }

    #[test]
    fn ledger_storage_compacts_file_and_recovers_interrupted_sidecar_and_reset() {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-ledger-compact-{}",
            crate::turn_ledger::mint_id("")
        )));
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());
        for sub in ["config", "data", "resources"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(
            dir.join("config/config.json"),
            br#"{"consoleToken":"fixture-compact-token-123456"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("data/social-v2.json"), b"{}").unwrap();
        let deps = json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:2",
            "onebot": "http://127.0.0.1:3",
        });
        std::fs::write(dir.join("dependencies.json"), deps.to_string()).unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        let table = PriceTable::default();

        for i in 0..8i64 {
            let row = json!({
                "k": "turn",
                "t": 1_700_000_000_000i64 + i,
                "sid": "harness-p/group:1",
                "seq": 0,
                "turn": i + 1,
                "step": 0,
                "retry": 0,
                "key": "group:1",
                "model": "deepseek-flash",
                "provider": "harness",
                "engine": "harness",
                "route": "primary",
                "job": "turn",
                "attemptId": format!("att-{i}"),
                "b": { "cacheMiss": 10, "cacheHit": 0, "cacheWrite": 0, "output": 5 },
            });
            LedgerStorage::append_record_for_runtime(&context, &row).unwrap();
        }
        let before = LedgerStorage::load_all_for_runtime(&context)
            .unwrap()
            .summary(&table, 50);
        // Below threshold: no rewrite.
        assert!(!LedgerStorage::compact_for_runtime(&context, &table, 9, 3).unwrap());
        // At threshold: compacts to 3 kept samples + 1 evict row.
        assert!(LedgerStorage::compact_for_runtime(&context, &table, 8, 3).unwrap());
        assert!(!dir.join("data/token-usage-rust.jsonl.compact").exists());
        let raw = std::fs::read_to_string(dir.join("data/token-usage-rust.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 4, "1 evict + 3 kept samples");
        let after = LedgerStorage::load_all_for_runtime(&context)
            .unwrap()
            .summary(&table, 50);
        assert_eq!(after["totals"]["buckets"], before["totals"]["buckets"]);

        // Simulate crash between truncate and write completion: empty main file + sidecar.
        std::fs::write(dir.join("data/token-usage-rust.jsonl.compact"), &raw).unwrap();
        std::fs::write(dir.join("data/token-usage-rust.jsonl"), b"").unwrap();
        let recovered = LedgerStorage::load_all_for_runtime(&context)
            .unwrap()
            .summary(&table, 50);
        assert_eq!(recovered["totals"]["buckets"], before["totals"]["buckets"]);

        // Reset removes both ledgers and sidecar cleanly.
        LedgerStorage::reset(&context).unwrap();
        assert_eq!(std::fs::read(dir.join("data/token-usage-rust.jsonl")).unwrap(), b"");
        assert!(!dir.join("data/token-usage-rust.jsonl.compact").exists());
    }

    #[test]
    fn pure_attribution_and_reconciliation_planner_match_contract() {
        let table = PriceTable::default();
        let rows = vec![
            json!({"k":"turn","t":1_700_000_000_000i64,"sid":"dsh-1","seq":1,"turn":1,"step":1,
                "retry":0,"key":"group:1","model":"deepseek-flash","provider":"official",
                "b":{"cacheMiss":100,"cacheHit":0,"cacheWrite":0,"output":10}})
            .to_string(),
            json!({"k":"turn","t":1_700_000_001_000i64,"sid":"h/group:1","seq":0,"turn":10,
                "step":0,"retry":0,"key":"group:1","model":"deepseek-flash","provider":"harness",
                "engine":"harness","route":"r1","job":"turn","attemptId":"att-known",
                "b":{"cacheMiss":50,"cacheHit":0,"cacheWrite":0,"output":5}})
            .to_string(),
            json!({"k":"turn","t":1_700_000_002_000i64,"sid":"h/group:1","seq":0,"turn":11,
                "step":0,"retry":0,"key":"group:1","model":"deepseek-flash","provider":"harness",
                "engine":"harness","route":"r1","job":"turn",
                "b":{"cacheMiss":25,"cacheHit":0,"cacheWrite":0,"output":3}})
            .to_string(),
        ];
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let l = ledger_from(&refs);
        let start_day = crate::pricing::beijing_parts(1_700_000_000_000).2;
        let attr = l.dsh_attribution(&table, &start_day, 1_700_000_005_000);
        assert_eq!(attr.samples, 1, "only DSH sample enters attribution");
        let resp = attr.to_response(
            7,
            &start_day,
            &start_day,
            &table,
            DshTotals {
                turns: 3,
                tokens: 210.0,
                cost: 0.0,
            },
        );
        assert_eq!(resp["unattributed"]["turns"], 2);

        let b_legacy = Buckets {
            cache_miss: 25.0,
            cache_hit: 0.0,
            cache_write: 0.0,
            output: 3.0,
            reasoning: 0.0,
        };
        let candidates = vec![
            ReconcileCandidate {
                attempt_id: "att-known".into(),
                key: "group:1".into(),
                model: "deepseek-flash".into(),
                ended_ms: 1_700_000_001_000,
                buckets: None,
            },
            ReconcileCandidate {
                attempt_id: "att-legacy-1".into(),
                key: "group:1".into(),
                model: "deepseek-flash".into(),
                ended_ms: 1_700_000_002_500,
                buckets: Some(b_legacy),
            },
            ReconcileCandidate {
                attempt_id: "att-legacy-dup".into(),
                key: "group:1".into(),
                model: "deepseek-flash".into(),
                ended_ms: 1_700_000_002_600,
                buckets: Some(b_legacy),
            },
        ];
        // att-known skipped by attemptId; att-legacy-1 matched 1-to-1; att-legacy-dup missing.
        assert_eq!(plan_harness_reconciliation(&l, &candidates), vec![2]);
    }
}

/// 写入规则：只留下账本里还没有、或序号更新的采样与基线（重放、重连、恢复不会重复记账）。
/// 同一批内部也按同一规则去重。返回应当追加的记录（保持输入顺序）。
pub fn plan_appends(existing: &Ledger, candidates: &[Value]) -> Vec<Value> {
    let mut sample_seq: HashMap<(String, i64, i64, i64), i64> =
        existing.samples.iter().map(|(k, s)| (k.clone(), s.seq)).collect();
    let mut base_state: HashMap<String, (i64, Value)> = existing
        .baselines
        .iter()
        .map(|(sid, b)| {
            (
                sid.clone(),
                (b.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1), b.get("buckets").cloned().unwrap_or(Value::Null)),
            )
        })
        .collect();
    let mut out = Vec::new();
    for c in candidates {
        match c.get("k").and_then(|k| k.as_str()) {
            Some("turn") => {
                // 无明细占位：只作证据，不计入采样，每次都写。
                if c.get("b").map_or(false, Value::is_null) {
                    out.push(c.clone());
                    continue;
                }
                let (Some(sid), Some(turn), Some(step)) =
                    (c.get("sid").and_then(|v| v.as_str()), c.get("turn").and_then(|v| v.as_i64()), c.get("step").and_then(|v| v.as_i64()))
                else {
                    continue;
                };
                let retry = c.get("retry").and_then(|v| v.as_i64()).unwrap_or(0);
                let seq = c.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
                let k = (sid.to_string(), turn, step, retry);
                if let Some(prev) = sample_seq.get(&k) {
                    // 幂等：只有序号严格更大才覆盖（与 `apply` 同规则）。
                    if seq >= 0 && *prev >= seq {
                        continue;
                    }
                }
                sample_seq.insert(k, seq);
                out.push(c.clone());
            }
            Some("base") => {
                let Some(sid) = c.get("sid").and_then(|v| v.as_str()) else { continue };
                let seq = c.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
                let buckets = buckets_json(&normalize_usage(c.get("b")));
                if let Some((prev_seq, prev_buckets)) = base_state.get(sid) {
                    // 旧基线不得覆盖新基线；同序号且内容未变的重复基线（重连重发）不再落盘。
                    if *prev_seq > seq || (*prev_seq == seq && *prev_buckets == buckets) {
                        continue;
                    }
                }
                base_state.insert(sid.to_string(), (seq, buckets));
                out.push(c.clone());
            }
            _ => {}
        }
    }
    out
}

/// 把快照里的历史采样与 `tokenUsage` 基线转成账本行（字段与 `turn_records` / JS `appendLine` 一致）。
/// `key` 为空时不写 key 字段（与 JS 的 undefined 一致）。
pub fn harvest_candidates(
    key: Option<&str>,
    sid: &str,
    harvest: &crate::dsh::UsageHarvest,
    now: i64,
) -> Vec<Value> {
    fn row(mut v: Value, key: Option<&str>, model: &Option<String>, provider: &Option<String>) -> Value {
        if let Some(obj) = v.as_object_mut() {
            if let Some(k) = key {
                obj.insert("key".into(), json!(k));
            }
            if let Some(m) = model {
                obj.insert("model".into(), json!(m));
            }
            if let Some(p) = provider {
                obj.insert("provider".into(), json!(p));
            }
        }
        v
    }
    let mut rows = Vec::new();
    for s in &harvest.samples {
        rows.push(row(
            json!({"k": "turn", "t": s.t, "sid": sid, "seq": s.seq, "turn": s.turn, "step": s.step, "retry": s.retry, "b": s.usage}),
            key,
            &s.model,
            &s.provider,
        ));
    }
    if let Some(base) = &harvest.base {
        rows.push(row(
            json!({"k": "base", "t": now, "sid": sid, "seq": base.as_of_seq, "b": base.usage}),
            key,
            &base.model,
            &base.provider,
        ));
    }
    rows
}

/// Pure attribution aggregate across DSH-comparable samples (`/api/usage/attribution`).
#[derive(Debug, Default, Clone)]
pub struct AttributionAgg {
    pub turns: HashSet<(String, i64)>,
    pub tokens: f64,
    pub cost: f64,
}

impl AttributionAgg {
    fn add(&mut self, sid: &str, turn: i64, tokens: f64, cost: f64) {
        self.turns.insert((sid.to_string(), turn));
        self.tokens += tokens;
        self.cost += cost;
    }

    fn to_json(&self) -> Value {
        json!({
            "turns": self.turns.len() as i64,
            "tokens": self.tokens,
            "cost": self.cost,
        })
    }
}

/// Pure bridge vs. DSH usage attribution summary.
#[derive(Debug, Default, Clone)]
pub struct DshAttribution {
    pub our: AttributionAgg,
    pub samples: i64,
    pub by_key: BTreeMap<String, AttributionAgg>,
    pub by_provider: BTreeMap<String, AttributionAgg>,
}

/// DSH total summary passed into `DshAttribution::to_response`.
#[derive(Debug, Clone, Copy)]
pub struct DshTotals {
    pub turns: i64,
    pub tokens: f64,
    pub cost: f64,
}

impl DshAttribution {
    /// Combine bridge attribution with `.dshw-usage.json` totals for `/api/usage/attribution`.
    pub fn to_response(
        &self,
        days: i64,
        today: &str,
        start_day: &str,
        table: &PriceTable,
        dsh: DshTotals,
    ) -> Value {
        let token_sort_key = |v: &Value| v.get("tokens").and_then(|t| t.as_f64()).unwrap_or(0.0);
        let mut by_key_rows: Vec<Value> = self
            .by_key
            .iter()
            .map(|(k, a)| {
                let mut row = a.to_json();
                row["key"] = json!(k);
                row
            })
            .collect();
        by_key_rows.sort_by(|a, b| {
            token_sort_key(b)
                .partial_cmp(&token_sort_key(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut by_provider_rows: Vec<Value> = self
            .by_provider
            .iter()
            .map(|(p, a)| {
                let mut row = a.to_json();
                row["provider"] = json!(p);
                row
            })
            .collect();
        by_provider_rows.sort_by(|a, b| {
            token_sort_key(b)
                .partial_cmp(&token_sort_key(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let unattributed_turns = dsh.turns - self.samples;
        let unattributed_tokens = dsh.tokens - self.our.tokens;
        let unattributed_cost = dsh.cost - self.our.cost;
        let note = "DSH 总账（.dshw-usage.json）减去桥接账本（token-usage.jsonl 里带 key 的记录）。\
差额没有被吞掉：可能是 Web GUI / CLI 探针等没有 QQ 会话归属的调用，也可能是回合刚跑完、\
总账尚未落盘（此时为负）。dshTotal.turns 与 unattributed.turns 都按 LLM 请求计（总账事件\
条数减去账本里带 key 的 usage 采样数 our.samples）；our.turns 是账本里的对话轮数，不参与相减。";
        json!({
            "ok": true,
            "days": days,
            "symbol": table.symbol,
            "window": { "fromDay": start_day, "toDay": today },
            "our": {
                "turns": self.our.turns.len() as i64,
                "tokens": self.our.tokens,
                "cost": self.our.cost,
                "samples": self.samples,
            },
            "dshTotal": {
                "turns": dsh.turns,
                "tokens": dsh.tokens,
                "cost": dsh.cost,
                "events": dsh.turns,
            },
            "unattributed": {
                "turns": unattributed_turns,
                "tokens": unattributed_tokens,
                "cost": unattributed_cost,
                "note": note,
            },
            "byKey": by_key_rows,
            "byProvider": by_provider_rows,
        })
    }
}

/// Pure output of token ledger compaction (`Ledger::compact_plan` / `compact_jsonl`).
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    #[cfg(test)]
    pub records: Vec<Value>,
    pub jsonl: String,
    pub evicted_attempt_ids: Vec<String>,
    pub compacted: bool,
}

#[derive(Default)]
struct EvictAccum {
    key: Option<String>,
    t: i64,
    buckets: Buckets,
    cost: f64,
    peak: f64,
    off: f64,
    estimated: bool,
    dropped: i64,
    max_seq: i64,
}

/// Pure candidate metadata for reconciling `conversations.db` completed attempts into the ledger.
#[derive(Clone, Debug)]
pub struct ReconcileCandidate {
    pub attempt_id: String,
    pub key: String,
    pub model: String,
    pub ended_ms: i64,
    pub buckets: Option<Buckets>,
}

/// Pure startup crash-reconciliation planner: returns the indices of `candidates` not yet
/// recorded in `existing`, matching by `attemptId` first and 1-to-1 legacy `(key, model, buckets,
/// timestamp)` when `attemptId` was absent pre-upgrade.
pub fn plan_harness_reconciliation(
    existing: &Ledger,
    candidates: &[ReconcileCandidate],
) -> Vec<usize> {
    let legacy_pool: Vec<&Sample> = existing
        .sample_order
        .iter()
        .filter_map(|sk| existing.samples.get(sk))
        .filter(|s| {
            s.attempt_id.is_none()
                && s.engine == crate::harness::usage::ENGINE
                && s.job.as_deref() == Some(crate::harness::usage::TURN)
        })
        .collect();
    let mut legacy_used = vec![false; legacy_pool.len()];
    let mut missing = Vec::new();
    for (idx, candidate) in candidates.iter().enumerate() {
        if existing.recorded_attempts.contains(&candidate.attempt_id) {
            continue;
        }
        if let Some(buckets) = candidate.buckets {
            if let Some((pos, _)) = legacy_pool.iter().enumerate().find(|(i, s)| {
                !legacy_used[*i]
                    && s.key.as_deref() == Some(candidate.key.as_str())
                    && s.model == candidate.model
                    && s.buckets == buckets
                    && (s.t - candidate.ended_ms).abs() <= 10_000
            }) {
                legacy_used[pos] = true;
                continue;
            }
        }
        missing.push(idx);
    }
    missing
}

impl Ledger {
    /// Pure attribution math over DSH-comparable samples from `start_day` (Beijing YYYY-MM-DD).
    pub fn dsh_attribution(
        &self,
        table: &PriceTable,
        start_day: &str,
        now: i64,
    ) -> DshAttribution {
        let mut out = DshAttribution::default();
        for s in self.dsh_comparable_samples() {
            let Some(key) = s
                .key
                .clone()
                .or_else(|| self.key_of(&s.session_id))
                .filter(|k| !k.is_empty())
            else {
                continue;
            };
            let at = if s.t != 0 { s.t } else { now };
            if crate::pricing::beijing_parts(at).2.as_str() < start_day {
                continue;
            }
            let (_, cost) = price_usage(&s.buckets, &s.model, at, table);
            let tokens = s.buckets.total();
            out.our.add(&s.session_id, s.turn, tokens, cost.total);
            let provider = if s.provider.is_empty() {
                "unknown".to_string()
            } else {
                s.provider.clone()
            };
            out.by_key
                .entry(key)
                .or_default()
                .add(&s.session_id, s.turn, tokens, cost.total);
            out.by_provider
                .entry(provider)
                .or_default()
                .add(&s.session_id, s.turn, tokens, cost.total);
            out.samples += 1;
        }
        out
    }

    /// Pure compaction math: retain the newest `keep_samples` samples and fold older live samples
    /// per session into `"evict"` summary records (matching DSH's `evict` / `base` semantics so
    /// totals, tiers, and conversation views stay invariant).
    pub fn compact_plan(&self, table: &PriceTable, keep_samples: usize, at: i64) -> CompactionPlan {
        let mut ordered: Vec<(usize, &Sample)> = self
            .sample_order
            .iter()
            .enumerate()
            .filter_map(|(idx, sk)| self.samples.get(sk).map(|s| (idx, s)))
            .collect();
        ordered.sort_by_key(|(idx, s)| (s.t, *idx));
        let evict_count = ordered.len().saturating_sub(keep_samples);
        let (to_evict, to_keep) = ordered.split_at(evict_count);

        let mut evicts: HashMap<String, EvictAccum> = HashMap::new();
        let mut evict_sids: Vec<String> = Vec::new();
        for sid in &self.evicted_order {
            let Some(gone) = self.evicted.get(sid) else {
                continue;
            };
            let covered = self.eviction_covered(sid, gone);
            let acc = EvictAccum {
                key: gone
                    .get("key")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.key_of(sid)),
                t: self.session_seen.get(sid).copied().unwrap_or(0),
                buckets: if covered {
                    Buckets::default()
                } else {
                    gone.get("buckets").map(value_to_buckets).unwrap_or_default()
                },
                cost: if covered {
                    0.0
                } else {
                    gone.get("cost").and_then(Value::as_f64).unwrap_or(0.0)
                },
                peak: if covered {
                    0.0
                } else {
                    gone.pointer("/byTier/peak")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                },
                off: if covered {
                    0.0
                } else {
                    gone.pointer("/byTier/off")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                },
                estimated: !covered
                    && gone
                        .get("estimated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                dropped: gone.get("dropped").and_then(Value::as_i64).unwrap_or(0),
                max_seq: gone.get("maxSeq").and_then(Value::as_i64).unwrap_or(-1),
            };
            evicts.insert(sid.clone(), acc);
            evict_sids.push(sid.clone());
        }

        let mut evicted_attempt_ids = Vec::new();
        for (_, s) in to_evict {
            if let Some(id) = &s.attempt_id {
                evicted_attempt_ids.push(id.clone());
            }
            if !evicts.contains_key(&s.session_id) {
                evict_sids.push(s.session_id.clone());
            }
            let acc = evicts.entry(s.session_id.clone()).or_insert_with(|| EvictAccum {
                key: s.key.clone().or_else(|| self.key_of(&s.session_id)),
                max_seq: -1,
                ..Default::default()
            });
            if acc.key.is_none() {
                acc.key = s.key.clone().or_else(|| self.key_of(&s.session_id));
            }
            acc.t = acc.t.max(s.t);
            acc.dropped += 1;
            acc.max_seq = acc.max_seq.max(s.seq);
            let is_live = match self.baselines.get(&s.session_id) {
                Some(b) => s.seq > b.get("seq").and_then(Value::as_i64).unwrap_or(-1),
                None => true,
            };
            if is_live {
                let sample_at = if s.t != 0 { s.t } else { at };
                let (rates, cost) = price_usage(&s.buckets, &s.model, sample_at, table);
                acc.buckets = acc.buckets.add(&s.buckets);
                acc.cost += cost.total;
                if tier_slot(&rates.tier) == 0 {
                    acc.peak += cost.total;
                } else {
                    acc.off += cost.total;
                }
                acc.estimated |= rates.estimated;
            }
        }

        let mut records = Vec::with_capacity(
            evict_sids.len() + self.base_order.len() + to_keep.len(),
        );
        for sid in evict_sids {
            let Some(acc) = evicts.get(&sid) else {
                continue;
            };
            let mut row = json!({
                "k": "evict",
                "t": acc.t,
                "sid": sid,
                "b": buckets_json(&acc.buckets),
                "cost": acc.cost,
                "peak": acc.peak,
                "off": acc.off,
                "estimated": acc.estimated,
                "dropped": acc.dropped,
                "maxSeq": acc.max_seq,
            });
            if let (Some(key), Some(obj)) = (&acc.key, row.as_object_mut()) {
                obj.insert("key".into(), json!(key));
            }
            records.push(row);
        }
        for sid in &self.base_order {
            let Some(base) = self.baselines.get(sid) else {
                continue;
            };
            let mut row = json!({
                "k": "base",
                "t": base.get("t").and_then(Value::as_i64).unwrap_or(0),
                "sid": sid,
                "seq": base.get("seq").and_then(Value::as_i64).unwrap_or(-1),
                "b": base.get("buckets").cloned().unwrap_or_else(|| json!({})),
            });
            if let Some(obj) = row.as_object_mut() {
                for field in ["key", "model", "provider"] {
                    if let Some(val) = base.get(field).filter(|v| !v.is_null()) {
                        obj.insert(field.into(), val.clone());
                    }
                }
            }
            records.push(row);
        }
        for (_, s) in to_keep {
            records.push(sample_to_record(s));
        }

        let jsonl: String = records.iter().map(|r| format!("{r}\n")).collect();
        let compacted = evict_count > 0 || self.journal_lines > records.len();
        CompactionPlan {
            #[cfg(test)]
            records,
            jsonl,
            evicted_attempt_ids,
            compacted,
        }
    }

    /// Pure in-memory compaction returning the compacted `Ledger` without touching disk.
    #[cfg(test)]
    pub fn compact_in_memory(
        &self,
        table: &PriceTable,
        keep_samples: usize,
        at: i64,
    ) -> Ledger {
        let plan = self.compact_plan(table, keep_samples, at);
        let mut out = Ledger {
            file: self.file.clone(),
            resolved: self.resolved.clone(),
            ..Default::default()
        };
        for record in &plan.records {
            out.journal_lines += 1;
            if !out.apply(record) {
                out.dropped_lines += 1;
            }
        }
        out
    }
}

fn sample_to_record(s: &Sample) -> Value {
    let mut row = json!({
        "k": "turn",
        "t": s.t,
        "sid": s.session_id,
        "seq": s.seq,
        "turn": s.turn,
        "step": s.step,
        "retry": s.retry,
        "model": s.model,
        "provider": s.provider,
        "b": buckets_json(&s.buckets),
    });
    if let Some(obj) = row.as_object_mut() {
        if let Some(key) = &s.key {
            obj.insert("key".into(), json!(key));
        }
        if !s.engine.is_empty() {
            obj.insert("engine".into(), json!(s.engine));
        }
        if let Some(route) = &s.route {
            obj.insert("route".into(), json!(route));
        }
        if let Some(job) = &s.job {
            obj.insert("job".into(), json!(job));
        }
        if let Some(attempt_id) = &s.attempt_id {
            obj.insert("attemptId".into(), json!(attempt_id));
        }
    }
    row
}

/// Pure JSONL-level compaction helper shared by `LedgerStorage`.
pub fn compact_jsonl(
    raw: &str,
    sessions_json: Option<&str>,
    table: &PriceTable,
    keep_samples: usize,
    at: i64,
) -> CompactionPlan {
    let ledger = Ledger::load_all_from_sources(None, Some(raw), sessions_json);
    ledger.compact_plan(table, keep_samples, at)
}

/// 读取运行时账本、按 `plan_appends` 过滤后追加到 Rust 自己的账本。返回实际写入的行数。
pub fn append_planned(context: &RuntimeContext, candidates: &[Value]) -> Result<usize, String> {
    LedgerStorage::append_planned(context, candidates)
}

#[cfg(test)]
mod writer_tests;

#[cfg(test)]
mod oracle_tests;
