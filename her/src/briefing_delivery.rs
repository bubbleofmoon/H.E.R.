//! Briefing delivery (ADR 0007): what a conversation's session already holds of its briefing and what the next
//! wake must carry. The state is keyed by conversation and session: the engine's store keeps one record per
//! conversation for the session it runs (the DSH route: `briefings.<key>` in sessions.json, through
//! `session_map::update_context`, checked against the engine's session id and the conversation's epoch). Within
//! that session a counted generation starts at 0. The record holds the generation, the pending and the installed
//! briefing (receipt id, revision, section hashes, carried recall ids) and "needs a full briefing again"
//! (`needsBriefing`). The stored field names are format 1 (`FORMAT`); the receipt id is stored as `rpcId`.
//!
//! The rules live here for every engine; an engine adapter only reports facts:
//! - `delivered`: the prompt with this receipt id reached the engine's history at `seq`;
//! - `history_gap`: the engine's history has a gap the bridge never observed, through `through`;
//! - `next_generation`: the session's history was compacted, or the persona switched.
//!
//! Every new session (rotation, reset, and the engine switch of #10) starts its record with `begin`.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The stored record's format; a record of another format is started again.
pub(crate) const FORMAT: u64 = 1;

/// The record a new session starts with: generation 0, nothing pending or installed.
pub(crate) fn fresh(sid: &str, epoch: &str) -> Value {
    json!({"sid": sid, "epoch": epoch, "format": FORMAT, "generation": 0})
}

/// Whether `record` is the record of session `sid` in epoch `epoch`.
pub(crate) fn belongs(record: &Value, sid: &str, epoch: &str) -> bool {
    record["sid"] == sid && record["epoch"] == epoch && record["format"] == FORMAT
}

/// Start the record of session (`sid`, `epoch`) unless `record` already is it. Every new session starts this way:
/// its next wake carries a full briefing and no recall is held.
pub(crate) fn begin(record: &mut Value, sid: &str, epoch: &str) {
    if !belongs(record, sid, epoch) {
        *record = fresh(sid, epoch);
    }
}

/// What the next wake of a session must carry, read from its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WakePlan {
    pub generation: u64,
    /// A full briefing: the generation has none installed yet, or a history gap asked for one again.
    pub full: bool,
    /// "Needs a full briefing again" as read; `hold` holds a prepared prompt only while it is unchanged.
    pub needs_briefing: bool,
    /// The memories the generation already carries: the installed briefing's recall, kept while that briefing was
    /// installed for this generation (a history-gap refresh keeps them; a new generation or session has none).
    pub held: Vec<i64>,
    /// The installed briefing's section hashes by section name.
    installed: BTreeMap<String, String>,
}

impl WakePlan {
    /// Whether section `name`, whose current hash is `hash` (`None`: the section is empty), differs from the
    /// installed briefing: a delta replaces or removes exactly these sections.
    pub fn changed(&self, name: &str, hash: Option<&str>) -> bool {
        self.installed.get(name).map(String::as_str) != hash
    }
}

