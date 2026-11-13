//! 价目表与峰谷分时计费（对应 JS 的 `model-prices.js`）。
//!
//! 为什么桥接要自己算钱：DSH 只回 token 数，provider 也不报花费
//! （`dsh-llm-pi-ai` 明写 "no consumer reports spend"）。所以「花了多少钱」
//! 只能由消费方拿价目表换算。
//!
//! 口径（元 / 百万 token）：
//!   · 扣减 = Σ(各桶 token × 对应单价)
//!   · 三个互不重叠的输入桶：缓存命中 / 缓存未命中 / 缓存写，再加输出桶
//!   · 空闲单价 = 高峰单价 ÷ peakMultiplier（默认 2）
//!   · 高峰 = 北京时间周一至周五 9:00-12:00、14:00-18:00，且不是法定节假日
//!
//! 注意一个容易算错钱的地方：DSH 的 `inputTokens` 是**未命中**部分，不是总输入。
//! 把 cacheRead 当成 inputTokens 的一部分会导致重复计价。

use serde_json::Value;

/// 价目表版本，改了价目就改这里（控制台用它提示「你看到的是哪版价格」）。
pub const PRICE_TABLE_VERSION: &str = "2026-11-04";

/// 2026 年法定节假日（放假日，含调休连休段）。
pub const CN_HOLIDAYS_2026: &[&str] = &[
    "2026-01-01", "2026-01-02", "2026-01-03", "2026-02-15", "2026-02-16", "2026-02-17",
    "2026-02-18", "2026-02-19", "2026-02-20", "2026-02-21", "2026-02-22", "2026-02-23",
    "2026-04-04", "2026-04-05", "2026-04-06", "2026-05-01", "2026-05-02", "2026-05-03",
    "2026-05-04", "2026-05-05", "2026-06-19", "2026-06-20", "2026-06-21", "2026-09-25",
    "2026-09-26", "2026-09-27", "2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04",
    "2026-10-05", "2026-10-06", "2026-10-07",
];

/// 2027 年法定节假日。
///
/// 为什么要预置到 2027：缺日期时的表现是**静默高估** —— 节假日里的调用会被
/// 按高峰计价（2 倍），金额偏高但不报错。春节/国庆横跨多天，很容易把整年账单
/// 抬高几个百分点，而且没人会发现。
pub const CN_HOLIDAYS_2027: &[&str] = &[
    "2027-01-01", "2027-01-02", "2027-01-03", "2027-02-05", "2027-02-06", "2027-02-07",
    "2027-02-08", "2027-02-09", "2027-02-10", "2027-02-11", "2027-02-12", "2027-02-13",
    "2027-04-04", "2027-04-05", "2027-04-06", "2027-05-01", "2027-05-02", "2027-05-03",
    "2027-05-04", "2027-05-05", "2027-06-09", "2027-06-10", "2027-06-11", "2027-09-15",
    "2027-09-16", "2027-09-17", "2027-10-01", "2027-10-02", "2027-10-03", "2027-10-04",
    "2027-10-05", "2027-10-06", "2027-10-07",
];

const BEIJING_OFFSET_MINUTES: i64 = 8 * 60;

/// 四个互不重叠的计费桶（外加 reasoning，仅展示用）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Buckets {
    pub cache_miss: f64,
    pub cache_hit: f64,
    pub cache_write: f64,
    pub output: f64,
    pub reasoning: f64,
}

// total/add/sub 与 normalize_usage 是 token-ledger 的构件：
// 现在只接上了价目表这一层，账本聚合（P10-a 的 tokens 接口）还没搬过来。
// 标注 allow 而不是删掉，是因为删了下一轮又要重写一遍。
#[allow(dead_code)]
impl Buckets {
    pub fn total(&self) -> f64 {
        self.cache_miss + self.cache_hit + self.cache_write + self.output
    }
    pub fn add(&self, other: &Buckets) -> Buckets {
        Buckets {
            cache_miss: self.cache_miss + other.cache_miss,
            cache_hit: self.cache_hit + other.cache_hit,
            cache_write: self.cache_write + other.cache_write,
            output: self.output + other.output,
            reasoning: self.reasoning + other.reasoning,
        }
    }
    /// 逐桶下限为 0（用于「基线之后」的增量）。
    pub fn sub(&self, other: &Buckets) -> Buckets {
        let s = |a: f64, b: f64| (a - b).max(0.0);
        Buckets {
            cache_miss: s(self.cache_miss, other.cache_miss),
            cache_hit: s(self.cache_hit, other.cache_hit),
            cache_write: s(self.cache_write, other.cache_write),
            output: s(self.output, other.output),
            reasoning: s(self.reasoning, other.reasoning),
        }
    }
}

