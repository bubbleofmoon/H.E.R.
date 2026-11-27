//! Group mute state (#46). When QQ mutes her in a group, nothing is sent there and the group
//! does not wake her until the mute ends; the owner gets one private, content-free notice per
//! mute episode and none for a lift. State lives in `data/group-mutes.json` (keys `group:<id>`)
//! under `locks/group-mutes.lock`; it never reads or writes `social-v2.json`, so it cannot create
//! conversation records. Personal and whole-group restrictions are stored apart and the effective
//! mute is derived. Reads fail open (a refused send relearns it).

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::runtime::{RuntimeArea, RuntimeContext};

const FILE: &str = "group-mutes.json";
const GATE: &str = "locks/group-mutes.lock";
const GATE_WAIT: Duration = Duration::from_secs(5);
const QUERY_DEADLINE: Duration = Duration::from_secs(3);
/// Mutes with no known end (whole-group, or learned from a refusal) are rechecked this often.
pub const RECHECK_EVERY: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mute {
    /// End in ms since the epoch; `None` when the end is unknown.
    pub until: Option<i64>,
    pub whole_group: bool,
}

fn clock(at: i64, now: i64, cfg: &Value) -> String {
    let zone = crate::local_time::configured_zone(cfg);
    let time = crate::local_time::at(at, &zone);
    if at.saturating_sub(now) < 86_400_000 {
        time.strftime("%H:%M").to_string()
    } else {
        crate::local_time::display(&time)
    }
}

impl Mute {
    /// Model-facing result of a locally refused send.
    pub fn refusal_text(&self, now: i64, cfg: &Value) -> String {
        let when = match (self.whole_group, self.until) {
            (false, Some(until)) => { let t = clock(until, now, cfg); format!("当前群内被禁言直到 {t} 为止（muted until {t}）") }
            _ => "当前群内正处于禁言状态且解禁时间未明（muted end unknown）".to_string(),
        };
        format!("你在这个群{when}，消息没有发出，也不会重发。禁言结束前不要再在这个群发消息。{}", crate::social_state::END_TURN)
    }
}

/// Effective mute of one stored entry at `now`. A personal mute ending exactly at `now` is over.
pub(crate) fn effective(entry: &Value, now: i64) -> Option<Mute> {
    let personal = entry["personalUntil"].as_i64().unwrap_or(0);
    if entry["wholeGroup"] == true { return Some(Mute { until: None, whole_group: true }); }
    if personal == -1 { return Some(Mute { until: None, whole_group: false }); }
    (personal > now).then_some(Mute { until: Some(personal), whole_group: false })
}

fn empty() -> Value { json!({"v": 1, "groups": {}}) }

/// Reader view: a missing file is empty; an unreadable or invalid file is an error (callers fail open).
fn read(context: &RuntimeContext) -> Result<Value, String> {
    match context.read_optional_text(RuntimeArea::Data, FILE) {
        Ok(None) => Ok(empty()),
        Ok(Some(text)) => serde_json::from_str::<Value>(&text).ok()
            .filter(|v| v["groups"].is_object()).ok_or_else(|| "group-mutes.json is invalid".to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn gate(context: &RuntimeContext) -> Result<std::fs::File, String> {
    let deadline = Instant::now() + GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, GATE) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" && Instant::now() < deadline =>
                std::thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(format!("group-mutes gate: {error}")),
        }
    }
}

pub fn active(context: &RuntimeContext, key: &str) -> Option<Mute> {
    active_at(context, key, crate::parity_clock::now_ms())
}