/// Read the next wake's plan from the session's record.
pub(crate) fn plan(record: &Value) -> WakePlan {
    let generation = record["generation"].as_u64().unwrap_or(0);
    let installed = &record["installed"];
    let current = installed["generation"].as_u64() == Some(generation);
    let needs_briefing = record["needsBriefing"] == true;
    let held = if current {
        installed["recalled"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let sections = installed["sections"]
        .as_object()
        .map(|sections| {
            sections
                .iter()
                .filter_map(|(name, hash)| hash.as_str().map(|hash| (name.clone(), hash.to_string())))
                .collect()
        })
        .unwrap_or_default();
    WakePlan { generation, full: needs_briefing || !current, needs_briefing, held, installed: sections }
}

/// Hold a prepared prompt's briefing as pending under `receipt` (the id the engine reports on delivery), only while
/// the record is still at the generation and the "needs a full briefing again" state `plan` read; returns whether it
/// was held. A generation or refresh that started since the plan was read never installs the old briefing: the
/// caller prepares the prompt again.
pub(crate) fn hold(
    record: &mut Value,
    plan: &WakePlan,
    receipt: &str,
    revision: &str,
    sections: &BTreeMap<&str, String>,
    recalled: &[i64],
) -> bool {
    if record["generation"].as_u64().unwrap_or(0) != plan.generation
        || (record["needsBriefing"] == true) != plan.needs_briefing
    {
        return false;
    }
    record["pending"] = json!({
        "rpcId": receipt,
        "generation": plan.generation,
        "revision": revision,
        "sections": sections,
        "full": plan.full,
        "recalled": recalled
    });
    true
}

/// The engine reports that the prompt with `receipt` reached its history at `seq`. The pending briefing becomes the
/// installed one when it is that prompt's and of the current generation, and "needs a full briefing again" clears; a
/// foreign receipt, or a pending briefing of an earlier generation, changes nothing. Returns whether it installed.
pub(crate) fn delivered(record: &mut Value, receipt: &str, seq: i64) -> bool {
    if record["pending"]["rpcId"].as_str() != Some(receipt) || record["pending"]["generation"] != record["generation"] {
        return false;
    }
    record["installed"] = record["pending"].take();
    record["needsBriefing"] = json!(false);
    record["installed"]["seq"] = json!(seq);
    true
}

/// The engine reports a gap in its history that the bridge never observed, through position `through`: the next wake
/// carries a full briefing again in the same generation (the generation's held recall stays) and a pending delta is
/// dropped; a pending full briefing stays, because its delivery installs a full briefing anyway. A gap already
/// reported through `through` or later changes nothing. Returns whether it asked for a full briefing.
pub(crate) fn history_gap(record: &mut Value, through: i64) -> bool {
    if record["gapThrough"].as_i64().unwrap_or(0) >= through {
        return false;
    }
    record["gapThrough"] = json!(through);
    record["needsBriefing"] = json!(true);
    if record["pending"]["full"] != true {
        record["pending"] = Value::Null;
    }
    true
}

/// Start the next generation in the same session (a completed compaction, a persona switch): nothing is installed
/// for it, so the next wake carries a full briefing and holds no recall; a pending briefing of the old generation is
/// dropped and can no longer be installed.
pub(crate) fn next_generation(record: &mut Value) {
    record["generation"] = json!(record["generation"].as_u64().unwrap_or(0) + 1);
    record["pending"] = Value::Null;
    record["needsBriefing"] = json!(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(pairs: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
        pairs.iter().map(|(name, hash)| (*name, hash.to_string())).collect()
    }

    /// Prepare a wake as an engine route does: read the plan, then hold the prompt under `receipt`, carrying the
    /// generation's held recall and `recalled`.
    fn prepare(record: &mut Value, receipt: &str, hashes: &[(&'static str, &str)], recalled: &[i64]) -> WakePlan {
        let next = plan(record);
        let carried: Vec<i64> = next.held.iter().chain(recalled).copied().collect();
        assert!(hold(record, &next, receipt, "revision", &sections(hashes), &carried), "{record}");
        next
    }

    #[test]
    fn a_first_wake_carries_the_full_briefing_and_only_its_delivery_installs_it() {
        let mut record = fresh("sid", "epoch");
        let first = prepare(&mut record, "rpc-1", &[("identity", "h1"), ("memory", "m1")], &[7]);
        assert_eq!((first.generation, first.full, first.held.clone()), (0, true, vec![]));
        assert!(record["installed"].is_null(), "a held prompt is not installed before the engine reports it delivered");
        assert_eq!(record["pending"]["full"], true);
        assert!(delivered(&mut record, "rpc-1", 5));
        assert_eq!(record["installed"]["seq"], 5);
        assert_eq!(record["installed"]["full"], true);
        assert!(record["pending"].is_null());
        let next = plan(&record);
        assert_eq!((next.generation, next.full, next.held), (0, false, vec![7]), "the next wake is a delta holding the delivered recall");
    }

    #[test]
    fn a_receipt_the_engine_never_reports_installs_nothing_and_holds_nothing_back() {
        let mut record = fresh("sid", "epoch");
        prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
        assert!(!delivered(&mut record, "rpc-other", 1), "a foreign receipt is no proof");
        let again = prepare(&mut record, "rpc-2", &[("identity", "h1")], &[7]);
        assert!(again.full && again.held.is_empty(), "an undelivered briefing holds nothing back: {again:?}");
        assert!(!delivered(&mut record, "rpc-1", 2), "a replaced receipt is no proof either");
        assert!(delivered(&mut record, "rpc-2", 3));
        let mut stale = fresh("sid", "epoch");
        prepare(&mut stale, "rpc-9", &[], &[]);
        stale["generation"] = json!(1);
        assert!(!delivered(&mut stale, "rpc-9", 4), "a pending briefing of an earlier generation is never installed");
    }

    #[test]
    fn a_delta_wake_names_only_the_changed_sections_and_installs_on_delivery() {
        let mut record = fresh("sid", "epoch");
        prepare(&mut record, "rpc-1", &[("identity", "h1"), ("memory", "m1")], &[7]);
        assert!(delivered(&mut record, "rpc-1", 1));
        let delta = prepare(&mut record, "rpc-2", &[("identity", "h1"), ("memory", "m2")], &[8]);
        assert!(!delta.full, "{delta:?}");
        assert!(!delta.changed("identity", Some("h1")), "an unchanged section is not sent again");
        assert!(delta.changed("memory", Some("m2")), "a changed section is replaced");
        assert!(delta.changed("identity", None), "a section that is empty now is removed");
        assert!(delta.changed("slang", Some("s1")), "a new section is added");
        assert!(!delta.changed("slang", None), "a section empty before and now is not named");
        assert_eq!(record["pending"]["full"], false);
        assert_eq!(record["pending"]["recalled"], json!([7, 8]), "a delta carries the generation's recall and its own");
        assert!(delivered(&mut record, "rpc-2", 2));
        let next = plan(&record);
        assert_eq!((next.full, next.held.clone()), (false, vec![7, 8]));
        assert!(!next.changed("memory", Some("m2")), "the delivered delta's hashes are installed");
    }

    #[test]
    fn a_history_gap_asks_for_a_full_briefing_again_in_the_same_generation_and_keeps_the_held_recall() {
        let mut record = fresh("sid", "epoch");
        prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
        assert!(delivered(&mut record, "rpc-1", 1));
        prepare(&mut record, "rpc-2", &[("identity", "h1")], &[8]);
        assert!(history_gap(&mut record, 10));
        assert!(record["pending"].is_null(), "a gap drops a pending delta");
        assert!(!history_gap(&mut record, 10), "a gap counts once");
        assert!(!history_gap(&mut record, 9), "an older gap changes nothing");
        let refresh = prepare(&mut record, "rpc-3", &[("identity", "h1")], &[9]);
        assert_eq!((refresh.generation, refresh.full, refresh.held.clone()), (0, true, vec![7]), "the same generation, full, recall kept");
        assert!(history_gap(&mut record, 20));
        assert_eq!(record["pending"]["rpcId"], "rpc-3", "a pending full briefing survives a later gap");
        assert!(delivered(&mut record, "rpc-3", 21));
        let next = plan(&record);
        assert_eq!((next.full, next.needs_briefing, next.held), (false, false, vec![7, 9]));
    }

    #[test]
    fn a_generation_or_refresh_that_starts_while_a_wake_is_prepared_refuses_to_hold_it() {
        let mut record = fresh("sid", "epoch");
        let stale = plan(&record);
        next_generation(&mut record);
        assert!(!hold(&mut record, &stale, "rpc-1", "revision", &sections(&[]), &[]), "a new generation since the plan");
        assert!(record["pending"].is_null());
        prepare(&mut record, "rpc-2", &[("identity", "h1")], &[]);
        assert!(delivered(&mut record, "rpc-2", 1));
        let stale = plan(&record);
        assert!(history_gap(&mut record, 5));
        assert!(!hold(&mut record, &stale, "rpc-3", "revision", &sections(&[]), &[]), "a refresh asked for since the plan");
        let now = plan(&record);
        assert!(hold(&mut record, &now, "rpc-3", "revision", &sections(&[]), &[]));
    }

    #[test]
    fn a_compaction_or_persona_switch_starts_the_next_generation_with_a_full_briefing_and_no_held_recall() {
        let mut record = fresh("sid", "epoch");
        prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
        assert!(delivered(&mut record, "rpc-1", 1));
        prepare(&mut record, "rpc-2", &[("identity", "h1")], &[8]);
        next_generation(&mut record);
        assert!(record["pending"].is_null(), "the old generation's pending briefing is dropped");
        assert!(!delivered(&mut record, "rpc-2", 2), "and can no longer be installed");
        let next = plan(&record);
        assert_eq!((next.generation, next.full, next.held), (1, true, vec![]));
        let first = prepare(&mut record, "rpc-3", &[("identity", "h1")], &[7]);
        assert_eq!(first.generation, 1);
        assert!(delivered(&mut record, "rpc-3", 3));
        assert_eq!(plan(&record).held, vec![7], "the new generation carries the memory again");
    }

    #[test]
    fn rotation_reset_and_an_engine_switch_all_start_the_new_session_the_same_way() {
        let running = || {
            let mut record = fresh("dsh-1", "epoch-1");
            prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
            assert!(delivered(&mut record, "rpc-1", 1));
            next_generation(&mut record);
            assert!(history_gap(&mut record, 5));
            record["rotation"] = json!({"phase": "rotate", "turnSeq": 3});
            record
        };
        // Rotation and reset retire the DSH session and replace the epoch; an engine switch moves the conversation to
        // the other engine's session, with its epoch replaced or kept.
        for (what, sid, epoch) in [
            ("rotation", "dsh-2", "epoch-2"),
            ("reset", "dsh-3", "epoch-3"),
            ("engine switch", "harness-1", "epoch-4"),
            ("engine switch keeping the epoch", "harness-1", "epoch-1"),
        ] {
            let mut record = running();
            begin(&mut record, sid, epoch);
            assert_eq!(record, fresh(sid, epoch), "{what}");
            let next = plan(&record);
            assert_eq!((next.generation, next.full, next.held), (0, true, vec![]), "{what}");
        }
        let mut same = running();
        let before = same.clone();
        begin(&mut same, "dsh-1", "epoch-1");
        assert_eq!(same, before, "the running session keeps its record");
        let mut other_format = running();
        other_format["format"] = json!(0);
        begin(&mut other_format, "dsh-1", "epoch-1");
        assert_eq!(other_format, fresh("dsh-1", "epoch-1"), "a record of another format is started again");
    }

    #[test]
    fn a_stale_generation_alone_refuses_to_hold_a_wake_prepared_for_it() {
        let mut record = fresh("sid", "epoch");
        next_generation(&mut record);
        let stale = plan(&record);
        next_generation(&mut record);
        assert!(record["needsBriefing"] == true && stale.needs_briefing, "both refresh flags stay set across the two generations");
        assert!(!hold(&mut record, &stale, "rpc-1", "revision", &sections(&[]), &[]), "the generation moved since the plan, and nothing else did");
        assert!(record["pending"].is_null());
    }

    #[test]
    fn a_new_epoch_alone_starts_a_fresh_record_with_nothing_installed() {
        let mut record = fresh("dsh-1", "epoch-1");
        prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
        assert!(delivered(&mut record, "rpc-1", 1));
        begin(&mut record, "dsh-1", "epoch-2");
        assert_eq!(record, fresh("dsh-1", "epoch-2"));
        assert!(record["installed"].is_null(), "{record}");
        let next = plan(&record);
        assert_eq!((next.generation, next.full, next.held), (0, true, vec![]), "the same session under a new epoch holds no recall");
    }

    #[test]
    fn a_compaction_sets_needs_briefing_explicitly() {
        let mut record = fresh("sid", "epoch");
        prepare(&mut record, "rpc-1", &[("identity", "h1")], &[7]);
        assert!(delivered(&mut record, "rpc-1", 1));
        assert_eq!(record["needsBriefing"], false, "a delivery clears the flag");
        next_generation(&mut record);
        assert_eq!(record["needsBriefing"], true, "a compaction asks for a full briefing again, as a persona switch does");
    }
}
