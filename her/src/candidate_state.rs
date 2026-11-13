//! Per-conversation retry cap and per-session sticky degraded candidate (ticket 25, AC3; decision Q5).
//!
//! The frozen JS keeps three maps beside its model chain (`bridge.js` 8424-8434, 11889-11934, 12079-12083):
//! `modelCandidateIndex` (session -> candidate currently in use), `modelAppliedSessions` (session -> selection
//! epoch already applied) and `failoverRetries` (conversation key -> failovers spent). This is the same state
//! as a small testable value:
//!
//! * the retry cap holds per conversation across messages, not per message: two failovers and the third
//!   failing turn gives up, until a turn completes and clears the count;
//! * a degraded candidate is sticky per session: the next message starts on the candidate the last one fell
//!   back to instead of re-trying a broken primary first (two wasted `selectModel` attempts and 1 s pauses
//!   per turn during an outage);
//! * a completed turn clears both, so the next message starts from the primary again;
//! * a change of the configured model or effort (`bump_epoch`) makes every session re-apply its selection;
//! * retiring a session forgets that it was applied, not which candidate it was on (as JS).
//!
//! `FailoverState::plan` returns the same [`FailoverAction`] as `failover::decide`; the state never decides
//! something `decide` would refuse. `pipeline.rs` holds one state per runtime (`Pipeline::failover_state`).
#![allow(dead_code)]

use crate::failover::{decide, FailoverAction};
use std::collections::HashMap;

/// JS `failoverRetries.get(key) >= 2` gives up.
pub const RETRY_CAP: usize = 2;

#[derive(Debug, Default)]
pub struct FailoverState {
    /// Selection epoch; a session whose applied epoch differs must `selectModel` again.
    epoch: u64,
    /// Conversation key -> failovers spent since the last completed turn.
    retries: HashMap<String, usize>,
    /// Session id -> candidate index currently in use.
    index: HashMap<String, usize>,
    /// Session id -> epoch whose selection was applied.
    applied: HashMap<String, u64>,
    /// Session id -> candidate DSH actually has selected (a completed turn clears `index` but not this).
    selected: HashMap<String, usize>,
    /// Session id -> fingerprint of the chain its selection was made from.
    chain_sig: HashMap<String, String>,
}