pub(crate) fn active_at(context: &RuntimeContext, key: &str, now: i64) -> Option<Mute> {
    if !key.starts_with("group:") { return None; }
    effective(&read(context).ok()?["groups"][key], now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Ban notice for her own id: personal mute until this ms, or -1 when the end is unknown.
    Banned(i64),
    /// Lift notice for her own id, or a ban with zero duration.
    Lifted,
    /// QQ refused a send with result=120 and no mute is known: personal, end unknown.
    Learned,
    /// Gateway refresh. `None` = not readable, left unchanged. `seen_seq` is the entry's write
    /// counter read before the queries: a newer notice or refusal wins over this result.
    Refreshed { personal_until: Option<i64>, whole_group: Option<bool>, seen_seq: i64 },
    /// QQ's whole-group notice arrived: no state change, but every refresh that started earlier is stale.
    Invalidate,
}

/// The change a `group_ban` notice for her own id makes; `None` for an unknown `sub_type`.
pub fn change_from_notice(sub_type: &str, duration: Option<&Value>, now: i64) -> Option<Change> {
    match sub_type {
        "lift_ban" => Some(Change::Lifted),
        "ban" => {
            let seconds = duration.and_then(|d| match d {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            });
            Some(match seconds {
                Some(0) => Change::Lifted,
                Some(s) if s > 0 => Change::Banned(now.saturating_add(s.saturating_mul(1000))),
                _ => Change::Banned(-1),
            })
        }
        _ => None,
    }
}

/// Apply one change under the gate. Returns the episode revision whose owner notice this call claimed.
/// A transient read error refuses the change (`Err`); only an invalid file is replaced.
pub(crate) fn apply_at(context: &RuntimeContext, key: &str, change: Change, operator: Option<i64>, now: i64) -> Result<Option<i64>, String> {
    if !key.starts_with("group:") { return Ok(None); }
    let _gate = gate(context)?;
    let text = context.read_optional_text(RuntimeArea::Data, FILE).map_err(|e| format!("group-mutes read: {e}"))?;
    let mut store = match text {
        None => empty(),
        Some(text) => serde_json::from_str::<Value>(&text).ok().filter(|v| v["groups"].is_object()).unwrap_or_else(|| {
            crate::safe_println!("[mute] group-mutes.json is invalid; starting from an empty mute state");
            empty()
        }),
    };
    let existing = store["groups"].get(key).filter(|v| v.is_object()).cloned();
    let mut entry = existing.clone().unwrap_or_else(|| json!({"personalUntil": 0, "wholeGroup": false, "rev": 0, "noticedRev": 0, "seq": 0}));
    let before = effective(&entry, now);
    match change {
        Change::Banned(until) => entry["personalUntil"] = json!(until),
        Change::Lifted => entry["personalUntil"] = json!(0),
        // No state change: the write below only advances `seq`, which drops every refresh that began earlier.
        Change::Invalidate => {}
        // A refusal never shortens a known mute, but it is newer evidence than any refresh in flight:
        // it still bumps `seq` below.
        Change::Learned => { if before.is_none() { entry["personalUntil"] = json!(-1); } }
        Change::Refreshed { personal_until, whole_group, seen_seq } => {
            if entry["seq"].as_i64().unwrap_or(0) != seen_seq { return Ok(None); }
            if let Some(p) = personal_until {
                // A mute learned from a refusal may be a whole-group mute the member view does not
                // show; only a readable whole-group answer may clear it (no learn/clear/notice loop).
                let learned = entry["personalUntil"].as_i64() == Some(-1);
                if !(learned && p == 0 && whole_group.is_none()) { entry["personalUntil"] = json!(p); }
            }
            if let Some(w) = whole_group { entry["wholeGroup"] = json!(w); }
        }
    }
    let after = effective(&entry, now);
    // Only a refresh that finds nothing may skip the write. A notice or refusal always bumps `seq`,
    // so a refresh that started before it can never land a stale answer.
    if existing.is_none() && after.is_none() && matches!(change, Change::Refreshed { .. }) { return Ok(None); }
    let mut claim = None;
    if before.is_none() && after.is_some() {
        let rev = entry["rev"].as_i64().unwrap_or(0) + 1;
        entry["rev"] = json!(rev);
        entry["mutedAt"] = json!(now);
        entry["operator"] = json!(operator);
        if entry["noticedRev"].as_i64().unwrap_or(0) < rev { entry["noticedRev"] = json!(rev); claim = Some(rev); }
    }
    entry["seq"] = json!(entry["seq"].as_i64().unwrap_or(0) + 1);
    store["groups"].as_object_mut().expect("checked object").insert(key.to_string(), entry);
    let bytes = serde_json::to_vec(&store).map_err(|e| e.to_string())?;
    context.write_atomic(RuntimeArea::Data, FILE, &bytes)?;
    Ok(claim)
}

fn notice_text(key: &str, mute: Mute, operator: Option<i64>, now: i64, cfg: &Value) -> String {
    let group = key.trim_start_matches("group:");
    let span = match (mute.whole_group, mute.until) {
        (true, _) => "全员禁言，结束时间未知".to_string(),
        (false, Some(until)) => format!("约 {} 分钟，到 {}", (until - now + 59_999) / 60_000, clock(until, now, cfg)),
        (false, None) => "时长未知".to_string(),
    };
    let who = operator.map(|id| format!("操作人 {id}")).unwrap_or_else(|| "操作人未知".to_string());
    format!("禁言提醒：我在群 {group} 被禁言（{span}，{who}）。禁言期间不会在这个群发消息，也不会为它唤醒。")
}

/// Apply a change; a new episode sends the one owner notice (queued, never awaited).
pub fn record(context: &RuntimeContext, cfg: &Value, key: &str, change: Change, operator: Option<i64>) {
    let now = crate::parity_clock::now_ms();
    match apply_at(context, key, change, operator, now) {
        Ok(Some(rev)) => {
            if let Some(mute) = active_at(context, key, now) {
                crate::admin_alert::notify_notice(context, cfg, &notice_text(key, mute, operator, now, cfg), &format!("group-mute:{key}:{rev}"));
            }
        }
        Ok(None) => {}
        Err(error) => crate::safe_println!("[mute] {key} 禁言状态未能保存：{error}"),
    }
}

/// QQ's whole-group notice arrived for `key`: every refresh that began before it is stale and is dropped.
pub fn note_whole_group_notice(context: &RuntimeContext, key: &str) {
    if let Err(error) = apply_at(context, key, Change::Invalidate, None, crate::parity_clock::now_ms()) {
        crate::safe_println!("[mute] {key} 全员禁言通知未能使旧的刷新失效：{error}");
    }
}

/// A `group_ban` notice that names her own id.
pub fn on_self_notice(context: &RuntimeContext, cfg: &Value, key: &str, sub_type: &str, duration: Option<&Value>, operator: Option<i64>) {
    if let Some(change) = change_from_notice(sub_type, duration, crate::parity_clock::now_ms()) {
        record(context, cfg, key, change, operator);
    }
}

/// A send to `key` was refused with result=120.
pub fn learn_from_refusal(context: &RuntimeContext, key: &str) {
    if !key.starts_with("group:") { return; }
    let Ok(cfg) = context.load_config() else { return };
    record(context, &cfg, key, Change::Learned, None);
}

fn integer(value: &Value) -> Option<i64> {
    match value { Value::Number(n) => n.as_i64(), Value::String(s) => s.trim().parse().ok(), _ => None }
}

fn ok_data(envelope: Option<&Value>) -> Option<&Value> {
    let v = envelope?;
    (v["status"] == "ok" && v["retcode"].as_i64() == Some(0)).then(|| &v["data"])
}

/// Read `get_group_member_info` (self) and `get_group_info` replies. Group owners and admins are
/// exempt from a whole-group mute; an unreadable role counts as not exempt.
pub(crate) fn parse_refresh(member: Option<&Value>, info: Option<&Value>, now: i64) -> (Option<i64>, Option<bool>) {
    let member = ok_data(member);
    let personal = member.and_then(|d| integer(&d["shut_up_timestamp"]))
        .map(|ts| { let ms = ts.saturating_mul(1000); if ms > now { ms } else { 0 } });
    let exempt = matches!(member.and_then(|d| d["role"].as_str()), Some("owner" | "admin"));
    let whole = ok_data(info).and_then(|d| integer(&d["group_all_shut"])).map(|v| v != 0 && !exempt);
    (personal, whole)
}

pub(crate) fn seq_of(context: &RuntimeContext, key: &str) -> i64 {
    read(context).ok().and_then(|s| s["groups"][key]["seq"].as_i64()).unwrap_or(0)
}

/// Groups whose mute has no known end.
pub fn unknown_end_groups(context: &RuntimeContext, now: i64) -> Vec<i64> {
    let Ok(store) = read(context) else { return Vec::new() };
    store["groups"].as_object().into_iter().flatten()
        .filter(|(_, e)| effective(e, now).is_some_and(|m| m.until.is_none()))
        .filter_map(|(k, _)| k.strip_prefix("group:")?.parse().ok()).collect()
}

/// A `group_ban` notice with `user_id` 0 (QQ's whole-group mute or lift). The ingress `Event`
/// refuses id 0, so only this narrow parse accepts it; every other present identity must be valid.
/// Returns the group and the (lenient) operator.
pub fn whole_group_notice(raw: &str) -> Option<(i64, Option<i64>)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v["post_type"] != "notice" || v["notice_type"] != "group_ban" { return None; }
    if crate::protocol_integer(&v["user_id"]) != Some(0) { return None; }
    for field in ["self_id", "sender_id", "target_id"] {
        if let Some(x) = v.get(field) { crate::protocol_integer(x).filter(|id| *id > 0)?; }
    }
    let group = crate::protocol_integer(&v["group_id"]).filter(|id| *id > 0)?;
    Some((group, crate::protocol_integer(&v["operator_id"]).filter(|id| *id > 0)))
}