/// 单价（已按峰谷折算好）。
#[derive(Debug, Clone, PartialEq)]
pub struct Rates {
    pub model: String,
    pub label: String,
    pub tier: String,
    pub tier_label: String,
    pub currency: String,
    pub symbol: String,
    pub unit: f64,
    /// 模型不在价目表里、按兜底模型计价 —— 控制台要提示「这是估算」。
    pub estimated: bool,
    pub cache_hit: f64,
    pub cache_miss: f64,
    pub cache_write: f64,
    pub output: f64,
}

/// 分项金额与合计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub cache_hit: f64,
    pub cache_miss: f64,
    pub cache_write: f64,
    pub output: f64,
    pub total: f64,
}

/// 单个模型的价格（`off` 是空闲时段单价）。
#[derive(Debug, Clone)]
pub struct ModelPrice {
    pub label: String,
    pub cache_hit: f64,
    pub cache_miss: f64,
    pub cache_write: f64,
    pub output: f64,
}

/// 价目表。
#[derive(Debug, Clone)]
pub struct PriceTable {
    pub version: String,
    pub currency: String,
    pub symbol: String,
    pub unit: f64,
    pub peak_multiplier: f64,
    /// 高峰小时窗口，半开区间 [start, end)，按北京时间。
    pub peak_windows: Vec<(i64, i64)>,
    /// 高峰只在周一至周五（1=周一 … 7=周日）。
    pub peak_weekdays: Vec<i64>,
    pub holidays: Vec<String>,
    pub models: Vec<(String, ModelPrice)>,
    pub fallback_model: String,
}

impl Default for PriceTable {
    fn default() -> Self {
        let flash = ModelPrice {
            label: "DeepSeek-V4.1-Flash".into(),
            cache_hit: 0.02,
            cache_miss: 1.0,
            cache_write: 1.0,
            output: 4.0,
        };
        let pro = ModelPrice {
            label: "DeepSeek-V4-Pro-0813".into(),
            cache_hit: 0.15,
            cache_miss: 4.5,
            cache_write: 4.5,
            output: 13.5,
        };
        let mut models = vec![
            ("deepseek-flash".to_string(), flash.clone()),
            (
                "deepseek-v4-flash".to_string(),
                ModelPrice { label: "DeepSeek-V4.1-Flash（旧模型名）".into(), ..flash.clone() },
            ),
            (
                "deepseek-v4-flash-vision-exp".to_string(),
                ModelPrice { label: "DeepSeek-V4.1-Flash（视觉实验名）".into(), ..flash.clone() },
            ),
            ("deepseek-v4-pro".to_string(), pro.clone()),
            (
                "deepseek-pro".to_string(),
                ModelPrice { label: "DeepSeek-V4-Pro-0813（旧模型名）".into(), ..pro.clone() },
            ),
        ];
        models.shrink_to_fit();
        let mut holidays: Vec<String> = CN_HOLIDAYS_2026
            .iter()
            .chain(CN_HOLIDAYS_2027.iter())
            .map(|s| s.to_string())
            .collect();
        holidays.sort();
        Self {
            version: PRICE_TABLE_VERSION.to_string(),
            currency: "CNY".into(),
            symbol: "¥".into(),
            unit: 1e6,
            peak_multiplier: 2.0,
            peak_windows: vec![(9, 12), (14, 18)],
            peak_weekdays: vec![1, 2, 3, 4, 5],
            holidays,
            models,
            fallback_model: "deepseek-flash".into(),
        }
    }
}

impl PriceTable {
    pub fn model(&self, name: &str) -> Option<&ModelPrice> {
        self.models.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }
}