impl FailoverState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Does this session need a (re-)selection before the next prompt?
    pub fn needs_apply(&self, session: &str) -> bool {
        self.applied.get(session) != Some(&self.epoch)
    }

    /// The candidate to start from for this session: the sticky one, clamped to the current chain length.
    pub fn start_index(&self, session: &str, chain_len: usize) -> usize {
        self.index
            .get(session)
            .copied()
            .unwrap_or(0)
            .min(chain_len.saturating_sub(1))
    }

    /// `selectModel` succeeded on candidate `idx`.
    pub fn mark_applied(&mut self, session: &str, idx: usize) {
        self.applied.insert(session.to_string(), self.epoch);
        self.index.insert(session.to_string(), idx);
        self.selected.insert(session.to_string(), idx);
    }

    /// The candidate DSH has selected for this session (what actually answers), clamped to the chain.
    pub fn selected_index(&self, session: &str, chain_len: usize) -> usize {
        self.selected.get(session).copied().unwrap_or(0).min(chain_len.saturating_sub(1))
    }

    /// The configured chain for this session may have changed (model, provider, effort or fallbacks edited, per-session
    /// override): a different fingerprint than the one the selection was made from makes the session select again.
    pub fn note_chain(&mut self, session: &str, signature: &str) {
        match self.chain_sig.get(session) {
            Some(old) if old == signature => {}
            Some(_) => {
                self.applied.remove(session);
                self.index.remove(session);
                self.chain_sig.insert(session.to_string(), signature.to_string());
            }
            None => {
                self.chain_sig.insert(session.to_string(), signature.to_string());
            }
        }
    }

    /// Failovers spent for this conversation since its last completed turn.
    pub fn retries(&self, key: &str) -> usize {
        self.retries.get(key).copied().unwrap_or(0)
    }

    /// What to do after a failed turn on `session` (conversation `key`): `decide` over the sticky index and the
    /// conversation's retries. The state is not changed until [`FailoverState::on_failover`].
    pub fn plan(
        &self,
        key: &str,
        session: &str,
        chain_len: usize,
        already_sent: bool,
    ) -> FailoverAction {
        decide(
            chain_len,
            self.start_index(session, chain_len),
            self.retries(key),
            already_sent,
        )
    }

    /// A retry on candidate `next` is about to be made: spend one retry, remember the degraded candidate and
    /// force the selection to be applied again.
    pub fn on_failover(&mut self, key: &str, session: &str, next: usize) {
        *self.retries.entry(key.to_string()).or_insert(0) += 1;
        self.index.insert(session.to_string(), next);
        self.applied.remove(session);
    }

    /// The turn completed: the next message starts from the primary and with a fresh retry budget.
    pub fn on_completed(&mut self, key: &str, session: &str) {
        self.retries.remove(key);
        self.index.remove(session);
    }

    /// The configured model or effort changed: every session must select again.
    pub fn bump_epoch(&mut self) {
        self.epoch += 1;
    }

    /// The DSH session was retired (replaced or gone): it must be selected again, its degraded index is kept (JS).
    pub fn retire_session(&mut self, session: &str) {
        self.applied.remove(session);
    }

    /// The session is gone for good (reset, mode switch): drop everything known about it.
    pub fn forget_session(&mut self, session: &str) {
        self.applied.remove(session);
        self.index.remove(session);
        self.selected.remove(session);
        self.chain_sig.remove(session);
    }

    /// The conversation was reset: its retry budget starts over.
    pub fn forget_conversation(&mut self, key: &str) {
        self.retries.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover::GiveUp;

    #[test]
    fn a_fresh_session_starts_on_the_primary_and_needs_a_selection() {
        let s = FailoverState::new();
        assert!(s.needs_apply("sess"));
        assert_eq!(s.start_index("sess", 3), 0);
        assert_eq!(s.retries("group:1"), 0);
    }

    #[test]
    fn applying_records_the_candidate_and_stops_reapplying() {
        let mut s = FailoverState::new();
        s.mark_applied("sess", 1);
        assert!(!s.needs_apply("sess"));
        assert_eq!(s.start_index("sess", 3), 1);
    }

    #[test]
    fn a_degraded_candidate_is_sticky_until_a_turn_completes() {
        let mut s = FailoverState::new();
        s.mark_applied("sess", 0);
        assert_eq!(
            s.plan("group:1", "sess", 3, false),
            FailoverAction::Retry { idx: 1 }
        );
        s.on_failover("group:1", "sess", 1);
        assert!(s.needs_apply("sess"), "the new candidate must be selected");
        s.mark_applied("sess", 1);
        // the next message of the same session starts on the degraded candidate, not on the broken primary
        assert_eq!(s.start_index("sess", 3), 1);
        s.on_completed("group:1", "sess");
        assert_eq!(
            s.start_index("sess", 3),
            0,
            "a completed turn returns to the primary"
        );
        assert_eq!(s.retries("group:1"), 0);
    }

    #[test]
    fn the_retry_cap_holds_across_messages_of_one_conversation() {
        let mut s = FailoverState::new();
        // message 1: fails on 0, fails on 1
        s.mark_applied("sess", 0);
        assert_eq!(
            s.plan("group:1", "sess", 5, false),
            FailoverAction::Retry { idx: 1 }
        );
        s.on_failover("group:1", "sess", 1);
        s.mark_applied("sess", 1);
        assert_eq!(
            s.plan("group:1", "sess", 5, false),
            FailoverAction::Retry { idx: 2 }
        );
        s.on_failover("group:1", "sess", 2);
        s.mark_applied("sess", 2);
        // message 2 (no turn completed in between) fails again: cap of 2 already spent, even though candidates remain
        assert_eq!(
            s.plan("group:1", "sess", 5, false),
            FailoverAction::GiveUp(GiveUp::RetriesExhausted)
        );
        // another conversation has its own budget
        assert_eq!(
            s.plan("group:2", "other", 5, false),
            FailoverAction::Retry { idx: 1 }
        );
    }

    #[test]
    fn a_completed_turn_gives_the_conversation_a_fresh_budget() {
        let mut s = FailoverState::new();
        s.on_failover("group:1", "sess", 1);
        s.on_failover("group:1", "sess", 2);
        s.on_completed("group:1", "sess");
        assert!(matches!(
            s.plan("group:1", "sess", 5, false),
            FailoverAction::Retry { idx: 1 }
        ));
    }

    #[test]
    fn the_last_candidate_gives_up_and_a_sent_message_is_never_retried() {
        let mut s = FailoverState::new();
        s.mark_applied("sess", 2);
        assert_eq!(
            s.plan("k", "sess", 3, false),
            FailoverAction::GiveUp(GiveUp::LastCandidate)
        );
        s.mark_applied("sess", 0);
        assert_eq!(
            s.plan("k", "sess", 3, true),
            FailoverAction::GiveUp(GiveUp::AlreadySent)
        );
        assert_eq!(
            s.plan("k", "sess", 1, false),
            FailoverAction::GiveUp(GiveUp::NoFallbacks)
        );
    }

    #[test]
    fn a_shorter_chain_clamps_the_sticky_index() {
        let mut s = FailoverState::new();
        s.mark_applied("sess", 3);
        assert_eq!(s.start_index("sess", 2), 1, "config edit shrank the chain");
        assert_eq!(s.start_index("sess", 0), 0);
    }

    #[test]
    fn a_config_change_reapplies_every_session_but_keeps_the_degraded_index() {
        let mut s = FailoverState::new();
        s.mark_applied("a", 1);
        s.mark_applied("b", 0);
        s.bump_epoch();
        assert!(s.needs_apply("a") && s.needs_apply("b"));
        assert_eq!(
            s.start_index("a", 3),
            1,
            "as JS: the epoch forces re-selection, not a reset to the primary"
        );
    }

    #[test]
    fn retiring_a_session_forgets_that_it_was_applied_only() {
        let mut s = FailoverState::new();
        s.mark_applied("a", 1);
        s.retire_session("a");
        assert!(s.needs_apply("a"));
        assert_eq!(s.start_index("a", 3), 1);
        s.forget_session("a");
        assert_eq!(s.start_index("a", 3), 0);
    }

    #[test]
    fn a_reset_conversation_starts_a_fresh_budget() {
        let mut s = FailoverState::new();
        s.on_failover("group:1", "sess", 1);
        s.on_failover("group:1", "sess", 2);
        s.forget_conversation("group:1");
        assert_eq!(s.retries("group:1"), 0);
    }
}