static REFRESHING: AtomicBool = AtomicBool::new(false);
/// Groups waiting for a refresh, with the operator a whole-group notice named (for its owner notice).
static PENDING: Mutex<Vec<(i64, Option<i64>)>> = Mutex::new(Vec::new());

/// Clears `REFRESHING` when the worker stops, including by panic.
struct Running;
impl Drop for Running {
    fn drop(&mut self) { REFRESHING.store(false, Ordering::SeqCst); }
}

/// Queue a gateway refresh of these groups on one background worker. Requests made while a worker
/// runs are picked up by that worker (it keeps the connection details it started with).
pub fn spawn_refresh(context: RuntimeContext, http_base: String, token: String, self_id: i64, groups: Vec<(i64, Option<i64>)>) {
    if groups.is_empty() || self_id <= 0 { return; }
    {
        let mut pending = PENDING.lock().unwrap_or_else(|p| p.into_inner());
        for (gid, operator) in groups {
            match pending.iter_mut().find(|(g, _)| *g == gid) {
                Some(slot) => { if operator.is_some() { slot.1 = operator; } }
                None => pending.push((gid, operator)),
            }
        }
    }
    if REFRESHING.swap(true, Ordering::SeqCst) { return; }
    let worker = move || {
        let mut running = Some(Running);
        loop {
            let batch = std::mem::take(&mut *PENDING.lock().unwrap_or_else(|p| p.into_inner()));
            if batch.is_empty() {
                drop(running.take());
                // A request queued after the take but before the release would otherwise wait for the next trigger.
                if PENDING.lock().unwrap_or_else(|p| p.into_inner()).is_empty() || REFRESHING.swap(true, Ordering::SeqCst) { break; }
                running = Some(Running);
                continue;
            }
            refresh(&context, &http_base, &token, self_id, &batch);
        }
    };
    if std::thread::Builder::new().name("mute-refresh".into()).spawn(worker).is_err() {
        REFRESHING.store(false, Ordering::SeqCst);
    }
}