/// 合并用户配置的价目覆盖项（`config.json` 的 `pricing`），失败回落到默认表。
pub fn resolve(override_cfg: Option<&Value>) -> PriceTable {
    let mut table = PriceTable::default();
    let Some(ov) = override_cfg.and_then(|v| v.as_object()) else {
        return table;
    };

    let get_str = |k: &str| ov.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_num = |k: &str| ov.get(k).and_then(|v| v.as_f64());

    if let Some(v) = get_str("version").filter(|s| !s.is_empty()) {
        table.version = v;
    }
    if let Some(v) = get_str("currency") {
        table.currency = v;
    }
    if let Some(v) = get_str("symbol") {
        table.symbol = v;
    }
    if let Some(v) = get_num("unit").filter(|n| *n > 0.0) {
        table.unit = v;
    }
    if let Some(v) = get_num("peakMultiplier").filter(|n| *n > 0.0) {
        table.peak_multiplier = v;
    }
    if let Some(arr) = ov.get("peakWindows").and_then(|v| v.as_array()) {
        let windows: Vec<(i64, i64)> = arr
            .iter()
            .filter_map(|pair| {
                let p = pair.as_array()?;
                Some((p.first()?.as_i64()?, p.get(1)?.as_i64()?))
            })
            .collect();
        if !windows.is_empty() {
            table.peak_windows = windows;
        }
    }
    if let Some(arr) = ov.get("peakWeekdays").and_then(|v| v.as_array()) {
        let days: Vec<i64> = arr.iter().filter_map(|d| d.as_i64()).collect();
        if !days.is_empty() {
            table.peak_weekdays = days;
        }
    }
    if let Some(arr) = ov.get("holidays").and_then(|v| v.as_array()) {
        let list: Vec<String> = arr
            .iter()
            .filter_map(|d| d.as_str())
            .map(|s| s.to_string())
            .collect();
        if !list.is_empty() {
            table.holidays = list;
        }
    }

    if let Some(models) = ov.get("models").and_then(|v| v.as_object()) {
        for (name, entry) in models {
            let Some(e) = entry.as_object() else { continue };
            let base = table.model(name).cloned();
            let given = e.get("off").and_then(|v| v.as_object());
            let fallback = table
                .model(&table.fallback_model)
                .cloned()
                .unwrap_or_else(default_model);
            let start = base.clone().unwrap_or(fallback);
            let num = |k: &str, d: f64| {
                given
                    .and_then(|g| g.get(k))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(d)
            };
            // 只有条目**自己**没写 cacheWrite 时才跟随 cacheMiss；
            // 否则会从兜底模型继承一个不相干的 cacheWrite 价（静默算错钱）。
            let cache_miss = num("cacheMiss", start.cache_miss);
            let cache_write = if given
                .and_then(|g| g.get("cacheWrite"))
                .and_then(|v| v.as_f64())
                .is_none()
            {
                cache_miss
            } else {
                given
                    .and_then(|g| g.get("cacheWrite"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(cache_miss)
            };
            let price = ModelPrice {
                label: e
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    // 与 JS 一致：label = 条目自己的 ?? 内置条目的 ?? 模型名（不继承兜底模型的名称）。
                    .unwrap_or_else(|| base.as_ref().map(|b| b.label.clone()).unwrap_or_else(|| name.clone())),
                cache_hit: num("cacheHit", start.cache_hit),
                cache_miss,
                cache_write,
                output: num("output", start.output),
            };
            match table.models.iter_mut().find(|(k, _)| k == name) {
                Some(slot) => slot.1 = price,
                None => table.models.push((name.clone(), price)),
            }
        }
    }

    if get_str("fallbackModel")
        .filter(|s| table.model(s).is_some())
        .is_some()
    {
        table.fallback_model = get_str("fallbackModel").unwrap_or(table.fallback_model);
    }
    table
}

fn default_model() -> ModelPrice {
    ModelPrice {
        label: "DeepSeek-V4.1-Flash".into(),
        cache_hit: 0.02,
        cache_miss: 1.0,
        cache_write: 1.0,
        output: 4.0,
    }
}

/// 把时间戳换成北京时间的日历分量（不依赖运行机器的本地时区）。
///
/// 返回 `(weekday 1=周一..7=周日, hour, "YYYY-MM-DD", "HH:MM")`。
pub fn beijing_parts(at_ms: i64) -> (i64, i64, String, String) {
    let local = at_ms + BEIJING_OFFSET_MINUTES * 60_000;
    let secs = local.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 是周四；getUTCDay 里周日是 0
    let dow = (days + 4).rem_euclid(7);
    let weekday = if dow == 0 { 7 } else { dow };
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    (
        weekday,
        hour,
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{hour:02}:{minute:02}"),
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 某一时刻是否落在高峰时段。
pub fn is_peak_at(at_ms: i64, table: &PriceTable) -> bool {
    let (weekday, hour, date, _) = beijing_parts(at_ms);
    if !table.peak_weekdays.contains(&weekday) {
        return false;
    }
    if table.holidays.iter().any(|h| h == &date) {
        return false;
    }
    table
        .peak_windows
        .iter()
        .any(|(start, end)| hour >= *start && hour < *end)
}

/// 某模型在某一时刻生效的单价。
pub fn rates_at(model: &str, at_ms: i64, table: &PriceTable) -> Rates {
    let key = model.trim().to_string();
    let entry = table.model(&key);
    let chosen = entry
        .cloned()
        .or_else(|| table.model(&table.fallback_model).cloned())
        .unwrap_or_else(default_model);
    let peak = is_peak_at(at_ms, table);
    let factor = if peak { table.peak_multiplier } else { 1.0 };
    Rates {
        model: if key.is_empty() { table.fallback_model.clone() } else { key },
        label: chosen.label.clone(),
        tier: if peak { "peak" } else { "off" }.into(),
        tier_label: if peak { "高峰时段" } else { "空闲时段" }.into(),
        currency: table.currency.clone(),
        symbol: table.symbol.clone(),
        unit: table.unit,
        estimated: entry.is_none(),
        cache_hit: chosen.cache_hit * factor,
        cache_miss: chosen.cache_miss * factor,
        cache_write: chosen.cache_write * factor,
        output: chosen.output * factor,
    }
}

/// 把 usage 归一化成四个互不重叠的计费桶。
///
/// 同时接受两种键名，这一点是**必须**的：
///   · DSH 原始口径 `inputTokens` / `cacheReadTokens` / `cacheWriteTokens` / `outputTokens`
///   · 账本落盘口径 `cacheMiss` / `cacheHit` / `cacheWrite` / `output`
/// 只认一种会在「写盘 → 重新载入」时把桶读成 0，花费凭空少一截。
#[allow(dead_code)]
pub fn normalize_usage(usage: Option<&Value>) -> Buckets {
    let pick = |v: Option<&Value>, keys: &[&str]| -> f64 {
        let Some(obj) = v.and_then(|x| x.as_object()) else {
            return 0.0;
        };
        for k in keys {
            if let Some(n) = obj.get(*k).and_then(|x| x.as_f64()) {
                if n.is_finite() && n > 0.0 {
                    return n.round();
                }
            }
        }
        0.0
    };
    Buckets {
        // DSH 的 inputTokens 是「未命中」部分，不是总输入
        cache_miss: pick(usage, &["cacheMiss", "uncachedInputTokens", "inputTokens"]),
        cache_hit: pick(usage, &["cacheHit", "cacheReadTokens"]),
        cache_write: pick(usage, &["cacheWrite", "cacheWriteTokens"]),
        output: pick(usage, &["output", "outputTokens"]),
        reasoning: pick(usage, &["reasoning", "reasoningTokens"]),
    }
}

/// 按单价把 token 桶折算成金额。
pub fn cost_of(buckets: &Buckets, rates: &Rates) -> Cost {
    let cache_hit = buckets.cache_hit / rates.unit * rates.cache_hit;
    let cache_miss = buckets.cache_miss / rates.unit * rates.cache_miss;
    let cache_write = buckets.cache_write / rates.unit * rates.cache_write;
    let output = buckets.output / rates.unit * rates.output;
    Cost {
        cache_hit,
        cache_miss,
        cache_write,
        output,
        total: cache_hit + cache_miss + cache_write + output,
    }
}

/// 一步到位：桶 + 时间 + 模型 → 单价与金额。
pub fn price_usage(buckets: &Buckets, model: &str, at_ms: i64, table: &PriceTable) -> (Rates, Cost) {
    let rates = rates_at(model, at_ms, table);
    let cost = cost_of(buckets, &rates);
    (rates, cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i64, m: i64, d: i64, h: i64, min: i64) -> i64 {
        // 北京时间 → UTC 毫秒
        let days = days_from_civil(y, m, d);
        (days * 86_400 + h * 3600 + min * 60 - BEIJING_OFFSET_MINUTES * 60) * 1000
    }

    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    #[test]
    fn weekday_matches_known_dates() {
        // 2026-10-21 是周三
        let (wd, hour, date, _) = beijing_parts(ts(2026, 10, 21, 12, 0));
        assert_eq!(wd, 3);
        assert_eq!(hour, 12);
        assert_eq!(date, "2026-10-21");
    }

    #[test]
    fn peak_window_is_half_open() {
        let t = PriceTable::default();
        // 周三 9:00-11:59 高峰，12:00 已经不是
        assert!(is_peak_at(ts(2026, 10, 21, 9, 0), &t));
        assert!(is_peak_at(ts(2026, 10, 21, 11, 59), &t));
        assert!(!is_peak_at(ts(2026, 10, 21, 12, 0), &t));
        // 下午窗口 14:00-18:00
        assert!(is_peak_at(ts(2026, 10, 21, 14, 0), &t));
        assert!(!is_peak_at(ts(2026, 10, 21, 18, 0), &t));
    }

    #[test]
    fn weekend_and_holiday_are_off_peak() {
        let t = PriceTable::default();
        // 2026-10-24 是周六
        assert!(!is_peak_at(ts(2026, 10, 24, 10, 0), &t));
        // 2026-10-01 国庆（周四）
        assert!(!is_peak_at(ts(2026, 10, 1, 10, 0), &t));
    }

    #[test]
    fn peak_doubles_the_price() {
        let t = PriceTable::default();
        let off = rates_at("deepseek-flash", ts(2026, 10, 21, 20, 0), &t);
        let peak = rates_at("deepseek-flash", ts(2026, 10, 21, 10, 0), &t);
        assert_eq!(off.tier, "off");
        assert_eq!(peak.tier, "peak");
        assert!((peak.output - off.output * 2.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_falls_back_and_flags_estimated() {
        let t = PriceTable::default();
        let r = rates_at("no-such-model", ts(2026, 10, 21, 20, 0), &t);
        assert!(r.estimated, "未知模型必须标 estimated，否则控制台会当成真实账单");
        assert_eq!(r.label, "DeepSeek-V4.1-Flash");
    }

    #[test]
    fn normalize_accepts_both_key_styles() {
        let dsh = serde_json::json!({
            "inputTokens": 100, "cacheReadTokens": 200,
            "cacheWriteTokens": 0, "outputTokens": 50
        });
        let b = normalize_usage(Some(&dsh));
        assert_eq!(b.cache_miss, 100.0, "inputTokens 是未命中部分，不是总输入");
        assert_eq!(b.cache_hit, 200.0);
        assert_eq!(b.output, 50.0);

        let ledger = serde_json::json!({
            "cacheMiss": 7, "cacheHit": 9, "cacheWrite": 1, "output": 3
        });
        let b2 = normalize_usage(Some(&ledger));
        assert_eq!(b2.cache_miss, 7.0);
        assert_eq!(b2.cache_hit, 9.0);
        assert_eq!(b2.cache_write, 1.0);
        assert_eq!(b2.output, 3.0);
    }

    #[test]
    fn cost_math_is_linear_in_tokens() {
        let t = PriceTable::default();
        let rates = rates_at("deepseek-flash", ts(2026, 10, 21, 20, 0), &t);
        let one = cost_of(
            &Buckets { cache_miss: 1_000_000.0, ..Default::default() },
            &rates,
        );
        assert!((one.total - 1.0).abs() < 1e-9, "100 万未命中 token = 1 元");
        let four = cost_of(
            &Buckets { output: 1_000_000.0, ..Default::default() },
            &rates,
        );
        assert!((four.total - 4.0).abs() < 1e-9, "100 万输出 token = 4 元");
    }

    #[test]
    fn override_keeps_custom_cache_write() {
        // 覆盖项自己写了 cacheWrite 就必须用它，不能跟随 cacheMiss
        let ov = serde_json::json!({
            "models": { "deepseek-flash": { "off": { "cacheMiss": 2, "cacheWrite": 0.5 } } }
        });
        let t = resolve(Some(&ov));
        let m = t.model("deepseek-flash").unwrap();
        assert_eq!(m.cache_miss, 2.0);
        assert_eq!(m.cache_write, 0.5);
    }

    #[test]
    fn override_without_cache_write_inherits_cache_miss() {
        let ov = serde_json::json!({
            "models": { "deepseek-flash": { "off": { "cacheMiss": 3 } } }
        });
        let t = resolve(Some(&ov));
        let m = t.model("deepseek-flash").unwrap();
        assert_eq!(m.cache_write, 3.0, "没写 cacheWrite 就跟 cacheMiss");
    }
}