fn refresh(context: &RuntimeContext, http_base: &str, token: &str, self_id: i64, groups: &[(i64, Option<i64>)]) {
    let Ok(ob) = crate::onebot::OneBot::try_new_url(http_base, token.to_string()) else { return };
    let Ok(cfg) = context.load_config() else { return };
    for (gid, operator) in groups {
        let key = format!("group:{gid}");
        let seen_seq = seq_of(context, &key);
        let member = ob.request_with_deadline("get_group_member_info",
            json!({"group_id": gid, "user_id": self_id, "no_cache": true}), Instant::now() + QUERY_DEADLINE).ok();
        let info = ob.request_with_deadline("get_group_info",
            json!({"group_id": gid, "no_cache": true}), Instant::now() + QUERY_DEADLINE).ok();
        let (personal_until, whole_group) = parse_refresh(member.as_ref(), info.as_ref(), crate::parity_clock::now_ms());
        if personal_until.is_none() && whole_group.is_none() { continue; }
        record(context, &cfg, &key, Change::Refreshed { personal_until, whole_group, seen_seq }, *operator);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const NOW: i64 = 1_000_000_000;

    /// Removes the temporary runtime root when the test ends, including on an assertion failure.
    struct TempRoot(std::path::PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(tag: &str) -> (TempRoot, RuntimeContext) {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(crate::turn_ledger::mint_id(tag)),
        );
        for dir in ["config", "data", "resources", "user/dsh"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"synthetic-review-token-abcdefghij"}"#,
        )
        .unwrap();
        fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (TempRoot(root), context)
    }

    /// The parsed `group-mutes.json`, or `Null` when it is missing or invalid.
    fn store(root: &TempRoot) -> Value {
        fs::read(root.0.join("data/group-mutes.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    }

    #[test]
    fn a_mute_ending_exactly_now_is_lifted() {
        assert_eq!(effective(&json!({"personalUntil": 1000}), 1000), None);
        assert_eq!(
            effective(&json!({"personalUntil": 1000}), 999),
            Some(Mute { until: Some(1000), whole_group: false })
        );
    }

    #[test]
    fn unknown_end_and_whole_group_mutes_stay_active() {
        assert_eq!(
            effective(&json!({"personalUntil": -1}), NOW),
            Some(Mute { until: None, whole_group: false })
        );
        assert_eq!(
            effective(&json!({"personalUntil": 0, "wholeGroup": true}), NOW),
            Some(Mute { until: None, whole_group: true })
        );
    }

    #[test]
    fn a_ban_starts_one_episode_and_claims_one_notice() {
        let (root, ctx) = fixture("mute-episode");
        let key = "group:1";
        let until = NOW + 60_000;
        assert_eq!(apply_at(&ctx, key, Change::Banned(until), Some(1001), NOW), Ok(Some(1)));
        assert_eq!(active_at(&ctx, key, NOW), Some(Mute { until: Some(until), whole_group: false }));
        // An extension inside the same episode claims no notice.
        assert_eq!(apply_at(&ctx, key, Change::Banned(until + 60_000), Some(1001), NOW), Ok(None));
        // A lift ends the episode.
        assert_eq!(apply_at(&ctx, key, Change::Lifted, None, NOW), Ok(None));
        assert_eq!(active_at(&ctx, key, NOW), None);
        // A ban after the lift is a new episode with its own notice.
        assert_eq!(apply_at(&ctx, key, Change::Banned(until), Some(1001), NOW), Ok(Some(2)));
        assert_eq!(store(&root)["groups"][key]["rev"], json!(2));
    }

    #[test]
    fn a_lift_keeps_an_ongoing_whole_group_mute() {
        let (_root, ctx) = fixture("mute-whole");
        let key = "group:1";
        let seen = seq_of(&ctx, key);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: None, whole_group: Some(true), seen_seq: seen }, None, NOW),
            Ok(Some(1))
        );
        assert_eq!(apply_at(&ctx, key, Change::Lifted, None, NOW), Ok(None));
        assert_eq!(active_at(&ctx, key, NOW), Some(Mute { until: None, whole_group: true }));
        let seen = seq_of(&ctx, key);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: None, whole_group: Some(false), seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert_eq!(active_at(&ctx, key, NOW), None);
    }

    #[test]
    fn a_refresh_that_raced_a_newer_notice_is_dropped() {
        let (_root, ctx) = fixture("mute-race");
        let key = "group:1";
        let seen = seq_of(&ctx, key);
        assert_eq!(apply_at(&ctx, key, Change::Banned(NOW + 60_000), Some(1001), NOW), Ok(Some(1)));
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: Some(0), whole_group: None, seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert!(active_at(&ctx, key, NOW).is_some(), "the older refresh did not clear the newer ban");

        let key = "group:2";
        let seen = seq_of(&ctx, key);
        assert_eq!(apply_at(&ctx, key, Change::Lifted, None, NOW), Ok(None));
        assert_eq!(seq_of(&ctx, key), 1);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: Some(NOW + 60_000), whole_group: None, seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert_eq!(active_at(&ctx, key, NOW), None, "the older refresh did not add a mute");
    }

    #[test]
    fn writes_leave_social_state_alone_and_unmuted_groups_unrecorded() {
        let (root, ctx) = fixture("mute-social");
        let social = root.0.join("data/social-v2.json");
        let before = fs::read(&social).unwrap();
        assert_eq!(apply_at(&ctx, "group:1", Change::Banned(NOW + 60_000), Some(1001), NOW), Ok(Some(1)));
        assert_eq!(store(&root)["groups"]["group:1"]["personalUntil"], json!(NOW + 60_000));
        assert_eq!(fs::read(&social).unwrap(), before, "social-v2.json is byte-identical");
        let seen = seq_of(&ctx, "group:2");
        assert_eq!(
            apply_at(&ctx, "group:2", Change::Refreshed { personal_until: Some(0), whole_group: Some(false), seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert!(store(&root)["groups"].get("group:2").is_none(), "an unmuted group with no entry is not recorded");
    }

    #[test]
    fn learning_never_shortens_a_known_mute() {
        let (_root, ctx) = fixture("mute-learn");
        let key = "group:1";
        let until = NOW + 60_000;
        assert_eq!(apply_at(&ctx, key, Change::Banned(until), Some(1001), NOW), Ok(Some(1)));
        assert_eq!(apply_at(&ctx, key, Change::Learned, None, NOW), Ok(None));
        assert_eq!(active_at(&ctx, key, NOW).unwrap().until, Some(until));
    }

    #[test]
    fn a_refresh_keeps_a_learned_mute_until_the_whole_group_state_is_read() {
        let (_root, ctx) = fixture("mute-learned-refresh");
        let key = "group:1";
        assert_eq!(apply_at(&ctx, key, Change::Learned, None, NOW), Ok(Some(1)));
        let seen = seq_of(&ctx, key);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: Some(0), whole_group: None, seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert_eq!(active_at(&ctx, key, NOW), Some(Mute { until: None, whole_group: false }));
        let seen = seq_of(&ctx, key);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: Some(0), whole_group: Some(false), seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert_eq!(active_at(&ctx, key, NOW), None);
    }

    #[test]
    fn a_refusal_while_muted_still_beats_an_older_refresh() {
        let (_root, ctx) = fixture("mute-refusal-race");
        let key = "group:1";
        assert_eq!(apply_at(&ctx, key, Change::Banned(NOW + 60_000), Some(1001), NOW), Ok(Some(1)));
        let seen = seq_of(&ctx, key);
        // A send refused with result=120 arrives after the refresh began and confirms the mute.
        assert_eq!(apply_at(&ctx, key, Change::Learned, None, NOW), Ok(None));
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: Some(0), whole_group: Some(false), seen_seq: seen }, None, NOW),
            Ok(None)
        );
        assert!(active_at(&ctx, key, NOW).is_some(), "the older refresh did not clear the mute the refusal confirmed");
    }

    #[test]
    fn a_whole_group_notice_invalidates_an_older_refresh() {
        let (_root, ctx) = fixture("mute-invalidate");
        let key = "group:1";
        let seen = seq_of(&ctx, key);
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: None, whole_group: Some(true), seen_seq: seen }, None, NOW),
            Ok(Some(1))
        );
        // A refresh starts, then the whole-group notice arrives; the refresh's answer predates it.
        let stale = seq_of(&ctx, key);
        assert_eq!(apply_at(&ctx, key, Change::Invalidate, None, NOW), Ok(None));
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: None, whole_group: Some(false), seen_seq: stale }, None, NOW),
            Ok(None)
        );
        assert!(active_at(&ctx, key, NOW).is_some(), "the older refresh did not clear the mute");
        // With no entry at all the notice still leaves a mark that drops an older answer.
        let key = "group:2";
        let stale = seq_of(&ctx, key);
        assert_eq!(apply_at(&ctx, key, Change::Invalidate, None, NOW), Ok(None));
        assert_eq!(seq_of(&ctx, key), stale + 1, "the notice advanced the write counter");
        assert_eq!(
            apply_at(&ctx, key, Change::Refreshed { personal_until: None, whole_group: Some(true), seen_seq: stale }, None, NOW),
            Ok(None)
        );
        assert_eq!(active_at(&ctx, key, NOW), None, "the stale answer was dropped");
    }

    #[test]
    fn apply_refuses_on_a_transient_read_error() {
        let (root, ctx) = fixture("mute-read-error");
        let path = root.0.join("data/group-mutes.json");
        fs::create_dir_all(&path).unwrap();
        assert!(apply_at(&ctx, "group:1", Change::Banned(NOW + 60_000), Some(1001), NOW).is_err());
        assert!(path.is_dir(), "the unreadable state is kept, not replaced");
    }

    #[test]
    fn whole_group_notice_is_narrow() {
        let valid = r#"{"post_type":"notice","notice_type":"group_ban","user_id":0,"group_id":420042,"self_id":7001,"operator_id":1001}"#;
        assert_eq!(whole_group_notice(valid), Some((420042, Some(1001))));
        let bad_operator = r#"{"post_type":"notice","notice_type":"group_ban","user_id":0,"group_id":420042,"self_id":7001,"operator_id":"x"}"#;
        assert_eq!(whole_group_notice(bad_operator), Some((420042, None)));
        for bad in [
            r#"{"post_type":"notice","notice_type":"group_ban","user_id":5,"group_id":420042}"#,
            r#"{"post_type":"notice","notice_type":"group_ban","user_id":0,"group_id":420042,"self_id":"oops"}"#,
            r#"{"post_type":"notice","notice_type":"group_ban","user_id":0,"group_id":0}"#,
            r#"{"post_type":"notice","notice_type":"group_upload","user_id":0,"group_id":420042}"#,
        ] {
            assert_eq!(whole_group_notice(bad), None, "{bad}");
        }
    }

    #[test]
    fn notice_changes_follow_duration() {
        assert_eq!(change_from_notice("ban", Some(&json!(600)), NOW), Some(Change::Banned(NOW + 600_000)));
        assert_eq!(change_from_notice("ban", Some(&json!("600")), NOW), Some(Change::Banned(NOW + 600_000)));
        assert_eq!(change_from_notice("ban", Some(&json!(0)), NOW), Some(Change::Lifted));
        assert_eq!(change_from_notice("ban", None, NOW), Some(Change::Banned(-1)));
        assert_eq!(change_from_notice("ban", Some(&json!(-5)), NOW), Some(Change::Banned(-1)));
        assert_eq!(change_from_notice("lift_ban", None, NOW), Some(Change::Lifted));
        assert_eq!(change_from_notice("set_admin", None, NOW), None);
    }

    #[test]
    fn parse_refresh_reads_seconds_role_and_all_shut() {
        let now = 1_000_000;
        let ok = |data: Value| json!({"status": "ok", "retcode": 0, "data": data});
        let member = |ts: Value, role: &str| ok(json!({"shut_up_timestamp": ts, "role": role}));
        let all_shut = |v: i64| ok(json!({"group_all_shut": v}));

        assert_eq!(parse_refresh(Some(&member(json!(2000), "member")), None, now), (Some(2_000_000), None));
        assert_eq!(parse_refresh(Some(&member(json!("2000"), "member")), None, now).0, Some(2_000_000));
        assert_eq!(parse_refresh(Some(&member(json!(500), "member")), None, now).0, Some(0));
        assert_eq!(
            parse_refresh(Some(&member(json!(1000), "member")), None, now).0,
            Some(0),
            "a mute ending exactly now is over"
        );
        assert_eq!(
            parse_refresh(Some(&ok(json!({"role": "member"}))), None, now).0,
            None,
            "a missing shut_up_timestamp is unknown"
        );
        assert_eq!(parse_refresh(Some(&json!({"status": "failed", "retcode": 1200})), None, now), (None, None));
        assert_eq!(parse_refresh(None, Some(&json!({"status": "failed", "retcode": 1200})), now), (None, None));

        assert_eq!(parse_refresh(Some(&member(json!(0), "member")), Some(&all_shut(-1)), now), (Some(0), Some(true)));
        assert_eq!(parse_refresh(Some(&member(json!(0), "admin")), Some(&all_shut(-1)), now).1, Some(false));
        assert_eq!(parse_refresh(Some(&member(json!(0), "owner")), Some(&all_shut(-1)), now).1, Some(false));
        assert_eq!(parse_refresh(Some(&member(json!(0), "member")), Some(&all_shut(0)), now).1, Some(false));
        assert_eq!(
            parse_refresh(Some(&ok(json!({"shut_up_timestamp": 0}))), Some(&all_shut(-1)), now).1,
            Some(true),
            "an unreadable role is not exempt"
        );
    }

    #[test]
    fn refusal_text_names_the_end_and_ends_the_turn() {
        let timed = Mute { until: Some(NOW + 600_000), whole_group: false };
        let text = timed.refusal_text(NOW, &json!({"time":{"zone":"Asia/Shanghai"}}));
        assert!(
            text.contains("muted until")
                && text.contains(crate::social_state::END_TURN),
            "{text}"
        );
        let open_ended = Mute { until: None, whole_group: true };
        let text = open_ended.refusal_text(NOW, &json!({}));
        assert!(
            text.contains("end unknown") && text.contains(crate::social_state::END_TURN),
            "{text}"
        );
    }
}
