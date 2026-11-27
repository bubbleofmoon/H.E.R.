//! Live selected-runtime owner of the existing wake driver. Turns run independently of ingress.
use crate::{
    chat, inbound, inbound_text, pipeline, prompt,
    runtime::{RuntimeArea, RuntimeContext},
    session, social_state,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const MAX_WAKE_RETRIES: u32 = 3;

/// Harness and pinned CLI wakes can run during an outage if admission needs no DSH handover.
pub(crate) fn wake_needs_dsh(context: &RuntimeContext, cfg: &Value, key: &str) -> bool {
    match crate::turn_engine::Engine::selected(context, cfg, key, "reserved2") {
        Ok(engine) => (engine.uses_dsh() && pipeline::pinned_cli_backend(cfg, key).is_none())
            || crate::engine_switch::handover_needs_dsh(context, key, engine).unwrap_or(true),
        Err(_) => true,
    }
}

/// Re-arm a finite sleep timer when a provisional wake that carried no remaining `sleepUntil`
/// is dropped without scheduling a retry or hold.
fn rearm_dropped_provisional_sleep(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    at: i64,
    previous: &Value,
) -> Result<(), String> {
    if previous["wakeConfig"]["infinite"] != false || !previous["wakeConfig"]["sleepUntil"].is_null() {
        return Ok(());
    }
    let records = crate::turn_ledger::read_records(context)?;
    let (turns, _) = crate::turn_ledger::fold(&records);
    let fallback = json!({ "at": at, "previous": previous });
    let claim = turns
        .iter()
        .rev()
        .find(|t| t.key == key)
        .and_then(|t| t.wake_claim.as_ref())
        .filter(|c| {
            c["previous"]["wakeConfig"]["confirmedAt"] == previous["wakeConfig"]["confirmedAt"]
                && c["previous"]["wakeConfig"]["sleepUntil"].is_string()
        })
        .unwrap_or(&fallback);
    social_state::rearm_interrupted_reserved2_wake(
        context,
        cfg,
        key,
        Some(claim),
        None,
        Some(at),
    )
}

struct DispatchResult {
    result: Result<usize, String>,
    unsubmitted: bool,
    failure_parks: Option<bool>,
}

impl DispatchResult {
    fn new(result: Result<usize, String>, unsubmitted: bool) -> Self {
        Self { result, unsubmitted, failure_parks: None }
    }
}

struct Active {
    task: tokio::task::JoinHandle<DispatchResult>,
    inbound: Vec<String>,
    reason: String,
    previous: Value,
    at: i64,
    token: String,
    mode_stamp: String,
    /// The DSH session the turn uses, once known: a reset or mode change cancels exactly that one.
    session: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The token the turn itself minted when it abandoned an attempt (collector timeout).
    minted: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The hub cancelled this turn at its cap but could not stop its DSH work: the collector is
    /// aborted at this time (cap + `wake::COLLECTOR_HEADROOM_MS`) instead of its own backstop.
    cap_abort_at: Option<i64>,
    /// Admission replaces the queued route with the one the wake actually runs.
    route: pipeline::SharedReserved2Route,
    admitted: pipeline::MintedEpoch,
}

impl Active {
    fn route(&self) -> pipeline::Reserved2Route {
        *self.route.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn uses_harness(&self) -> bool {
        use pipeline::Reserved2Route::{ExpiredBeforeAdmission, Harness, PendingHarness};
        matches!(self.route(), PendingHarness | Harness | ExpiredBeforeAdmission)
    }

    fn preserve_harness_cap(&self) -> bool {
        use pipeline::Reserved2Route::{Cli, Dsh, ExpiredBeforeAdmission, Harness, PendingHarness};
        let mut route = self.route.lock().unwrap_or_else(|p| p.into_inner());
        match *route {
            PendingHarness => {
                *route = ExpiredBeforeAdmission;
                true
            }
            Harness | ExpiredBeforeAdmission => true,
            Dsh | Cli => false,
        }
    }

    fn admitted_token(&self) -> String {
        self.admitted.lock().unwrap_or_else(|p| p.into_inner()).clone()
            .unwrap_or_else(|| self.token.clone())
    }

    fn session(&self) -> Option<String> {
        self.session.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Wakes waiting for DSH, per conversation: at most `dsh_health::QUEUE_MAX` each (the oldest of that
/// conversation dropped beyond it). There is no process-wide cap: one busy conversation never costs another its wake.
#[derive(Default)]
struct DshWait {
    per_key: HashMap<String, std::collections::VecDeque<(i64, String)>>,
}

impl DshWait {
    /// Queue a wake; returns the conversation's oldest wake when it was full and had to drop it.
    fn push(&mut self, key: &str, since: i64, reason: String) -> Option<(i64, String)> {
        let queue = self.per_key.entry(key.to_string()).or_default();
        let dropped = if queue.len() >= crate::dsh_health::QUEUE_MAX { queue.pop_front() } else { None };
        queue.push_back((since, reason));
        dropped
    }
    fn len(&self, key: &str) -> usize {
        self.per_key.get(key).map_or(0, |q| q.len())
    }
    fn remove(&mut self, key: &str) {
        self.per_key.remove(key);
    }
    fn clear(&mut self) {
        self.per_key.clear();
    }
    fn is_empty(&self) -> bool {
        self.per_key.is_empty()
    }
    /// Eligible waiting wakes as `(conversation, reason)`, oldest first.
    fn drain_ready(&mut self, ready: impl Fn(&str) -> bool) -> Vec<(String, String)> {
        let keys: Vec<String> = self.per_key.keys().filter(|key| ready(key)).cloned().collect();
        let mut all: Vec<(i64, String, String)> = keys.into_iter().flat_map(|key| {
            self.per_key.remove(&key).unwrap().into_iter().map(move |(t, reason)| (t, key.clone(), reason))
        }).collect();
        all.sort_by_key(|(t, _, _)| *t);
        all.into_iter().map(|(_, k, r)| (k, r)).collect()
    }

    #[cfg(test)]
    fn drain(&mut self) -> Vec<(String, String)> {
        self.drain_ready(|_| true)
    }
}

/// `claim_reserved2_wake` refusal: the conversation's token changed after the wake was queued.
const STALE_WAKE: &str = "queued wake token invalidated";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{console, conversation_state};

    async fn queued_hub(label: &str) -> (std::path::PathBuf, Hub) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-{label}-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data/logs", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), json!({"consoleToken":"synthetic-review-token","allow":{"groups":[420042,420043]},"socialV2":{"proactive":{"enabled":false},"wake":{"maxWakePerMinute":30,"maxWakePerHour":100}}}).to_string()).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let mut state = json!({"conversations":{}});
        for key in ["group:420042", "group:420043"] {
            state["conversations"][key] = json!({"agentToken":"test-token","bootstrapSent":true,"unread":[{"seq":1}],"wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}});
        }
        std::fs::write(root.join("data/social-v2.json"), state.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&context).unwrap());
        let mut hub = Hub::new(&context, journal, vec![], true);
        hub.tick("bot", 9001).await.unwrap();
        for key in ["group:420042", "group:420043"] {
            let event = format!("{key}|message:1");
            hub.journal.record_received(event.clone(), key.into(), "{}".into(), 1).await.unwrap();
            hub.awaiting.insert(key.into(), vec![event]);
            hub.ready.push((key.into(), "atMention".into()));
        }
        (root, hub)
    }

    #[tokio::test]
    async fn schedule_due_reminder_is_claimed_once_after_restart_and_paused_reminders_wait() {
        use crate::schedule::{Creator, NewItem, Repeat, ScheduleStore};
        let (root, mut hub) = queued_hub("schedule-reminder").await;
        let cfg = hub.context.load_config().unwrap();
        let store = ScheduleStore::new(&hub.context);
        let now = crate::now_ms() as i64;
        let at = now + 60_000;
        let every = store
            .create(NewItem { creator: Creator::Owner, conversation: "group:420042".into(), text: "喝水".into(), at, repeat: Repeat::Every { minutes: 60 } }, now)
            .unwrap();
        let paused = store
            .create(NewItem { creator: Creator::Owner, conversation: "group:420043".into(), text: "开会".into(), at, repeat: Repeat::Once }, now)
            .unwrap();
        store.set_item_paused(&paused.id, true).unwrap();
        hub.ready.clear();

        // The hub keeps no reminder state: a later clock over the same store file is the restart.
        let later = now + 3 * 86_400_000;
        hub.sync_schedule(&cfg, false, later);
        assert_eq!(hub.ready, vec![("group:420042".to_string(), format!("reminder:{}", every.id))]);
        hub.ready.clear();
        hub.sync_schedule(&cfg, false, later + 1_000);
        assert!(hub.ready.is_empty(), "a claimed repeat is not claimed again");
        let stored = store.item(&every.id).unwrap().unwrap();
        assert!(stored.next_at > later, "a missed repeat jumps to the next slot after now");
        assert!(stored.last_result.unwrap().late);

        store.run_now(&paused.id).unwrap();
        hub.sync_schedule(&cfg, false, later + 2_000);
        assert_eq!(hub.ready, vec![("group:420043".to_string(), format!("reminder:{}", paused.id))]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn schedule_publishes_her_timers_and_applies_console_cancels() {
        use crate::schedule::{ScheduleStore, TimerKind};
        let (root, mut hub) = queued_hub("schedule-timers").await;
        let cfg = hub.context.load_config().unwrap();
        let key = "group:420042";
        let now = crate::now_ms() as i64;
        hub.ready.clear();
        hub.drivers.get_mut(key).unwrap().sched.reply_check_at = Some(now + 600_000);

        let lists_reply_check = |root: &std::path::Path| {
            let published: Value = serde_json::from_slice(&std::fs::read(root.join("data/schedule-timers.json")).unwrap()).unwrap();
            published["timers"].as_array().unwrap().iter().any(|timer| timer["conversation"] == key && timer["kind"] == "replyCheck")
        };
        hub.sync_schedule(&cfg, false, now);
        assert!(lists_reply_check(&root), "her reply check is published");

        ScheduleStore::new(&hub.context).request_timer_cancel(key, TimerKind::ReplyCheck, now + 600_000, now).unwrap();
        hub.pump_timers(&cfg, now);
        hub.sync_schedule(&cfg, false, now);
        assert_eq!(hub.drivers[key].sched.reply_check_at, None, "the console cancel clears the timer");
        assert!(!lists_reply_check(&root), "the snapshot no longer lists the cancelled timer");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn schedule_cancel_applies_before_timers_fire_and_only_to_the_timer_seen() {
        use crate::schedule::{ScheduleStore, TimerKind};
        let (root, mut hub) = queued_hub("schedule-cancel-order").await;
        let cfg = hub.context.load_config().unwrap();
        let key = "group:420042";
        let now = crate::now_ms() as i64;
        let store = ScheduleStore::new(&hub.context);
        hub.ready.clear();
        {
            let d = hub.drivers.get_mut(key).unwrap();
            d.sched.pending_wake_at = None;
            d.sched.pending_wake_key_at = None;
            d.sched.prompt_running = false;
            d.sched.prompt_queued = false;
            d.sched.reply_check_at = Some(now - 1_000);
        }
        // Publish what the owner sees (suspended: nothing is claimed), then cancel that due timer.
        hub.sync_schedule(&cfg, true, now - 2_000);
        store.request_timer_cancel(key, TimerKind::ReplyCheck, now - 1_000, now - 1_500).unwrap();
        hub.pump_timers(&cfg, now);
        assert_eq!(hub.drivers[key].sched.reply_check_at, None);
        assert!(!hub.ready.iter().any(|(k, r)| k == key && r == "replyCheck"), "a cancelled reply check never fires: {:?}", hub.ready);

        // A cancel names the timer the owner saw: a timer re-armed since is left alone.
        hub.drivers.get_mut(key).unwrap().sched.reply_check_at = Some(now + 600_000);
        hub.sync_schedule(&cfg, true, now);
        store.request_timer_cancel(key, TimerKind::ReplyCheck, now + 600_000, now).unwrap();
        hub.drivers.get_mut(key).unwrap().sched.reply_check_at = Some(now + 900_000);
        hub.pump_timers(&cfg, now);
        assert_eq!(hub.drivers[key].sched.reply_check_at, Some(now + 900_000));

        // Reach-out: the cancelled chance is skipped and the next one is armed from it, never sooner.
        let chance = now + 600_000;
        {
            let d = hub.drivers.get_mut(key).unwrap();
            d.sched.cfg.proactive_enabled = true;
            d.sched.proactive_at = Some(chance);
        }
        hub.sync_schedule(&cfg, true, now + 1);
        store.request_timer_cancel(key, TimerKind::ReachOut, chance, now + 1).unwrap();
        hub.pump_timers(&cfg, now + 1);
        let next = hub.drivers[key].sched.proactive_at.expect("the next chance is armed");
        assert!(next > chance, "the next chance comes after the cancelled one: {next} <= {chance}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn schedule_cancels_that_cannot_be_read_defer_due_timers_until_they_can() {
        use crate::schedule::{ScheduleStore, TimerKind};
        let (root, mut hub) = queued_hub("schedule-cancel-unreadable").await;
        let cfg = hub.context.load_config().unwrap();
        let key = "group:420042";
        let now = crate::now_ms() as i64;
        let store = ScheduleStore::new(&hub.context);
        hub.ready.clear();
        {
            let d = hub.drivers.get_mut(key).unwrap();
            d.sched.pending_wake_at = None;
            d.sched.pending_wake_key_at = None;
            d.sched.prompt_running = false;
            d.sched.prompt_queued = false;
            d.sched.reply_check_at = Some(now - 1_000);
        }
        // The owner cancels the due reply check; the cancel is persisted.
        hub.sync_schedule(&cfg, true, now - 2_000);
        store.request_timer_cancel(key, TimerKind::ReplyCheck, now - 1_000, now - 1_500).unwrap();
        let persisted = std::fs::read(root.join("data/schedule.json")).unwrap();

        // The store becomes unreadable: the cancel cannot be taken, so the due timer waits instead of firing.
        std::fs::write(root.join("data/schedule.json"), "not json").unwrap();
        hub.pump_timers(&cfg, now);
        assert!(!hub.ready.iter().any(|(k, r)| k == key && r == "replyCheck"), "an unreadable cancel must not let the timer fire: {:?}", hub.ready);
        assert_eq!(hub.drivers[key].sched.reply_check_at, Some(now - 1_000), "the due timer is kept for a later tick");

        // The store reads again with the cancel in it: the next tick applies it, and the timer never fires.
        std::fs::write(root.join("data/schedule.json"), &persisted).unwrap();
        hub.pump_timers(&cfg, now);
        assert_eq!(hub.drivers[key].sched.reply_check_at, None, "the cancel is applied on the first readable tick");
        assert!(!hub.ready.iter().any(|(k, r)| k == key && r == "replyCheck"), "a cancelled reply check never fires: {:?}", hub.ready);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn schedule_reach_out_cancel_that_cannot_be_read_defers_the_due_chance_until_it_can() {
        use crate::schedule::{ScheduleStore, TimerKind};
        let (root, mut hub) = queued_hub("schedule-reachout-unreadable").await;
        let cfg = hub.context.load_config().unwrap();
        let key = "group:420042";
        let now = crate::now_ms() as i64;
        let store = ScheduleStore::new(&hub.context);
        hub.ready.clear();
        let due = now - 1_000;
        {
            let d = hub.drivers.get_mut(key).unwrap();
            d.sched.cfg.proactive_enabled = true;
            d.sched.pending_wake_at = None;
            d.sched.pending_wake_key_at = None;
            d.sched.prompt_running = false;
            d.sched.prompt_queued = false;
            d.sched.proactive_at = Some(due);
        }
        // The owner skips the due reach-out chance; the cancel is persisted.
        hub.sync_schedule(&cfg, true, now - 2_000);
        store.request_timer_cancel(key, TimerKind::ReachOut, due, now - 1_500).unwrap();
        let persisted = std::fs::read(root.join("data/schedule.json")).unwrap();

        // Unreadable store: the chance is kept as it was, and nothing is announced for it.
        std::fs::write(root.join("data/schedule.json"), "not json").unwrap();
        hub.pump_timers(&cfg, now);
        assert_eq!(hub.drivers[key].sched.proactive_at, Some(due), "the due reach-out is deferred, not re-rolled");
        assert!(!hub.ready.iter().any(|(k, r)| k == key && r == "proactiveCheck"), "a cancelled reach-out does not fire: {:?}", hub.ready);

        // Readable again: the cancel skips the chance, and the next one is armed after it.
        std::fs::write(root.join("data/schedule.json"), &persisted).unwrap();
        hub.pump_timers(&cfg, now);
        let next = hub.drivers[key].sched.proactive_at.expect("the next chance is armed");
        assert!(next > due, "the next chance comes after the cancelled one: {next} <= {due}");
        assert!(!hub.ready.iter().any(|(k, r)| k == key && r == "proactiveCheck"), "a cancelled reach-out never fires: {:?}", hub.ready);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn schedule_reminders_wait_while_dsh_is_down_or_a_wake_is_parked_then_run_late_once() {
        use crate::schedule::{Creator, NewItem, Repeat, ScheduleStore};
        let (root, mut hub) = queued_hub("schedule-dsh-wait").await;
        let cfg = hub.context.load_config().unwrap();
        let store = ScheduleStore::new(&hub.context);
        let now = crate::now_ms() as i64;
        let hourly = store
            .create(NewItem { creator: Creator::Owner, conversation: "group:420042".into(), text: "喝水".into(), at: now + 60_000, repeat: Repeat::Every { minutes: 60 } }, now)
            .unwrap();
        let pinned = store
            .create(NewItem { creator: Creator::Owner, conversation: "group:420043".into(), text: "开会".into(), at: now + 60_000, repeat: Repeat::Once }, now)
            .unwrap();
        hub.ready.clear();

        // A parked wake holds the conversation's reminders back.
        hub.dsh_wait.push("group:420042", now, "atMention".into());
        hub.dsh_wait.push("group:420043", now, "atMention".into());
        hub.sync_schedule(&cfg, false, now + 120_000);
        assert!(hub.ready.is_empty(), "nothing is claimed beside a parked wake: {:?}", hub.ready);
        hub.dsh_wait.clear();

        // Ten hours of DSH outage: no slot is claimed, except for a conversation with a pinned CLI.
        crate::dsh_health::observe(&hub.context, &cfg, false, "synthetic outage", true);
        let mut pinned_cfg = cfg.clone();
        pinned_cfg["backends"] = json!({"sessions": {"group:420043": "cli-fixture"}});
        for hour in 1..=10_i64 {
            hub.sync_schedule(&pinned_cfg, false, now + hour * 3_600_000);
        }
        assert_eq!(hub.ready, vec![("group:420043".to_string(), format!("reminder:{}", pinned.id))], "only the pinned CLI conversation is claimed");
        hub.ready.clear();
        let waiting = store.item(&hourly.id).unwrap().unwrap();
        assert!(waiting.last_result.is_none(), "no slot was marked run during the outage");
        assert_eq!(waiting.next_at, now + 60_000);

        // Recovery: one late run, then nothing more (no catch-up burst).
        crate::dsh_health::observe(&hub.context, &cfg, true, "", true);
        let later = now + 10 * 3_600_000 + 1_000;
        hub.sync_schedule(&cfg, false, later);
        assert_eq!(hub.ready, vec![("group:420042".to_string(), format!("reminder:{}", hourly.id))]);
        let ran = store.item(&hourly.id).unwrap().unwrap();
        assert!(ran.last_result.unwrap().late);
        assert!(ran.next_at > later);
        hub.ready.clear();
        hub.sync_schedule(&cfg, false, later + 1_000);
        assert!(hub.ready.is_empty(), "missed repeats are not caught up");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hf135_partial_reset_keeps_maintenance_and_queued_ingress_until_revoked() {
        use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
        let (root,mut hub) = queued_hub("hf135-partial-reset").await;
        let key="group:420042";
        let (stop,server)=fake_dsh(&root,&mut hub,key,false);
        let cfg=hub.context.load_config().unwrap();
        crate::session_map::update_context(&hub.context,key,"cap-session","test-token",|r| {
            r["lastTurnSeq"]=json!(5);
            r["rotation"]=json!({"phase":"rotate","turnSeq":5,"retireUnknown":true});
        }).unwrap();
        let plan=crate::session_rotation::candidate(&hub.context,&cfg,key,crate::now_ms() as i64).unwrap().unwrap();
        let task=tokio::spawn(async {MaintenanceResult::Digest(crate::session_rotation::DigestResult::Done{ran:true,retire_unknown:true})});
        while !task.is_finished() {tokio::task::yield_now().await;}
        hub.maintenance.insert(key.into(),Maintenance{plan:plan.clone(),cancel:Arc::new(AtomicBool::new(false)),task,digested:false});
        hub.maintenance_scan_at=crate::now_ms() as i64+60_000;
        let path=root.join("data/social-v2.json");
        let backup=root.join("data/social-before-reset.json");
        let blocked=path.clone(); let saved=backup.clone();
        crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|slot| *slot.borrow_mut()=Some(Box::new(move || {
            std::fs::rename(&blocked,&saved).unwrap();std::fs::create_dir(&blocked).unwrap();
        })));
        let result=hub.maintain_sessions(&cfg,false,crate::now_ms() as i64).await;
        crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|slot| *slot.borrow_mut()=None);
        std::fs::remove_dir(&path).unwrap();std::fs::rename(&backup,&path).unwrap();
        result.unwrap();
        assert!(hub.maintenance.contains_key(key));
        assert!(hub.active.is_empty());
        assert_eq!(social_state::token(&hub.context,key).unwrap().as_deref(),Some("test-token"));
        // Run the next verifier immediately, keeping the normal maintained result handling.
        hub.maintenance.remove(key).unwrap().task.abort();
        let result=crate::session_rotation::digest(hub.context.clone(),cfg.clone(),plan.clone(),Arc::new(AtomicBool::new(false))).await;
        if !matches!(result,crate::session_rotation::DigestResult::Done{ran:true,retire_unknown:true}) {
            let _=stop.send(());server.join().unwrap();drop(hub);std::fs::remove_dir_all(root).unwrap();
            panic!("mapping removal released old authority");
        }
        let task=tokio::spawn(async move {MaintenanceResult::Digest(result)});
        while !task.is_finished() {tokio::task::yield_now().await;}
        hub.maintenance.insert(key.into(),Maintenance{plan,cancel:Arc::new(AtomicBool::new(true)),task,digested:false});
        hub.maintain_sessions(&cfg,false,crate::now_ms() as i64).await.unwrap();
        assert_ne!(social_state::token(&hub.context,key).unwrap().as_deref(),Some("test-token"));
        assert_eq!(hub.tokens[key],social_state::token(&hub.context,key).unwrap().unwrap());
        assert_eq!(hub.awaiting[key],vec![format!("{key}|message:1")]);
        let event=format!("{key}|message:1");
        assert_eq!(hub.journal.run(move |j|j.stage_of(&event)).await.unwrap().as_deref(),Some("received"));
        assert!(hub.active.is_empty());
        for m in hub.maintenance.values() {m.cancel.store(true,std::sync::atomic::Ordering::SeqCst);m.task.abort();}
        let _=stop.send(());let methods=server.join().unwrap();
        assert!(!methods.iter().any(|m|m=="session/prompt" || m=="session/create"));
        drop(hub);std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn hf135_retirement_read_failure_holds_ingress_and_retries_next_tick() {
        use std::sync::{Arc, atomic::AtomicBool};
        let (root, mut hub) = queued_hub("hf135-retirement-read").await;
        let key = "group:420042";
        hub.ready.retain(|(k, _)| k == key);
        let (stop, server) = fake_dsh(&root, &mut hub, key, false);
        let cfg = hub.context.load_config().unwrap();
        crate::session_map::update_context(&hub.context, key, "cap-session", "test-token", |r| {
            r["lastTurnSeq"] = json!(5);
            r["rotation"] = json!({"phase":"rotate","turnSeq":5,"retireUnknown":true});
        }).unwrap();
        crate::session_map::retire_if(&hub.context, key, "cap-session").unwrap();
        let plan = crate::session_rotation::candidate(&hub.context, &cfg, key, crate::now_ms() as i64).unwrap().unwrap();
        let task = tokio::spawn(async {
            MaintenanceResult::Digest(crate::session_rotation::DigestResult::Done { ran: true, retire_unknown: true })
        });
        while !task.is_finished() { tokio::task::yield_now().await; }
        hub.maintenance.insert(key.into(), Maintenance {
            plan, cancel: Arc::new(AtomicBool::new(false)), task, digested: false,
        });
        hub.maintenance_scan_at = crate::now_ms() as i64 + 60_000;
        // The cancellation check reads successfully; the following retirement guard read fails once.
        hub.state_read_failure_after.set(Some(1));
        let failed = hub.maintain_sessions(&cfg, false, crate::now_ms() as i64).await;
        assert_eq!(failed.unwrap_err(), "synthetic social-state read failure");
        let retained = hub.maintenance.contains_key(key);
        if !retained {
            let _ = stop.send(()); server.join().unwrap();
            drop(hub); std::fs::remove_dir_all(root).unwrap();
            panic!("retirement guard read failure released maintenance before token revocation");
        }
        assert!(hub.active.is_empty());
        assert!(crate::session_map::lookup(&hub.context, key).unwrap().is_none());
        assert_eq!(social_state::token(&hub.context, key).unwrap().as_deref(), Some("test-token"));
        // Retry through the real tick, while the periodic scan is still throttled.
        hub.tick("bot", 9001).await.unwrap();
        assert!(hub.active.is_empty(), "queued ingress dispatched before retirement completed");
        assert_ne!(social_state::token(&hub.context, key).unwrap().as_deref(), Some("test-token"));
        assert_eq!(hub.awaiting[key], vec![format!("{key}|message:1")]);
        let event = format!("{key}|message:1");
        assert_eq!(hub.journal.run(move |j| j.stage_of(&event)).await.unwrap().as_deref(), Some("received"));
        let memory = console::memory_tool_action(&hub.context, "write", &json!({
            "key":key,"token":"test-token","content":"late digest memory","subject":"test"
        }));
        assert!(memory.unwrap_err().contains("token"));
        let (status, _) = console::admin_request(&hub.context, "POST", "/api/send/group", Some(&json!({
            "groupId":"420042","token":"test-token","message":"late digest send"
        })));
        assert_eq!(status, 403);
        for m in hub.maintenance.values() { m.task.abort(); }
        let _ = stop.send(()); let methods = server.join().unwrap();
        assert!(!methods.iter().any(|m| m == "session/prompt" || m == "session/create"));
        drop(hub); std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn hf135_recovery_scan_read_failure_retries_before_dispatch() {
        let (root, mut hub) = queued_hub("hf135-recovery-scan-read").await;
        let key = "group:420042";
        hub.ready.retain(|(k, _)| k == key);
        let (stop, server) = fake_dsh(&root, &mut hub, key, false);
        let cfg = hub.context.load_config().unwrap();
        crate::session_map::update_context(&hub.context, key, "cap-session", "test-token", |r| {
            r["lastTurnSeq"] = json!(5);
            r["rotation"] = json!({"phase":"rotate","turnSeq":5,"retireUnknown":true});
        }).unwrap();
        crate::session_map::retire_if(&hub.context, key, "cap-session").unwrap();
        hub.maintenance_scan_at = 0;
        hub.state_read_failure_after.set(Some(0));
        let failed = hub.maintain_sessions(&cfg, false, crate::now_ms() as i64).await;
        assert_eq!(failed.unwrap_err(), "synthetic social-state read failure");
        // A failed scan must remain due: the next tick reserves recovery before dispatch.
        hub.tick("bot", 9001).await.unwrap();
        let dispatched = !hub.active.is_empty();
        let recovering = hub.maintenance.contains_key(key);
        let event = format!("{key}|message:1");
        let stage = hub.journal.run(move |j| j.stage_of(&event)).await.unwrap();
        for a in hub.active.values() { a.task.abort(); }
        for m in hub.maintenance.values() { m.task.abort(); }
        let _ = stop.send(()); server.join().unwrap();
        drop(hub); std::fs::remove_dir_all(root).unwrap();
        assert!(!dispatched, "recovery scan read failure allowed queued ingress to dispatch");
        assert!(recovering, "failed recovery scan was throttled on the next tick");
        assert_eq!(stage.as_deref(), Some("received"));
    }

    async fn persistent_rotation_read_failure(scan: bool) {
        use std::sync::{Arc, atomic::AtomicBool};
        let (root, mut hub) = queued_hub("hf135-persistent-read").await;
        let key = "group:420042";
        hub.ready.retain(|(k, _)| k == key);
        let (stop, server) = fake_dsh(&root, &mut hub, key, false);
        let cfg = hub.context.load_config().unwrap();
        crate::session_map::update_context(&hub.context, key, "cap-session", "test-token", |r| {
            r["lastTurnSeq"] = json!(5);
            r["rotation"] = json!({"phase":"rotate","turnSeq":5,"retireUnknown":true});
        }).unwrap();
        crate::session_map::retire_if(&hub.context, key, "cap-session").unwrap();
        let now = crate::now_ms() as i64;
        if !scan {
            let plan = crate::session_rotation::candidate(&hub.context, &cfg, key, now).unwrap().unwrap();
            let task = tokio::spawn(async {
                MaintenanceResult::Digest(crate::session_rotation::DigestResult::Done { ran: true, retire_unknown: true })
            });
            while !task.is_finished() { tokio::task::yield_now().await; }
            hub.maintenance.insert(key.into(), Maintenance {
                plan, cancel: Arc::new(AtomicBool::new(false)), task, digested: false,
            });
        }
        hub.maintenance_scan_at = if scan { 0 } else { now + 1_000_000 };
        for attempt in 0..24 {
            hub.state_read_failure_after.set(Some(if scan { 0 } else { 1 }));
            assert!(hub.maintain_sessions(&cfg, false, now + attempt * 5_000).await.is_err());
            assert!(hub.active.is_empty());
            assert_eq!(social_state::token(&hub.context, key).unwrap().as_deref(), Some("test-token"));
            assert_eq!(hub.awaiting[key], vec![format!("{key}|message:1")]);
        }
        hub.state_read_failure_after.set(Some(0));
        let deferred = hub.tick("bot", 9001).await;
        let skipped_read = hub.state_read_failure_after.get() == Some(0);
        let log = crate::activity::tail(&hub.context, 100);
        let claimed = Value::Object(crate::session_map::read(&hub.context).unwrap())["rotationAlerts"][key].is_object();
        // A healthy read after the slow interval still completes the guarded retirement.
        hub.state_read_failure_after.set(None);
        let recovered_at = now + 180_000;
        hub.maintain_sessions(&cfg, false, recovered_at).await.unwrap();
        if scan {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !hub.maintenance[key].task.is_finished() { tokio::task::yield_now().await; }
            }).await.unwrap();
            hub.maintain_sessions(&cfg, false, recovered_at).await.unwrap();
        }
        let revoked = social_state::token(&hub.context, key).unwrap().as_deref() != Some("test-token");
        assert_eq!(crate::session_rotation::read_retry_ms(&hub.context, key), 60_000,
            "healthy reads and token revocation alone must retain the failure count");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !hub.maintenance[key].task.is_finished() { tokio::task::yield_now().await; }
        }).await.unwrap();
        hub.maintain_sessions(&cfg, false, recovered_at).await.unwrap();
        assert!(!hub.maintenance.contains_key(key));
        assert_eq!(crate::session_rotation::read_retry_ms(&hub.context, key), 5_000,
            "completed recovery must reset the failure count");
        // A later recovery begins with a fresh bound, rather than inheriting escalation.
        hub.state_read_failure_after.set(Some(0));
        assert!(hub.rotation_state(&cfg, "guard-read", recovered_at + 5_000).is_err());
        assert_eq!(crate::session_rotation::read_retry_ms(&hub.context, key), 5_000);
        hub.state_read_failure_after.set(None);
        let event = format!("{key}|message:1");
        let stage = hub.journal.run(move |j| j.stage_of(&event)).await.unwrap();
        for m in hub.maintenance.values() { m.task.abort(); }
        let _ = stop.send(()); let methods = server.join().unwrap();
        drop(hub); std::fs::remove_dir_all(root).unwrap();
        assert_eq!(log.lines().filter(|line| line.contains(key) && line.contains("kind=storage")).count(), 1, "persistent guard/scan failure needs one content-free diagnostic");
        assert!(!log.contains("synthetic social-state read failure"));
        assert!(claimed, "persistent guard/scan failure needs a daily owner notice claim");
        assert!(deferred.is_ok() && skipped_read, "slow retry must hold dispatch without reading or error spam");
        assert!(revoked);
        assert_eq!(stage.as_deref(), Some("received"));
        assert!(!methods.iter().any(|m| m == "session/prompt" || m == "session/create"));
    }

    #[tokio::test]
    async fn hf135_persistent_guard_read_failure_alerts_once_and_recovers() {
        persistent_rotation_read_failure(false).await;
    }

    #[tokio::test]
    async fn hf135_persistent_scan_read_failure_alerts_once_and_recovers() {
        persistent_rotation_read_failure(true).await;
    }

    #[tokio::test]
    async fn hf135_restart_reserves_partial_retirement_during_dsh_outage() {
        let (root,mut hub)=queued_hub("hf135-restart-retirement").await;
        let key="group:420042";
        crate::session_map::bind(&hub.context,key,"old-session",None).unwrap();
        crate::session_map::update_context(&hub.context,key,"old-session","test-token",|r| {
            r["lastTurnSeq"]=json!(5);
            r["rotation"]=json!({"phase":"rotate","turnSeq":5,"retireUnknown":true});
        }).unwrap();
        crate::session_map::retire_if(&hub.context,key,"old-session").unwrap();
        let journal=hub.journal.clone();drop(hub);
        let context=RuntimeContext::from_root(&root).unwrap();
        let cfg=context.load_config().unwrap();
        crate::dsh_health::observe(&context,&cfg,false,"synthetic outage",true);
        hub=Hub::new(&context,journal,vec![],true);
        hub.awaiting.insert(key.into(),vec![format!("{key}|message:1")]);
        hub.ready.push((key.into(),"atMention".into()));
        hub.maintain_sessions(&cfg,false,crate::now_ms() as i64).await.unwrap();
        let reserved=hub.maintenance.contains_key(key);
        assert!(hub.active.is_empty());
        let event=format!("{key}|message:1");
        assert_eq!(hub.journal.run(move |j|j.stage_of(&event)).await.unwrap().as_deref(),Some("received"));
        for m in hub.maintenance.values() {m.task.abort();}
        drop(hub);std::fs::remove_dir_all(root).unwrap();
        assert!(reserved,"restart dispatched without recovering unrevoked retirement");
    }

    async fn assert_predispatch_retry(hub: &mut Hub) {
        let result = hub.tick("bot", 9001).await;
        assert!(result.is_ok(), "a failed job escaped the dispatch loop: {result:?}");
        assert!(hub.active.is_empty());
        for key in ["group:420042", "group:420043"] {
            assert_eq!(hub.awaiting[key], vec![format!("{key}|message:1")]);
            assert!(hub.retries.contains_key(key), "drained job lost: {key}");
            assert!(!hub.drivers[key].sched.is_busy(crate::now_ms() as i64));
            let event = format!("{key}|message:1");
            assert_eq!(hub.journal.run(move |j| j.stage_of(&event)).await.unwrap().as_deref(), Some("received"));
            assert_eq!(hub.state().unwrap()["conversations"][key]["wakeConfig"]["wakeCount"], Value::Null);
        }
    }

    #[tokio::test]
    async fn a_long_inbound_message_keeps_its_middle_readable_beside_the_200_unit_preview() {
        let (root, mut hub) = queued_hub("long-message").await;
        let key = "group:420042";
        let long = format!("{}别告诉任何人这件事{}", "前".repeat(450), "后".repeat(450));
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":29101,"message":[{"type":"text","data":{"text":long}}]});
        let ev = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
        let state = hub.state().unwrap();
        let stored = state["conversations"][key]["recentMessages"].as_array().unwrap().last().unwrap().clone();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(stored["text"].as_str().unwrap().encode_utf16().count(), 200, "the preview stays 200 units");
        assert_eq!(stored["fullText"], json!(long), "the whole message stays readable");
        assert_eq!(stored["textChars"], 909);
        assert_eq!(stored["textTruncated"], true);
    }

    #[tokio::test]
    async fn a_long_message_stores_its_full_text_once_not_in_every_list_that_holds_it() {
        let (root, mut hub) = queued_hub("long-message-once").await;
        let key = "group:420042";
        let long = format!("{}别告诉任何人这件事{}", "前".repeat(450), "后".repeat(450));
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":29102,"message":[{"type":"text","data":{"text":long}}]});
        let ev = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
        let state = hub.state().unwrap();
        let conv = state["conversations"][key].clone();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        let (recent, unread) = (conv["recentMessages"].as_array().unwrap().last().unwrap(), conv["unread"].as_array().unwrap().last().unwrap());
        assert_eq!(recent["fullText"], json!(long), "message detail reads the recent list");
        assert!(unread.get("fullText").is_none(), "the unread copy carries only the preview: {unread}");
        assert_eq!((unread["textChars"].clone(), unread["textTruncated"].clone()), (json!(909), json!(true)), "but still says it was cut");
        assert_eq!(unread["seq"], recent["seq"]);
    }

    #[tokio::test]
    async fn an_opted_in_voice_record_is_stored_as_its_transcript_and_other_conversations_keep_the_label() {
        let (root, mut hub) = queued_hub("voice-transcript").await;
        let engine = crate::voice_stt::tests::fake_engine(Some(crate::voice_stt::tests::ok_reply(r#"{"text":"<|zh|><|NEUTRAL|>今天吃什么"}"#)));
        let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(root.join("config/config.json")).unwrap()).unwrap();
        cfg["voiceTranscription"] = json!({"conversations": ["group:420042"], "helper": engine.origin, "timeoutMs": 5000});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        let mut stored = Vec::new();
        for (group, id) in [(420042, 29301), (420043, 29302)] {
            let url = format!("https://voice.fixture.test/{id}");
            crate::voice_stt::DOWNLOAD_FIXTURES.lock().unwrap().insert(url.clone(), crate::voice_stt::tests::silk(40));
            let frame = json!({"post_type":"message","message_type":"group","group_id":group,"user_id":1001,
                "message_id":id,"message":[{"type":"record","data":{"file":"0f0e.amr","url":url}}]});
            let ev = serde_json::from_value(frame.clone()).unwrap();
            hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
            // A replay of the same event (a restart before it was settled) reads the kept transcript.
            hub.handle(&ev, &frame.to_string(), true, 9001, "bot", &HashMap::new()).await.unwrap();
            let state = hub.state().unwrap();
            let key = format!("group:{group}");
            stored.push(state["conversations"][key.as_str()]["recentMessages"].as_array().unwrap().last().unwrap().clone());
        }
        let requests = engine.requests.lock().unwrap().len();
        let activity = std::fs::read_to_string(root.join("data/qq-activity.log")).unwrap_or_default();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        let heard = crate::voice_stt::label("今天吃什么");
        assert!(
            activity.contains("group:420042 [reserved2] 收到一则新的会话来信，已经顺手记入未读待办列表以便稍后处理：[语音]"),
            "the activity log keeps the voice label: {activity}"
        );
        assert!(!activity.contains("今天吃什么"), "the transcript never reaches the activity log: {activity}");
        assert_eq!(
            (stored[0]["text"].as_str(), stored[0]["plain"].as_str()),
            (Some(heard.as_str()), Some(heard.as_str())),
            "the opted-in voice is stored as its transcript: {}",
            stored[0]
        );
        assert_eq!(stored[1]["text"], "[语音]", "a conversation that did not opt in keeps the label: {}", stored[1]);
        assert_eq!(requests, 1, "one transcription: the replay read the kept transcript and the other conversation asked nothing");
    }

    #[tokio::test]
    async fn timeout_during_ingress_cancels_once_and_preserves_buffered_work() {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        for ingress in [true, false] {
            let (root, mut hub) = queued_hub("cap-control").await;
            let key = "group:420042";
            // The pre-existing buffered seq must actually exist in persisted unread state.
            let mut buffered = hub.state().unwrap();
            buffered["conversations"][key]["lastUnreadSeq"] = json!(2);
            buffered["conversations"][key]["unread"] = json!([{"seq":1},{"seq":2}]);
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",buffered.to_string().as_bytes()).unwrap();
            hub.ready.clear();
            let mut state = hub.state().unwrap();
            state["conversations"][key]["lastUnreadSeq"] = json!(2);
            state["conversations"][key]["unread"] = json!([{"seq":1},{"seq":2}]);
            state["conversations"][key]["wakeConfig"]["triggers"]["nameMention"] = json!(true);
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            std::fs::write(root.join("dependencies.json"), json!({
                "dsh":format!("http://127.0.0.1:{port}"),"model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"
            }).to_string()).unwrap();
            // Reopen the context so dependency selection uses this fixture's listener.
            hub.context = RuntimeContext::from_root(&root).unwrap();
            std::fs::write(root.join("user/dsh/.credentials.yaml"), "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n").unwrap();
            crate::session_map::bind(&hub.context, key, "cap-session", None).unwrap();
            let (stop_server, stopped) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut methods = Vec::new();
                while Instant::now() < deadline && stopped.try_recv().is_err() {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5)); continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut raw = Vec::new();
                    let mut chunk = [0;4096];
                    let rpc_id = loop {
                        let count = stream.read(&mut chunk).unwrap();
                        assert!(count > 0);
                        raw.extend_from_slice(&chunk[..count]);
                        let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue; };
                        let head = String::from_utf8_lossy(&raw[..split]);
                        let length = head.lines().find_map(|line| {
                            let (name,value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                        }).unwrap_or(0);
                        if raw.len() < split + 4 + length { continue; }
                        let request: Value = serde_json::from_slice(&raw[split+4..split+4+length]).unwrap();
                        methods.push(request["method"].as_str().unwrap().to_string());
                        break request["rpcId"].clone();
                    };
                    let body = json!({"type":"server-response","rpcId":rpc_id,"result":{"ok":true,"value":null}}).to_string();
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
                methods
            });
            let now = crate::now_ms() as i64;
            let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
            let driver = hub.drivers.get_mut(key).unwrap();
            driver.sched.turn_started_at = Some(now - cap - 14);
            driver.sched.prompt_running = true;
            driver.apply(session::Input::Message {reason:"nameMention".into(),seq:2,at_ms:now-1000});
            // The first buffered trigger is before the cap; the next arrives at cap + 14 ms.
            let state = hub.state().unwrap();
            let (finish_turn, finished) = tokio::sync::oneshot::channel();
            hub.active.insert(key.into(), Active {
                task:tokio::spawn(async move { let _ = finished.await; DispatchResult::new(Err("回合已取消".into()), false) }),inbound:vec![],reason:"atMention".into(),
                previous:state["conversations"][key].clone(),at:now-cap-14,token:"test-token".into(),
                mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
            });
            let result = if ingress {
                let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
                    "message_id":29001,"message":[{"type":"text","data":{"text":"bot next"}}]});
                let ev = serde_json::from_value(frame.clone()).unwrap();
                hub.handle(&ev,&frame.to_string(),false,9001,"bot",&HashMap::new()).await
            } else { hub.tick("bot",9001).await };
            let _ = stop_server.send(());
            let methods = server.join().unwrap();
            result.unwrap();
            let timeouts = hub.ready.iter().filter(|(_,r)| r == "turnTimeout").count();
            let pending = hub.drivers[key].sched.pending.len();
            let token = social_state::token(&hub.context,key).unwrap().unwrap();
            let adopted_token = hub.tokens[key].clone();
            // The next tick must preserve the driver and awaited ingress after our own rotation.
            hub.tick("bot",9001).await.unwrap();
            let retained = hub.drivers.contains_key(key) && hub.awaiting.contains_key(key);
            finish_turn.send(()).unwrap();
            while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
            hub.tick("bot",9001).await.unwrap();
            let replayed = hub.active.get(key).is_some_and(|a| a.reason == "nameMention")
                || hub.drivers.get_mut(key).unwrap().pump(crate::now_ms() as i64 + 5000)
                    .iter().any(|e| e.reason == "nameMention");
            for active in hub.active.values() { active.task.abort(); }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(methods.iter().filter(|m| *m == "session/cancel").count(),1,"route ingress={ingress}: {methods:?}");
            assert_eq!(timeouts,0,"control event must never enter the wake queue");
            assert_ne!(token,"test-token");
            assert_eq!(adopted_token,token,"internal rotation must be adopted before settings invalidation");
            assert!(pending > 0 && retained,"valid buffered scheduling intent was discarded");
            assert!(replayed,"buffered unread work must replay when the cancelled turn finishes");
        }
    }

    /// A loopback DSH for cap-cancel tests: records each method; `refuse_cancel` makes
    /// `session/cancel` fail. The hub's context is reopened on it and `key` bound to a session.
    fn fake_dsh(root: &std::path::Path, hub: &mut Hub, key: &str, refuse_cancel: bool)
        -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(root.join("dependencies.json"), json!({
            "dsh":format!("http://127.0.0.1:{port}"),"model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"
        }).to_string()).unwrap();
        hub.context = RuntimeContext::from_root(root).unwrap();
        std::fs::write(root.join("user/dsh/.credentials.yaml"), "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n").unwrap();
        crate::session_map::bind(&hub.context, key, "cap-session", None).unwrap();
        let (stop_server, stopped) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut methods = Vec::new();
            while Instant::now() < deadline && matches!(stopped.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5)); continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut raw = Vec::new();
                let mut chunk = [0;4096];
                let (method, rpc_id) = loop {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    raw.extend_from_slice(&chunk[..count]);
                    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue; };
                    let head = String::from_utf8_lossy(&raw[..split]);
                    let length = head.lines().find_map(|line| {
                        let (name,value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    if raw.len() < split + 4 + length { continue; }
                    let request: Value = serde_json::from_slice(&raw[split+4..split+4+length]).unwrap();
                    break (request["method"].as_str().unwrap().to_string(), request["rpcId"].clone());
                };
                let result = if refuse_cancel && method == "session/cancel" {
                    json!({"ok":false,"error":{"code":"internal","message":"fixture refuses cancel"}})
                } else { json!({"ok":true,"value":null}) };
                methods.push(method);
                let body = json!({"type":"server-response","rpcId":rpc_id,"result":result}).to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            methods
        });
        (stop_server, server)
    }

    #[tokio::test]
    async fn ticket159_reserved2_dsh_cap_refuses_late_admission() {
        let fixture = queued_hub("dsh-cap-before-admission").await;
        let _root = CleanupRoot(fixture.0.clone());
        let (root, mut hub) = fixture;
        let key = "group:420042";
        let cfg = hub.context.load_config().unwrap();
        let now = crate::now_ms() as i64;
        let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        let token = social_state::token(&hub.context, key).unwrap().unwrap();
        let previous = hub.state().unwrap()["conversations"][key].clone();
        let (stop_server, server) = fake_dsh(&root, &mut hub, key, false);
        hub.active.insert(key.into(), Active {
            task: tokio::spawn(std::future::pending::<DispatchResult>()),
            inbound: vec![], reason: "atMention".into(), previous,
            at: now - cap, token: token.clone(), mode_stamp: hub.mode_stamp.clone(),
            session: Default::default(), minted: Default::default(), cap_abort_at: None,
            route: Default::default(),
            admitted: Default::default(),
        });
        hub.route_driver_event(&cfg, key, session::WakeEvent { reason: "turnTimeout".into(), at_ms: now });
        let admission = crate::engine_switch::prepare_turn(&hub.context, key, "reserved2", Some(&token), None);
        let active = hub.active.remove(key).unwrap();
        active.task.abort();
        let _ = active.task.await;
        let _ = stop_server.send(());
        let methods = server.join();
        assert_eq!(admission.err().as_deref(), Some(crate::turn_engine::ADMISSION_CHANGED),
            "the DSH cap must refuse late admission with the original token");
        assert!(methods.unwrap().iter().any(|method| method == "session/cancel"));
    }

    struct CleanupRoot(std::path::PathBuf);

    impl Drop for CleanupRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const HARNESS_CAP_KEY: &str = "group:420042";

    async fn harness_cap_fixture(
        label: &str,
        model: Option<std::net::SocketAddr>,
    ) -> (CleanupRoot, Hub, tokio::net::TcpListener) {
        let (root, mut hub) = queued_hub(label).await;
        let cleanup = CleanupRoot(root.clone());
        let console = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut cfg = hub.context.load_config().unwrap();
        cfg["engineDefault"] = json!("harness");
        cfg["harness"]["model"] = json!("fake-chat");
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        std::fs::write(root.join("config/harness-api-key"), "synthetic-cap-test-key").unwrap();
        if let Some(model) = model {
            std::fs::write(
                root.join("dependencies.json"),
                json!({"model":format!("http://{model}"),"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}).to_string(),
            ).unwrap();
            hub.context = RuntimeContext::from_root(&root).unwrap();
        }
        hub.context = hub
            .context
            .clone()
            .with_console_port(console.local_addr().unwrap().port());
        hub.ready.clear();
        // The captured base deadline has passed. Engagement keeps the effective deadline open
        // until the test withdraws the extension while the wake or cap check is in flight.
        set_harness_cap_engagement(&hub.context, json!(crate::turn_ledger::now_ms() + 60_000));
        (cleanup, hub, console)
    }

    fn set_harness_cap_engagement(context: &RuntimeContext, engaged_at: Value) {
        conversation_state::update_raw(context, |state| {
            state["conversations"][HARNESS_CAP_KEY]["engagedAt"] = engaged_at;
            Ok(())
        }).unwrap();
    }

    struct CancelHarnessCapFixture(tokio::task::AbortHandle);

    impl Drop for CancelHarnessCapFixture {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn start_harness_cap_wake(
        hub: &mut Hub,
        configure: impl FnOnce(&mut pipeline::Pipeline),
    ) -> CancelHarnessCapFixture {
        let key = HARNESS_CAP_KEY;
        let cfg = hub.context.load_config().unwrap();
        let token = social_state::token(&hub.context, key).unwrap().unwrap();
        let previous = hub.state().unwrap()["conversations"][key].clone();
        let inbound = hub.awaiting.remove(key).unwrap();
        let now = crate::turn_ledger::now_ms();
        hub.journal.mark_processing(inbound.clone(), now).await.unwrap();
        let route = pipeline::SharedReserved2Route::default();
        *route.lock().unwrap() = pipeline::Reserved2Route::PendingHarness;
        let (minted, admitted) = (pipeline::MintedEpoch::default(), pipeline::MintedEpoch::default());
        let mut pipe = pipeline::Pipeline::new_without_dsh(&hub.context, &cfg, true).unwrap();
        pipe.share_reserved2_route(route.clone(), admitted.clone());
        pipe.share_minted_epoch(minted.clone());
        pipe.expect_dispatch_token(token.clone());
        pipe.expect_dispatch_deadline(now - 1);
        // Admit before the base cap, then exercise the render/runner race on the real clock.
        pipe.admission_clock = Some(Box::new(move || now - 2));
        pipe.note_turn("atMention", &inbound);
        configure(&mut pipe);
        let task = tokio::spawn(async move {
            let result = pipe.run_turn(&crate::onebot::Target::Group(420042), "cap fixture wake", "", &cfg).await;
            DispatchResult::new(result, pipe.dispatch_definitely_unsubmitted())
        });
        let abort = task.abort_handle();
        hub.active.insert(key.into(), Active {
            task, inbound, reason: "atMention".into(), previous, at: now, token,
            mode_stamp: hub.mode_stamp.clone(), session: Default::default(), minted,
            cap_abort_at: None, route, admitted,
        });
        CancelHarnessCapFixture(abort)
    }

    async fn harness_cap_request(
        listener: &tokio::net::TcpListener,
        path: &str,
    ) -> (tokio::net::TcpStream, Value) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert!(line.starts_with(&format!("POST {path} ")), "{line}");
        let mut length = None;
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0, "truncated headers");
            if line == "\r\n" { break; }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse().unwrap());
                }
            }
        }
        let mut body = vec![0; length.unwrap()];
        stream.read_exact(&mut body).await.unwrap();
        (stream.into_inner(), serde_json::from_slice(&body).unwrap())
    }

    async fn harness_cap_response(stream: tokio::net::TcpStream, content_type: &str, body: &str) {
        harness_cap_http_response(stream, "200 OK", content_type, body).await;
    }

    async fn harness_cap_http_response(
        mut stream: tokio::net::TcpStream,
        status: &str,
        content_type: &str,
        body: &str,
    ) {
        use tokio::io::AsyncWriteExt;

        let header = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
    }

    async fn remove_fixture_wake(hub: &mut Hub) -> Option<String> {
        let active = hub.active.remove(HARNESS_CAP_KEY)?;
        active.task.abort();
        let _ = active.task.await;
        Some(active.reason)
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_render_failure_after_cap_does_not_retry() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-render-failure", None).await;
            let _task = start_harness_cap_wake(&mut hub, |_| {}).await;
            let (snapshot, _) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            set_harness_cap_engagement(&hub.context, Value::Null);
            harness_cap_http_response(snapshot, "503 Service Unavailable", "application/json", "{}").await;
            wait_for_harness_cap_wake(&hub).await;
            hub.tick("bot", 9001).await.unwrap();

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let turn = records.iter().find(|r| r["k"] == "begun" && r["key"] == HARNESS_CAP_KEY).unwrap();
            let end = records.iter().find(|r| r["k"] == "ended" && r["turn"] == turn["turn"]).unwrap();
            assert_eq!(end["reason"], "turnTimeout", "an expired render error must settle as turnTimeout");
            assert_eq!(end["outcome"], "cancelled");
            assert!(!hub.retries.contains_key(HARNESS_CAP_KEY));
        }).await.expect("the render error fixture must settle");
    }

    fn begin_receipt_turn(context: &RuntimeContext, token: &str, deadline: i64) -> String {
        let key = HARNESS_CAP_KEY;
        let turn = crate::turn_ledger::Recorder::begin_reserved2(
            context, key, "harness", "atMention", &crate::send_outbox::token_epoch(token), &[],
        ).unwrap();
        crate::turn_ledger::append_record(context, &json!({"k":"budget","turn":turn.id,
            "t":crate::turn_ledger::now_ms(),"deadline":deadline,"capMs":1000,"ceiling":deadline + 120_000})).unwrap();
        turn.id
    }

    async fn snapshot_grant_fixture(label: &str) -> (CleanupRoot, Hub, tokio::net::TcpListener, String, String) {
        let (root, hub, console) = harness_cap_fixture(label, None).await;
        conversation_state::update_raw(&hub.context, |state| {
            let conv = &mut state["conversations"][HARNESS_CAP_KEY];
            conv["unread"] = json!([{"seq":1},{"seq":2}]);
            conv["lastUnreadSeq"] = json!(2);
            conv["lastReadThroughSeq"] = json!(0);
            conv["wakeConfig"]["mode"] = json!("active");
            conv["wakeConfig"]["infinite"] = json!(false);
            Ok(())
        }).unwrap();
        let token = social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().unwrap();
        let turn = begin_receipt_turn(&hub.context, &token, crate::turn_ledger::now_ms() - 1);
        (root, hub, console, token, turn)
    }

    #[tokio::test]
    async fn ticket159_lost_snapshot_response_retry_replaces_grant() {
        let (_root, hub, console, token, turn) = snapshot_grant_fixture("lost-snapshot").await;
        let key = HARNESS_CAP_KEY;
        let (client, credential, turn_id) = (hub.context.clone(), token.clone(), turn.clone());
        let request = tokio::task::spawn_blocking(move || {
            crate::qq_tools::wake_snapshot(&client, &client.load_config().unwrap(), key, &credential, &turn_id, true)
        });
        let (socket, body) = tokio::time::timeout(std::time::Duration::from_secs(5),
            harness_cap_request(&console, "/api/socialV2/wake-snapshot")).await.unwrap();
        let (status, lost) = console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot", Some(&body));
        assert_eq!(status, 200);
        assert_eq!(lost["readThroughSeq"], 2);
        drop(socket);
        assert!(request.await.unwrap().is_err(), "the renderer must lose the first response");
        // Retry under the same live turn and token.
        let mut cfg = hub.context.load_config().unwrap();
        cfg["socialV2"]["context"]["wakeMessageLimit"] = json!(1);
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        let (status, retry) = console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot", Some(&body));
        assert_eq!(status, 200);
        assert_eq!(retry["includedUnreadCount"], 1);
        let mark = |seq| console::agent_request(&hub.context, "POST", "/api/socialV2/mark-read",
            Some(&json!({"key":key,"throughSeq":seq})), &token).0;
        assert_eq!(mark(2), 400, "the lost response must not authorize the unseen suffix");
        assert_eq!(retry["readThroughSeq"], 1);
        assert_eq!(mark(1), 200, "the delivered retry grants its own prefix");
    }

    #[tokio::test]
    async fn mark_read_requires_explicit_offered_cursor() {
        let (_root, hub, _console, token, turn) = snapshot_grant_fixture("mark-read-explicit").await;
        let key = HARNESS_CAP_KEY;
        let context = &hub.context;
        let (status, snapshot) = console::admin_request(context, "POST", "/api/socialV2/wake-snapshot",
            Some(&json!({"key":key,"token":token,"turnId":turn})));
        assert_eq!(status, 200);
        assert_eq!(snapshot["readThroughSeq"], 2);

        let route = "/api/socialV2/mark-read";
        let mark = |body| console::agent_request(context, "POST", route, Some(&body), &token);
        let (status, reply) = mark(json!({"key":key}));
        assert_eq!(status, 400, "agent mark-read requires an explicit cursor even with receipts: {reply}");
        assert_eq!(mark(json!({"key":key,"clearAll":true})).0, 400);

        conversation_state::update_raw(context, |state| {
            let conv = &mut state["conversations"][key];
            conv["unread"].as_array_mut().unwrap().push(json!({"seq":3}));
            conv["lastUnreadSeq"] = json!(3);
            Ok(())
        }).unwrap();
        let (status, reply) = console::agent_request(context, "POST", "/api/socialV2/wake-config",
            Some(&json!({"key":key,"config":{"mode":"active"}})), &token);
        assert_eq!(status, 200);
        assert_eq!(reply["markedCount"], 0);
        assert_eq!(mark(json!({"key":key,"throughSeq":3})).0, 400);
        let (status, reply) = mark(json!({"key":key,"throughSeq":snapshot["readThroughSeq"]}));
        assert_eq!(status, 200);
        assert_eq!(reply["unreadCount"], 1);

        assert_eq!(console::admin_request(context, "POST", route, Some(&json!({"key":key}))).0, 400);
        let (status, reply) = console::admin_request(context, "POST", route, Some(&json!({"key":key,"clearAll":true})));
        assert_eq!(status, 200);
        assert_eq!(reply["unreadCount"], 0);
    }

    #[tokio::test]
    async fn ticket159_snapshot_expiry_survives_revocation_lock_timeout() {
        let (_root, hub, _console, token, turn) = snapshot_grant_fixture("locked-snapshot").await;
        let key = HARNESS_CAP_KEY;
        let (status, lost) = console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot",
            Some(&json!({"key":key,"token":token,"turnId":turn})));
        assert_eq!(status, 200);
        assert_eq!(lost["readThroughSeq"], 2);
        let (status, read) = console::agent_request(&hub.context, "GET",
            &format!("/api/socialV2/unread?key={key}&afterSeq=0&limit=1"), None, &token);
        assert_eq!(status, 200);
        assert_eq!(read["messages"].as_array().unwrap().len(), 1);
        assert_eq!(read["messages"][0]["seq"], 1);
        set_harness_cap_engagement(&hub.context, Value::Null);
        let lock = social_state::acquire_gate(&hub.context).unwrap();
        let error = pipeline::revoke_reserved2_epoch(&hub.context, key, &token, None).unwrap_err();
        drop(lock);
        assert!(error.contains("busy"), "{error}");
        assert_eq!(social_state::token(&hub.context, key).unwrap().as_deref(), Some(token.as_str()));
        for route in ["/api/socialV2/mark-read", "/api/socialV2/wake-config"] {
            let (status, _) = console::agent_request(&hub.context, "POST", route,
                Some(&json!({"key":key,"throughSeq":2})), &token);
            assert_eq!(status, 400, "an expired grant cannot authorize unseen reads on {route}");
        }
        let (status, _) = console::agent_request(&hub.context, "POST", "/api/socialV2/mark-read",
            Some(&json!({"key":key})), &token);
        assert_eq!(status, 400, "omitting the cursor must not authorize an unseen read either");
        assert_eq!(hub.state().unwrap()["conversations"][key]["unread"].as_array().unwrap().len(), 2);
        let (status, _) = console::agent_request(&hub.context, "POST", "/api/socialV2/mark-read",
            Some(&json!({"key":key,"throughSeq":1})), &token);
        assert_eq!(status, 200, "the independent tool read retains its own receipt");
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_expired_snapshot_cannot_authorize_unseen_reads() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-unseen-receipts", None).await;
            let key = HARNESS_CAP_KEY;
            conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["unread"] = json!((1..=12).map(|seq| json!({"seq":seq,"text":"fixture"})).collect::<Vec<_>>());
                conv["lastUnreadSeq"] = json!(12);
                conv["lastReadThroughSeq"] = json!(0);
                Ok(())
            }).unwrap();
            let _task = start_harness_cap_wake(&mut hub, |_| {}).await;
            let (snapshot, input) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            let (status, packet) = console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot", Some(&input));
            assert_eq!(status, 200);
            assert_eq!(packet["readThroughSeq"], 12);
            set_harness_cap_engagement(&hub.context, Value::Null);
            harness_cap_response(snapshot, "application/json", &packet.to_string()).await;
            wait_for_harness_cap_wake(&hub).await;
            hub.tick("bot", 9001).await.unwrap();

            let mut cfg = hub.context.load_config().unwrap();
            cfg["socialV2"]["context"]["wakeMessageLimit"] = json!(1);
            hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
            let token = social_state::token(&hub.context, key).unwrap().unwrap();
            let turn = begin_receipt_turn(&hub.context, &token, crate::turn_ledger::now_ms() + 60_000);
            let (status, next) = console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot",
                Some(&json!({"key":key,"token":token,"turnId":turn})));
            assert_eq!(status, 200);
            assert_eq!(next["includedUnreadCount"], 1);
            assert_eq!(next["readThroughSeq"], 1, "an expired snapshot must not authorize reading unseen messages");
            let (status, _) = console::agent_request(&hub.context, "POST", "/api/socialV2/mark-read",
                Some(&json!({"key":key,"throughSeq":12})), &token);
            assert_eq!(status, 400);
        }).await.expect("the snapshot receipt fixture must settle");
    }

    #[tokio::test]
    async fn ticket159_reserved2_expired_finite_sleep_wake_fires_once() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, _console) = harness_cap_fixture("harness-cap-finite-sleep", None).await;
            let key = HARNESS_CAP_KEY;
            let due = crate::parity_clock::now_ms() - 1_000;
            conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["engagedAt"] = json!(0);
                conv["wakeConfig"]["confirmedAt"] = json!(2);
                conv["wakeConfig"]["infinite"] = json!(false);
                conv["wakeConfig"]["sleepUntil"] = json!(crate::sticker::iso_at(due));
                Ok(())
            }).unwrap();
            let cfg = hub.context.load_config().unwrap();
            let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(hub.active[key].reason, "timeout");
            let _task = CancelHarnessCapFixture(hub.active[key].task.abort_handle());
            let cap = hub.drivers[key].sched.turn_cap_at().unwrap();
            hub.pump_timers(&cfg, cap);
            assert!(hub.active[key].route() == pipeline::Reserved2Route::ExpiredBeforeAdmission);
            drop(gate);
            wait_for_harness_cap_wake(&hub).await;

            hub.tick("bot", 9001).await.unwrap();
            hub.tick("bot", 9001).await.unwrap();
            assert!(remove_fixture_wake(&mut hub).await.is_none(), "an expired finite sleep must not wake twice");
            assert_eq!(hub.state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"], Value::Null);
        }).await.expect("the finite sleep fixture must settle");
    }

    #[tokio::test]
    async fn ticket297_expired_provisional_wake_rearms_stranded_finite_sleep() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, _console) = harness_cap_fixture("harness-cap-stranded-finite-sleep", None).await;
            let key = HARNESS_CAP_KEY;
            conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["engagedAt"] = json!(0);
                conv["wakeConfig"]["confirmedAt"] = json!(2);
                conv["wakeConfig"]["infinite"] = json!(false);
                conv["wakeConfig"]["sleepUntil"] = Value::Null;
                Ok(())
            }).unwrap();
            let cfg = hub.context.load_config().unwrap();
            let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
            hub.ready.push((key.into(), "atMention".into()));
            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(hub.active[key].reason, "atMention");
            assert!(hub.active[key].previous["wakeConfig"]["sleepUntil"].is_null());
            let _task = CancelHarnessCapFixture(hub.active[key].task.abort_handle());
            let cap = hub.drivers[key].sched.turn_cap_at().unwrap();
            hub.pump_timers(&cfg, cap);
            assert!(hub.active[key].route() == pipeline::Reserved2Route::ExpiredBeforeAdmission);
            drop(gate);
            wait_for_harness_cap_wake(&hub).await;

            hub.tick("bot", 9001).await.unwrap();
            assert!(
                hub.state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"]
                    .as_str()
                    .and_then(crate::wake::parse_iso_ms)
                    .is_some(),
                "dropping an unadmitted provisional wake must re-arm stranded finite sleepUntil"
            );
            assert!(
                hub.drivers[key].sched.wc.sleep_until.is_some(),
                "live driver must adopt re-armed finite sleep_until in the same process"
            );
        }).await.expect("the stranded finite sleep fixture must settle");
    }

    #[tokio::test]
    async fn ticket297_expired_provisional_wake_preserves_interrupted_future_deadline() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, _console) = harness_cap_fixture("harness-cap-future-deadline", None).await;
            let key = HARNESS_CAP_KEY;
            let future_ms = crate::turn_ledger::now_ms() + 500_000;
            let future_iso = crate::sticker::iso_at(future_ms);
            let rec = crate::turn_ledger::Recorder::begin_reserved2(
                &hub.context,
                key,
                "harness",
                "timeout",
                "tok-epoch",
                &[],
            )
            .unwrap();
            rec.wake_claim(&json!({
                "at": 100,
                "previous": {
                    "wakeConfig": {
                        "confirmedAt": 2,
                        "sleepUntil": future_iso
                    }
                }
            }))
            .unwrap();
            rec.end_reconciled("failed", "harness-interrupted").unwrap();

            conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["engagedAt"] = json!(0);
                conv["wakeConfig"]["confirmedAt"] = json!(2);
                conv["wakeConfig"]["infinite"] = json!(false);
                conv["wakeConfig"]["sleepUntil"] = Value::Null;
                Ok(())
            })
            .unwrap();

            let cfg = hub.context.load_config().unwrap();
            let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
            hub.ready.push((key.into(), "atMention".into()));
            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(hub.active[key].reason, "atMention");
            assert!(hub.active[key].previous["wakeConfig"]["sleepUntil"].is_null());
            let _task = CancelHarnessCapFixture(hub.active[key].task.abort_handle());
            let cap = hub.drivers[key].sched.turn_cap_at().unwrap();
            hub.pump_timers(&cfg, cap);
            assert!(hub.active[key].route() == pipeline::Reserved2Route::ExpiredBeforeAdmission);
            drop(gate);
            wait_for_harness_cap_wake(&hub).await;

            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(
                hub.state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"],
                json!(future_iso),
                "dropping an unadmitted provisional wake must preserve the interrupted turn's still-future deadline"
            );
            assert_eq!(
                hub.drivers[key].sched.wc.sleep_until.as_deref(),
                Some(future_iso.as_str()),
                "live driver must adopt the preserved future deadline"
            );
        }).await.expect("the preserved future deadline fixture must settle");
    }

    #[tokio::test]
    async fn ticket297_transient_rearm_failure_retries_on_next_tick() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (root, mut hub, _console) = harness_cap_fixture("harness-cap-transient-rearm", None).await;
            let key = HARNESS_CAP_KEY;
            conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["engagedAt"] = json!(0);
                conv["wakeConfig"]["confirmedAt"] = json!(2);
                conv["wakeConfig"]["infinite"] = json!(false);
                conv["wakeConfig"]["sleepUntil"] = Value::Null;
                Ok(())
            })
            .unwrap();

            let cfg = hub.context.load_config().unwrap();
            let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
            hub.ready.push((key.into(), "atMention".into()));
            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(hub.active[key].reason, "atMention");
            let _task = CancelHarnessCapFixture(hub.active[key].task.abort_handle());
            let cap = hub.drivers[key].sched.turn_cap_at().unwrap();
            hub.pump_timers(&cfg, cap);
            assert!(hub.active[key].route() == pipeline::Reserved2Route::ExpiredBeforeAdmission);
            drop(gate);
            wait_for_harness_cap_wake(&hub).await;

            let ledger_path = root.0.join("data").join(crate::turn_ledger::ISOLATED_LEDGER_FILE);
            let saved_ledger = std::fs::read(&ledger_path).ok();
            std::fs::write(&ledger_path, b"{corrupt-json-line\n").unwrap();

            hub.tick("bot", 9001).await.unwrap();
            assert!(
                hub.state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"].is_null(),
                "sleepUntil remains null while the rearm store read fails transiently"
            );

            match saved_ledger {
                Some(bytes) => std::fs::write(&ledger_path, bytes).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&ledger_path);
                }
            }

            hub.tick("bot", 9001).await.unwrap();
            assert!(
                hub.state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"]
                    .as_str()
                    .and_then(crate::wake::parse_iso_ms)
                    .is_some(),
                "next tick must retry and re-arm finite sleepUntil after transient failure clears"
            );
            assert!(
                hub.drivers[key].sched.wc.sleep_until.is_some(),
                "live driver must adopt re-armed sleep_until after retry succeeds"
            );
        }).await.expect("the transient rearm retry fixture must settle");
    }

    async fn wait_for_harness_cap_wake(hub: &Hub) {
        while !hub.active[HARNESS_CAP_KEY].task.is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_cap_during_render_does_not_retry() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-render", None).await;
            let _task = start_harness_cap_wake(&mut hub, |_| {}).await;
            let (snapshot, _) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            set_harness_cap_engagement(&hub.context, Value::Null);
            harness_cap_response(snapshot, "application/json", "null").await;
            wait_for_harness_cap_wake(&hub).await;
            hub.tick("bot", 9001).await.unwrap();

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let begun = records.iter().find(|r| r["k"] == "begun" && r["key"] == HARNESS_CAP_KEY).unwrap();
            let id = &begun["turn"];
            let end = records.iter().find(|r| r["k"] == "ended" && &r["turn"] == id).unwrap();
            assert_eq!(end["outcome"], "cancelled");
            assert_eq!(end["reason"], "turnTimeout");
            assert!(!records.iter().any(|r| r["k"] == "attempt" && &r["turn"] == id));

            if let Some((at, _)) = hub.retries.get_mut(HARNESS_CAP_KEY) { *at = 0; }
            hub.tick("bot", 9001).await.unwrap();
            let redispatched = hub.active.remove(HARNESS_CAP_KEY);
            let retried = redispatched.is_some();
            if let Some(active) = redispatched {
                active.task.abort();
                let _ = active.task.await;
            }
            assert!(!retried, "a wake capped during render must never dispatch a second time");
        }).await.expect("the render cap fixture must settle");
    }

    async fn finish_harness_before_cap_poll(
        hub: &mut Hub,
        console: &tokio::net::TcpListener,
        model: &tokio::net::TcpListener,
    ) -> String {
        let (before_check, cap_poll) = tokio::sync::oneshot::channel();
        let _task = start_harness_cap_wake(hub, |pipe| {
            pipe.harness_cap_poll_hook = Some(before_check);
        }).await;
        let (snapshot, _) = harness_cap_request(console, "/api/socialV2/wake-snapshot").await;
        harness_cap_response(snapshot, "application/json", "null").await;
        let (request, _) = harness_cap_request(model, "/v1/chat/completions").await;
        let release_check = cap_poll.await.unwrap();

        let ledger = crate::turn_ledger::read_records(&hub.context).unwrap();
        let begun = ledger.iter().find(|r| r["k"] == "begun" && r["key"] == HARNESS_CAP_KEY).unwrap();
        let turn_id = begun["turn"].as_str().unwrap();
        let chunk = json!({"id":"cap-fixture","object":"chat.completion.chunk","model":"fake-chat",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"finished before cap"},"finish_reason":null}]});
        let end = json!({"id":"cap-fixture","object":"chat.completion.chunk","model":"fake-chat",
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
        harness_cap_response(request, "text/event-stream", &body).await;

        // The 100 ms branch already won select, but the runner can commit while the check is paused.
        let harness = crate::harness::Harness::open(&hub.context).unwrap();
        loop {
            match harness.status(turn_id).unwrap().unwrap() {
                crate::harness::Status::Running => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                crate::harness::Status::Completed(text) => {
                    assert_eq!(text, "finished before cap");
                    break;
                }
                status => panic!("the harness did not complete before the cap check: {status:?}"),
            }
        }
        set_harness_cap_engagement(&hub.context, Value::Null);
        release_check.send(()).unwrap();
        wait_for_harness_cap_wake(hub).await;
        let current = social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().unwrap();
        assert_ne!(current, hub.active[HARNESS_CAP_KEY].admitted_token(), "the cap check must rotate the token");
        turn_id.to_owned()
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_completion_wins_the_cap_race() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let model = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-completion", Some(model.local_addr().unwrap())).await;
            let turn_id = finish_harness_before_cap_poll(&mut hub, &console, &model).await;

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let end = records.iter().find(|r| r["k"] == "ended" && r["turn"] == turn_id);
            assert_eq!(end.and_then(|r| r["outcome"].as_str()), Some("completed"),
                "the cap check must preserve the runner's completed turn");
            assert_eq!(end.unwrap()["reason"], "ok");

            // A queued urgent follow-up is already covered according to the companion console.
            let urgent = format!("{HARNESS_CAP_KEY}|message:2");
            hub.journal.record_received(urgent.clone(), HARNESS_CAP_KEY.into(), "{}".into(), 2).await.unwrap();
            hub.awaiting.insert(HARNESS_CAP_KEY.into(), vec![urgent.clone()]);
            hub.queued_messages.insert(HARNESS_CAP_KEY.into(), vec![(2, urgent.clone())]);
            hub.ready.push((HARNESS_CAP_KEY.into(), "atMention".into()));
            let current = social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().unwrap();
            let receipt = async {
                let (request, evidence) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
                assert_eq!(evidence, json!({"key":HARNESS_CAP_KEY,"token":current,"turnId":turn_id,"readOnly":true}));
                harness_cap_response(request, "application/json", r#"{"urgentDeliveredSeqs":[2]}"#).await;
            };
            let (tick, ()) = tokio::join!(hub.tick("bot", 9001), receipt);
            tick.unwrap();
            let stage = hub.journal.run(move |j| j.stage_of(&urgent)).await.unwrap();
            assert_eq!(stage.as_deref(), Some("handled"));
            assert_ne!(remove_fixture_wake(&mut hub).await.as_deref(), Some("atMention"),
                "covered ingress must not wake another turn");
        }).await.expect("the completion cap fixture must settle");
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_completion_after_cap_rotation_checks_closure() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let model = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-closure", Some(model.local_addr().unwrap())).await;
            finish_harness_before_cap_poll(&mut hub, &console, &model).await;
            hub.tick("bot", 9001).await.unwrap();
            let reminder = remove_fixture_wake(&mut hub).await;
            let state = hub.state().unwrap();
            assert_eq!(state["conversations"][HARNESS_CAP_KEY]["wakeConfig"]["noActionCount"], 1,
                "completed turns must count no action after an owned cap rotation");
            assert_eq!(reminder.as_deref(), Some("wakeReminder"));
        }).await.expect("the closing reminder fixture must settle");
    }

    #[tokio::test]
    async fn exhausted_wait_closes_after_final_response() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let model = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (_root, mut hub, console) = harness_cap_fixture("wait-limit-closeout", Some(model.local_addr().unwrap())).await;
            let key = HARNESS_CAP_KEY;
            let _task = start_harness_cap_wake(&mut hub, |_| {}).await;
            let (snapshot, _) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            harness_cap_response(snapshot, "application/json", "null").await;
            let (request, _) = harness_cap_request(&model, "/v1/chat/completions").await;
            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let begun = records.iter().find(|r| r["k"] == "begun" && r["key"] == key).unwrap();
            let turn = begun["turn"].as_str().unwrap();
            let token = hub.active[key].admitted_token();
            let epoch = crate::send_outbox::token_epoch(&token);
            let scope = format!("{epoch}:{turn}");
            let cfg = hub.context.load_config().unwrap();
            let max = social_state::wait_policy(&cfg).max_per_turn;
            let refunds = social_state::turn_send_cap(&cfg);
            let wake = conversation_state::update_raw(&hub.context, |state| {
                let conv = &mut state["conversations"][key];
                conv["waitBudget"] = json!({"scope":scope,"used":max-1,"refunds":refunds});
                conv["lastUnreadSeq"] = json!(1);
                conv["wakeConfig"]["triggers"]["atMention"] = json!(true);
                conv["wakeConfig"]["infinite"] = json!(true);
                conv["wakeConfig"]["sleepUntil"] = Value::Null;
                Ok(conv["wakeConfig"].clone())
            }).unwrap();
            let seq = social_state::record_reserved2_incoming(&hub.context, &cfg, key, "final-wait-question",
                json!({"text":"fixture question","time":crate::turn_ledger::now_ms(),"atSelf":true,"isSelf":false})).unwrap();
            let (status, wait) = console::agent_request(&hub.context, "POST", "/api/socialV2/wait",
                Some(&json!({"key":key,"purpose":"messages","timeoutMs":1000})), &token);
            assert_eq!(status, 200, "{wait}");
            assert_eq!(wait["newMessages"].as_array().unwrap().last().unwrap()["seq"], seq);
            assert_eq!(wait["waitLimitReached"], true);
            assert!(wait["nextAction"].is_null(), "the question gets a response step: {wait}");

            let chunk = json!({"id":"wait-limit","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{"role":"assistant","content":"finished after answering"},"finish_reason":null}]});
            let end = json!({"id":"wait-limit","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
            harness_cap_response(request, "text/event-stream", &body).await;
            wait_for_harness_cap_wake(&hub).await;

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let end = records.iter().find(|r| r["k"] == "ended" && r["turn"] == turn).unwrap();
            assert_eq!(end["reason"], "observationSleep", "an exhausted turn closes after its response");
            hub.tick("bot", 9001).await.unwrap();
            assert!(remove_fixture_wake(&mut hub).await.is_none(), "no closeout reminder is dispatched");
            assert_eq!(hub.state().unwrap()["conversations"][key]["wakeConfig"], wake);
        }).await.expect("the wait-limit fixture must settle");
    }

    #[tokio::test]
    async fn ticket159_reserved2_harness_observation_cap_completes_both_stores() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let model = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-observation", Some(model.local_addr().unwrap())).await;
            let (before_check, cap_poll) = tokio::sync::oneshot::channel();
            let _task = start_harness_cap_wake(&mut hub, |pipe| {
                pipe.harness_cap_poll_hook = Some(before_check);
            }).await;
            let (snapshot, _) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            harness_cap_response(snapshot, "application/json", "null").await;
            let (request, _) = harness_cap_request(&model, "/v1/chat/completions").await;
            let release_check = cap_poll.await.unwrap();
            let call = json!({"index":0,"id":"observe","type":"function","function":{
                "name":"qq_wait_for_messages","arguments":r#"{"key":"","token":"","purpose":"messages","timeoutMs":60000}"#
            }});
            let chunk = json!({"id":"observation-fixture","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":null}]});
            let end = json!({"id":"observation-fixture","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
            let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
            harness_cap_response(request, "text/event-stream", &body).await;
            let (wait, input) = harness_cap_request(&console, "/api/socialV2/wait").await;
            assert_eq!(input["purpose"], "messages");

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let begun = records.iter().find(|r| r["k"] == "begun" && r["key"] == HARNESS_CAP_KEY).unwrap();
            let turn_id = begun["turn"].as_str().unwrap();
            let token = hub.active[HARNESS_CAP_KEY].admitted_token();
            let generation = crate::send_outbox::token_epoch(&token);
            let _lease = social_state::hold_test_observation_wait(&hub.context, HARNESS_CAP_KEY, &token, turn_id);
            set_harness_cap_engagement(&hub.context, Value::Null);
            release_check.send(()).unwrap();
            while social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().as_deref() == Some(token.as_str()) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            harness_cap_http_response(wait, "403 Forbidden", "application/json", "{}").await;
            wait_for_harness_cap_wake(&hub).await;
            hub.tick("bot", 9001).await.unwrap();

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let end = records.iter().find(|r| r["k"] == "ended" && r["turn"] == turn_id).unwrap();
            assert_eq!(end["outcome"], "completed");
            assert_eq!(end["reason"], "observationSleep");
            let db = rusqlite::Connection::open(hub.context.data_dir().join("conversations.db")).unwrap();
            let state: String = db.query_row("SELECT state FROM turns WHERE id=?1", [turn_id], |row| row.get(0)).unwrap();
            assert_eq!(state, "completed", "the conversation store must agree with the observation sleep ledger end");
            let harness = crate::harness::Harness::open(&hub.context).unwrap();
            assert!(matches!(harness.status(turn_id).unwrap(), Some(crate::harness::Status::ObservationSleep)));
            let receipt = harness.wake_briefing(HARNESS_CAP_KEY, &generation).unwrap();
            assert!(receipt["installed"].is_object(), "the completed observation must be eligible as a briefing receipt");
        }).await.expect("the observation cap fixture must settle");
    }

    #[tokio::test]
    async fn hard_cap_observation_sleep_rearms_consumed_finite_deadline() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let model = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (_root, mut hub, console) = harness_cap_fixture("harness-cap-rearm-sleep", Some(model.local_addr().unwrap())).await;
            let mut cfg = hub.context.load_config().unwrap();
            cfg["socialV2"]["wake"]["recommendedSleepMinMs"] = json!(75_000);
            cfg["socialV2"]["wake"]["recommendedSleepMaxMs"] = json!(90_000);
            hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
            conversation_state::update_raw(&hub.context, |state| {
                let wake = &mut state["conversations"][HARNESS_CAP_KEY]["wakeConfig"];
                wake["infinite"] = json!(false);
                wake["sleepUntil"] = Value::Null;
                Ok(())
            }).unwrap();

            let (before_check, cap_poll) = tokio::sync::oneshot::channel();
            let _task = start_harness_cap_wake(&mut hub, |pipe| {
                pipe.harness_cap_poll_hook = Some(before_check);
            }).await;
            let (snapshot, _) = harness_cap_request(&console, "/api/socialV2/wake-snapshot").await;
            harness_cap_response(snapshot, "application/json", "null").await;
            let (request, _) = harness_cap_request(&model, "/v1/chat/completions").await;
            let release_check = cap_poll.await.unwrap();
            let call = json!({"index":0,"id":"observe","type":"function","function":{
                "name":"qq_wait_for_messages","arguments":r#"{"key":"","token":"","purpose":"messages","timeoutMs":60000}"#
            }});
            let chunk = json!({"id":"rearm-sleep-fixture","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":null}]});
            let end = json!({"id":"rearm-sleep-fixture","object":"chat.completion.chunk","model":"fake-chat",
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
            let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
            harness_cap_response(request, "text/event-stream", &body).await;
            let (wait, input) = harness_cap_request(&console, "/api/socialV2/wait").await;
            assert_eq!(input["purpose"], "messages");

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let begun = records.iter().find(|r| r["k"] == "begun" && r["key"] == HARNESS_CAP_KEY).unwrap();
            let turn_id = begun["turn"].as_str().unwrap();
            let token = hub.active[HARNESS_CAP_KEY].admitted_token();
            let _lease = social_state::hold_test_observation_wait(&hub.context, HARNESS_CAP_KEY, &token, turn_id);
            set_harness_cap_engagement(&hub.context, Value::Null);
            release_check.send(()).unwrap();
            while social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().as_deref() == Some(token.as_str()) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            harness_cap_http_response(wait, "403 Forbidden", "application/json", "{}").await;
            wait_for_harness_cap_wake(&hub).await;
            let before_tick = crate::now_ms() as i64;
            hub.tick("bot", 9001).await.unwrap();
            let after_tick = crate::now_ms() as i64;

            let records = crate::turn_ledger::read_records(&hub.context).unwrap();
            let end = records.iter().find(|r| r["k"] == "ended" && r["turn"] == turn_id).unwrap();
            assert_eq!(end["outcome"], "completed");
            assert_eq!(end["reason"], "observationSleep");
            assert_ne!(social_state::token(&hub.context, HARNESS_CAP_KEY).unwrap().as_deref(), Some(token.as_str()));

            let state = hub.state().unwrap();
            let wake = &state["conversations"][HARNESS_CAP_KEY]["wakeConfig"];
            assert_eq!(wake["infinite"], false);
            assert_eq!(wake["sleepRangeMs"], json!([75_000, 90_000]));
            let until = wake["sleepUntil"].as_str()
                .and_then(crate::wake::parse_iso_ms)
                .expect("hard-cap observationSleep must rearm consumed finite deadline even after cap token rotation");
            assert!(
                until >= before_tick + 75_000 && until <= after_tick + 90_000,
                "rearmed sleepUntil {until} outside [{}, {}]: {wake}",
                before_tick + 75_000,
                after_tick + 90_000
            );
        }).await.expect("the hard-cap observation rearm fixture must settle");
    }

    #[tokio::test]
    async fn ticket159_reserved2_dsh_cap_revokes_an_unmapped_waiting_wake() {
        let fixture = queued_hub("dsh-cap-unmapped").await;
        let _root = CleanupRoot(fixture.0.clone());
        let (_, mut hub) = fixture;
        let key = "group:420042";
        let cfg = hub.context.load_config().unwrap();
        let now = crate::now_ms() as i64;
        let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        let token = social_state::token(&hub.context, key).unwrap().unwrap();
        assert!(crate::session_map::lookup(&hub.context, key).unwrap().is_none());
        let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
        let mut pipe = pipeline::Pipeline::new_without_dsh(&hub.context, &cfg, true).unwrap();
        let route = pipeline::SharedReserved2Route::default();
        let minted = pipeline::MintedEpoch::default();
        let admitted = pipeline::MintedEpoch::default();
        pipe.share_reserved2_route(route.clone(), admitted.clone());
        pipe.share_minted_epoch(minted.clone());
        pipe.expect_dispatch_token(token.clone());
        let previous = hub.state().unwrap()["conversations"][key].clone();
        hub.active.insert(key.into(), Active {
            task: tokio::spawn(std::future::pending::<DispatchResult>()),
            inbound: vec![], reason: "atMention".into(), previous,
            at: now - cap, token: token.clone(), mode_stamp: hub.mode_stamp.clone(),
            session: Default::default(), minted, cap_abort_at: None, route, admitted,
        });
        let target = crate::onebot::Target::Group(420042);
        let wake = pipe.run_turn(&target, "first DSH wake", "unused CLI wake", &cfg);
        tokio::pin!(wake);
        tokio::select! { biased;
            result = &mut wake => {
                hub.active.remove(key).unwrap().task.abort();
                panic!("the wake passed its held admission gate: {result:?}");
            }
            _ = std::future::ready(()) => {}
        }
        hub.route_driver_event(&cfg, key, session::WakeEvent { reason: "turnTimeout".into(), at_ms: now });
        let active = hub.active.remove(key).unwrap();
        active.task.abort();
        let _ = active.task.await;
        drop(gate);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut wake).await.unwrap();

        assert_eq!(result.err().as_deref(), Some(crate::turn_engine::ADMISSION_CHANGED),
            "the cap must invalidate an unmapped wake before the gate opens");
        let current = social_state::token(&hub.context, key).unwrap().unwrap();
        assert_ne!(current, token);
        assert_eq!(hub.tokens.get(key), Some(&current));
        assert!(!crate::turn_ledger::read_records(&hub.context).unwrap().iter().any(|r| r["k"] == "begun"));
    }

    #[tokio::test]
    async fn ticket159_reserved2_keeps_the_cap_when_a_waiting_wake_gains_a_cli_pin() {
        let fixture = queued_hub("harness-cap-before-cli-admission").await;
        let _root = CleanupRoot(fixture.0.clone());
        let (root, mut hub) = fixture;
        let key = "group:420042";
        let mut cfg = hub.context.load_config().unwrap();
        cfg["engineDefault"] = json!("harness");
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        hub.ready.retain(|(ready, _)| ready == key);
        let gate = crate::engine_switch::turn_gate(&hub.context, key).await.unwrap();
        hub.tick("bot", 9001).await.unwrap();
        assert!(hub.active[key].route() == pipeline::Reserved2Route::PendingHarness);
        tokio::task::yield_now().await;

        let cap = hub.drivers[key].sched.turn_cap_at().unwrap();
        hub.pump_timers(&cfg, cap);
        assert!(hub.drivers[key].sched.turn_cap_at().is_none(), "the driver must emit and clear its cap");
        assert!(!hub.active[key].task.is_finished(), "the wake must still be waiting at the gate");

        cfg["backends"] = json!({"sessions":{key:"missing-fixture-cli"}});
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        drop(gate);

        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !hub.active[key].task.is_finished() {
                tokio::task::yield_now().await;
            }
        }).await;
        if completed.is_err() {
            let active = hub.active.remove(key).unwrap();
            active.task.abort();
            let _ = active.task.await;
            panic!("the expired wake did not settle after the gate opened");
        }
        hub.tick("bot", 9001).await.unwrap();

        let trials = std::fs::read_to_string(root.join("data/logs/opencode-trial.jsonl")).unwrap_or_default();
        assert!(
            !trials.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|row| row["key"] == key && row["backend"] == "missing-fixture-cli"),
            "the expired wake must not dispatch the CLI pin added while it waited for admission"
        );
        assert!(!crate::turn_ledger::read_records(&hub.context).unwrap().iter()
            .any(|r| r["k"] == "begun" && r["key"] == key));
        assert!(!hub.retries.contains_key(key));
    }

    #[tokio::test]
    async fn ticket159_reserved2_parks_a_pending_archive_without_a_live_mapping() {
        let fixture = queued_hub("pending-dsh-archive-outage").await;
        let _root = CleanupRoot(fixture.0.clone());
        let (_, hub) = fixture;
        let key = "group:420042";
        let cfg = hub.context.load_config().unwrap();
        crate::settings::apply_conversation(
            &hub.context, key, "conversation.engine", &json!("harness"), "administrator", "console",
        ).unwrap();
        let token = social_state::token(&hub.context, key).unwrap().unwrap();
        let stamp = crate::reserved::mode_stamp(&hub.context).unwrap();
        let mut state = hub.state().unwrap();
        state["conversations"][key]["engineHandover"]["pending"] = json!({
            "from":"dsh", "to":"harness", "epoch":crate::send_outbox::token_epoch(&token),
            "mode_stamp":stamp, "session":"saved-dsh-session", "summary":"staged transfer", "archive_started":true,
        });
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        assert!(crate::session_map::lookup(&hub.context, key).unwrap().is_none());
        crate::dsh_health::observe(&hub.context, &cfg, false, "fixture outage", true);

        assert!(wake_needs_dsh(&hub.context, &cfg, key), "the hub must park a wake with a pending DSH archive");
        let admission = crate::engine_switch::prepare_turn(&hub.context, key, "reserved2", Some(&token), None);
        assert_eq!(admission.err().as_deref(), Some("DSH is unavailable for the handover"));
        assert_eq!(hub.state().unwrap()["conversations"][key]["engineHandover"], state["conversations"][key]["engineHandover"]);
        state["conversations"][key]["engineHandover"] = Value::Null;
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        assert!(!wake_needs_dsh(&hub.context, &cfg, key), "a wake with neither mapping nor archive can use the harness during the outage");
    }

    /// Engagement window: while she is engaged the hard cap runs from her last successful send in
    /// the turn; a send from before the turn never extends it; the per-turn ceiling always holds.
    #[tokio::test]
    async fn engage_hub_cap_runs_from_her_last_send_until_the_ceiling() {
        for (label, started_ago, engaged_ago, cancels) in [
            ("engaged", 515_014, 1000, 0usize),
            ("ceiling", 1_800_014, 1000, 1),
            ("stale send", 515_014, 515_015, 1),
        ] {
            let (root, mut hub) = queued_hub("engage-cap").await;
            let key = "group:420042";
            hub.ready.clear();
            let now = crate::now_ms() as i64;
            let mut state = hub.state().unwrap();
            state["conversations"][key]["wakeConfig"]["confirmedAt"] = json!(2);
            state["conversations"][key]["engagedAt"] = json!(now - engaged_ago);
            state["conversations"][key]["lastAiReplyAt"] = json!(now - engaged_ago);
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
            let (stop_server, server) = fake_dsh(&root, &mut hub, key, false);
            assert_eq!(hub.drivers[key].sched.cfg.turn_hard_cap_ms, 515000);
            let driver = hub.drivers.get_mut(key).unwrap();
            driver.sched.turn_started_at = Some(now - started_ago);
            driver.sched.prompt_running = true;
            hub.active.insert(key.into(), Active {
                task:tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Err("cancelled".into()), false) }),
                inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
                at:now-started_ago,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
            });
            let result = hub.tick("bot",9001).await;
            let _ = stop_server.send(());
            let methods = server.join().unwrap();
            for active in hub.active.values() { active.task.abort(); }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            result.unwrap();
            assert_eq!(methods.iter().filter(|m| *m == "session/cancel").count(), cancels, "{label}: {methods:?}");
        }
    }

    /// Ingress records whether each message is for her (bridge hint only; the model decides).
    #[tokio::test]
    async fn engage_ingress_records_whether_each_message_is_for_her() {
        let (root, mut hub) = queued_hub("engage-for-you").await;
        let key = "group:420042";
        let now = crate::now_ms() as i64;
        let mut state = hub.state().unwrap();
        state["conversations"][key]["lastAiReplyAt"] = json!(now - 1000);
        state["conversations"][key]["recentMessages"] = json!([{"isSelf":true,"text":"好呀","time":now-1000,"replyToUserId":"1002"}]);
        hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
        let text = |t: &str| json!([{"type":"text","data":{"text":t}}]);
        let at = |qq: i64, t: &str| json!([{"type":"at","data":{"qq":qq}},{"type":"text","data":{"text":t}}]);
        let mut seen = Vec::new();
        for (user, id, message, expected) in [
            (1001, 29101, text("大家晚上好"), false),
            (1001, 29102, text("bot 在吗"), true),
            (1001, 29103, at(9001, " 帮我看看"), true),
            (1002, 29104, text("那就这么定了"), true),
            (1002, 29105, at(1003, " 你看这个"), false),
            // Pokes (review fix 10): one at her is for her; one at someone else is not.
            (1003, 29106, json!({"poke":9001}), true),
            (1002, 29107, json!({"poke":1004}), false),
        ] {
            let frame = match message.get("poke") {
                Some(target) => json!({"post_type":"notice","notice_type":"notify","sub_type":"poke","group_id":420042,
                    "user_id":user,"target_id":target,"sender":{"nickname":format!("u{user}"),"card":""}}),
                None => json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":user,
                    "message_id":id,"sender":{"nickname":format!("u{user}"),"card":""},"message":message}),
            };
            let ev = serde_json::from_value(frame.clone()).unwrap();
            hub.handle(&ev,&frame.to_string(),false,9001,"bot",&HashMap::new()).await.unwrap();
            let state = hub.state().unwrap();
            let stored = state["conversations"][key]["recentMessages"].as_array().unwrap().last().unwrap().clone();
            seen.push((id, stored["forYou"].clone(), stored["forYouReason"].clone(), expected));
        }
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        for (id, for_you, reason, expected) in seen {
            assert_eq!(for_you, json!(expected), "message {id}: {reason}");
            assert!(reason.as_str().is_some_and(|r| !r.is_empty()), "message {id} needs a short reason");
            if id == 29106 { assert_eq!(reason, json!(crate::question::FOR_YOU_POKE), "a poke at her has its own reason"); }
        }
    }

    /// Review 4: a cap cancellation whose DSH stop fails still rotated the token. The hub adopts
    /// that minted epoch, keeps the driver and buffered work, and finishes ingress or tick.
    #[tokio::test]
    async fn a_failed_cap_stop_still_adopts_the_minted_token() {
        for ingress in [true, false] {
            let (root, mut hub) = queued_hub("cap-stop-fails").await;
            let key = "group:420042";
            hub.ready.clear();
            let mut state = hub.state().unwrap();
            state["conversations"][key]["lastUnreadSeq"] = json!(2);
            state["conversations"][key]["unread"] = json!([{"seq":1},{"seq":2}]);
            state["conversations"][key]["wakeConfig"]["triggers"]["nameMention"] = json!(true);
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
            let (stop_server, server) = fake_dsh(&root, &mut hub, key, true);
            let now = crate::now_ms() as i64;
            let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
            let driver = hub.drivers.get_mut(key).unwrap();
            driver.sched.turn_started_at = Some(now - cap - 14);
            driver.sched.prompt_running = true;
            driver.apply(session::Input::Message {reason:"nameMention".into(),seq:2,at_ms:now-1000});
            let state = hub.state().unwrap();
            hub.active.insert(key.into(), Active {
                task:tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Err("cancelled".into()), false) }),
                inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
                at:now-cap-14,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
            });
            let result = if ingress {
                let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
                    "message_id":29011,"message":[{"type":"text","data":{"text":"bot after a failed stop"}}]});
                let ev = serde_json::from_value(frame.clone()).unwrap();
                hub.handle(&ev,&frame.to_string(),false,9001,"bot",&HashMap::new()).await
            } else { hub.tick("bot",9001).await };
            let token = social_state::token(&hub.context,key).unwrap().unwrap();
            let adopted = hub.tokens.get(key).cloned();
            let finished_ingress = crate::activity::tail(&hub.context, 50).contains("bot after a failed stop");
            let next_tick = hub.tick("bot",9001).await;
            let retained = hub.drivers.contains_key(key) && hub.awaiting.contains_key(key)
                && !hub.drivers[key].sched.pending.is_empty();
            let _ = stop_server.send(());
            let methods = server.join().unwrap();
            for active in hub.active.values() { active.task.abort(); }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.is_ok(), "ingress={ingress}: a failed stop aborted the route: {result:?}");
            assert_eq!(methods.iter().filter(|m| *m == "session/cancel").count(), 1, "ingress={ingress}: {methods:?}");
            assert_ne!(token, "test-token", "the cancellation must still revoke the capped turn's token");
            assert_eq!(adopted.as_deref(), Some(token.as_str()), "ingress={ingress}: the minted token was not adopted");
            if ingress { assert!(finished_ingress, "ingress stopped before recording the message"); }
            next_tick.unwrap();
            assert!(retained, "ingress={ingress}: driver or buffered work was invalidated after our own rotation");
        }
    }

    #[tokio::test]
    async fn a_failed_preempt_stop_finishes_ingress_and_keeps_the_replacement_wake() {
        let (root, mut hub) = queued_hub("preempt-stop-fails").await;
        let key = "group:420042";
        hub.ready.clear();
        let mut cfg = hub.context.load_config().unwrap();
        cfg["socialV2"]["wake"]["preemptAfterMs"] = json!(1);
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        let mut state = hub.state().unwrap();
        state["conversations"][key]["lastUnreadSeq"] = json!(1);
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        let (stop_server, server) = fake_dsh(&root, &mut hub, key, true);
        let now = crate::now_ms() as i64;
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.cfg.preempt_after_ms = 1;
        driver.sched.turn_started_at = Some(now - 3000);
        driver.sched.prompt_running = true;
        hub.active.insert(key.into(), Active {
            task: tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Ok(0), false) }),
            inbound: vec![], reason: "atMention".into(), previous: state["conversations"][key].clone(),
            at: now - 3000, token: "test-token".into(), mode_stamp: hub.mode_stamp.clone(),
            session: Default::default(), minted: Default::default(), cap_abort_at: None, route: Default::default(), admitted: Default::default(),
        });
        // Only an actual mention/owner arrival may preempt a running turn.
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":29021,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":"bot after a failed preempt stop"}}]});
        let ev = serde_json::from_value(frame.clone()).unwrap();
        let result = hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await;
        let token = social_state::token(&hub.context, key).unwrap().unwrap();
        let adopted = hub.tokens.get(key).cloned();
        let finished_ingress = crate::activity::tail(&hub.context, 50).contains("bot after a failed preempt stop");
        let replacement_queued = hub.ready.iter().any(|(k, reason)| k == key && reason == "atMention");
        let next_tick = hub.tick("bot", 9001).await;
        let retained = hub.awaiting.get(key).is_some_and(|events| events.len() == 2)
            && hub.drivers.get(key).is_some_and(|d| d.sched.pending.iter().any(|e| e.reason == "atMention"));
        for active in hub.active.values() { active.task.abort(); }
        let _ = stop_server.send(());
        let methods = server.join().unwrap();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.is_ok(), "a failed preempt stop aborted ingress: {result:?}");
        assert_eq!(methods.iter().filter(|m| *m == "session/cancel").count(), 1, "{methods:?}");
        assert_ne!(token, "test-token");
        assert_eq!(adopted.as_deref(), Some(token.as_str()));
        assert!(finished_ingress, "ingress stopped before its activity record");
        assert!(replacement_queued, "preemption did not queue its replacement wake");
        next_tick.unwrap();
        assert!(retained, "our preemption token invalidated buffered ingress or its replacement wake");
    }

    /// Review fix 5: when the cap cancellation could not stop DSH, the collector is aborted a fixed
    /// headroom after the cap instead of holding the conversation until its 30-minute backstop.
    #[tokio::test]
    async fn engage_failed_cap_stop_aborts_the_collector_after_headroom() {
        let (root, mut hub) = queued_hub("cap-stop-abort").await;
        let key = "group:420042";
        hub.ready.clear();
        let (stop_server, server) = fake_dsh(&root, &mut hub, key, true);
        let now = crate::now_ms() as i64;
        let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.turn_started_at = Some(now - cap - 14);
        driver.sched.prompt_running = true;
        let state = hub.state().unwrap();
        hub.active.insert(key.into(), Active {
            task:tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Err("collector still waiting".into()), false) }),
            inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
            at:now-cap-14,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
        });
        let capped = hub.tick("bot",9001).await;
        let after_cap = crate::now_ms() as i64;
        let abort_at = hub.active.get(key).and_then(|a| a.cap_abort_at);
        let waiting = hub.active.get(key).is_some_and(|a| !a.task.is_finished());
        let early = hub.tick("bot",9001).await;
        let kept_before_headroom = hub.active.get(key).is_some_and(|a| !a.task.is_finished());
        // The headroom elapsed: the next tick aborts the collector and completion releases the slot.
        if let Some(active) = hub.active.get_mut(key) { active.cap_abort_at = Some(crate::now_ms() as i64 - 1); }
        let aborting = hub.tick("bot",9001).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while hub.active.get(key).is_some_and(|a| !a.task.is_finished()) && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        let collected = hub.tick("bot",9001).await;
        let released = !hub.active.contains_key(key);
        let _ = stop_server.send(());
        let methods = server.join().unwrap();
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        capped.unwrap();
        assert_eq!(methods.iter().filter(|m| *m == "session/cancel").count(), 1, "{methods:?}");
        let abort_at = abort_at.expect("a failed cap stop schedules the collector abort");
        assert!((now + 30_000..=after_cap + 30_000).contains(&abort_at), "abort {abort_at} is not 30 s after the cap ({now}..{after_cap})");
        assert!(waiting, "the collector is not aborted at the cap itself");
        early.unwrap();
        assert!(kept_before_headroom, "the collector is kept until the headroom elapses");
        aborting.unwrap();
        collected.unwrap();
        assert!(released, "the aborted collector's turn must be collected so the conversation can run again");
    }

    /// Review 5: a turn that completes after its cap but before a tick pumped the cap is not
    /// cancelled on the way out; turn end drops the stale control event.
    #[tokio::test]
    async fn a_turn_that_ends_within_the_cap_grace_is_not_cancelled() {
        let (root, mut hub) = queued_hub("cap-grace").await;
        let key = "group:420042";
        hub.ready.clear();
        let mut state = hub.state().unwrap();
        // The turn closed normally, so completion queues no reminder.
        state["conversations"][key]["wakeConfig"]["confirmedAt"] = json!(2);
        state["conversations"][key]["wakeConfig"]["confirmedBy"] = json!("mark_read");
        hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
        let (stop_server, server) = fake_dsh(&root, &mut hub, key, false);
        let now = crate::now_ms() as i64;
        let cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.turn_started_at = Some(now - cap - 14);
        driver.sched.prompt_running = true;
        hub.active.insert(key.into(), Active {
            task:tokio::spawn(async { DispatchResult::new(Ok(0), false) }),
            inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
            at:now-cap-14,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
        });
        while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
        let result = hub.tick("bot",9001).await;
        let token = social_state::token(&hub.context,key).unwrap().unwrap();
        let _ = stop_server.send(());
        let methods = server.join().unwrap();
        for active in hub.active.values() { active.task.abort(); }
        let turn_started = hub.drivers[key].sched.turn_started_at;
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
        assert!(!methods.iter().any(|m| m == "session/cancel"), "a finished turn was cancelled: {methods:?}");
        assert_eq!(token, "test-token", "a finished turn's epoch must not be rotated");
        assert_eq!(turn_started, None);
    }

    /// A tick sees the abandoned attempt's persisted token while publication is paused.
    #[tokio::test]
    async fn a_tick_between_abandon_persistence_and_publication_keeps_buffered_work() {
        use std::time::Duration;
        let (root, mut hub) = queued_hub("abandon-publication").await;
        let key = "group:420042";
        hub.ready.clear();
        let (stop_server, server) = fake_dsh(&root, &mut hub, key, false);
        let now = crate::now_ms() as i64;
        let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        hub.active.insert(key.into(), Active {
            task: tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Ok(0), false) }),
            inbound: vec![], reason: "atMention".into(), previous: hub.state().unwrap()["conversations"][key].clone(),
            at: now, token: "test-token".into(), mode_stamp: hub.mode_stamp.clone(),
            session: Default::default(), minted: minted.clone(), cap_abort_at: None, route: Default::default(), admitted: Default::default(),
        });
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.turn_started_at = Some(now);
        driver.sched.prompt_running = true;
        driver.apply(session::Input::Message { reason: "atMention".into(), seq: 2, at_ms: now });
        let mut pipe = pipeline::Pipeline::new_with_runtime(&hub.context, &hub.context.load_config().unwrap(), true).unwrap();
        pipe.share_minted_epoch(minted.clone());
        let (persisted, persisted_rx) = std::sync::mpsc::channel();
        let (publish, publish_rx) = std::sync::mpsc::channel();
        let rotation = std::thread::spawn(move || pipe.abandon_attempt_for_test(key, "cap-session", move || {
            persisted.send(()).unwrap();
            publish_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
        persisted_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let next = social_state::token(&hub.context, key).unwrap().unwrap();
        assert_ne!(next, "test-token", "the tick must see the persisted rotation");
        let (ticked, ticked_rx) = std::sync::mpsc::channel();
        let tick = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let result = runtime.block_on(hub.tick("bot", 9001));
            ticked.send(()).unwrap();
            (hub, result)
        });
        // A tick that sees the new state must wait for publication. If it completes early,
        // observe its resulting driver/journal state below, before any later repair can hide loss.
        let waited_for_publication = ticked_rx.recv_timeout(Duration::from_millis(200)).is_err();
        let slot_held = minted.try_lock().is_err();
        publish.send(()).unwrap();
        rotation.join().unwrap().unwrap();
        let (hub, result) = tick.join().unwrap();
        let retained = hub.drivers.contains_key(key) && hub.awaiting.contains_key(key)
            && !hub.drivers[key].sched.pending.is_empty();
        let event = format!("{key}|message:1");
        let stage = hub.journal.run(move |j| j.stage_of(&event)).await.unwrap();
        let adopted = hub.tokens.get(key).cloned();
        for active in hub.active.values() { active.task.abort(); }
        let _ = stop_server.send(());
        server.join().unwrap();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
        assert!(retained, "a tick in the publication window discarded buffered messages");
        assert_eq!(stage.as_deref(), Some("received"), "buffered ingress was settled without being processed");
        assert_eq!(adopted.as_deref(), Some(next.as_str()));
        assert!(waited_for_publication && slot_held, "rotation must hold the slot until publication");
    }

    /// After a cap cancellation the collector can abandon and rotate again. Only its own
    /// rotation keeps buffered work; an external rotation must still invalidate it.
    #[tokio::test]
    async fn a_capped_turns_own_abandon_rotation_keeps_buffered_work() {
        for own in [true, false] {
            let (root, mut hub) = queued_hub("cap-abandon").await;
            let key = "group:420042";
            hub.ready.clear();
            let now = crate::now_ms() as i64;
            let state = hub.state().unwrap();
            let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
            hub.active.insert(key.into(), Active {
                task:tokio::spawn(async { std::future::pending::<()>().await; DispatchResult::new(Err("cancelled".into()), false) }),
                inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
                at:now,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),
                minted:minted.clone(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
            });
            let driver = hub.drivers.get_mut(key).unwrap();
            driver.sched.turn_started_at = Some(now);
            driver.sched.prompt_running = true;
            driver.apply(session::Input::Message {reason:"nameMention".into(),seq:2,at_ms:now});
            // The cap cancellation rotated and the hub adopted that epoch.
            let cap_token = social_state::ensure_or_rotate(&hub.context, key, true).unwrap();
            hub.tokens.insert(key.into(), cap_token);
            // Then the collector abandons the attempt (own) or someone else rotates the token.
            let next = social_state::ensure_or_rotate(&hub.context, key, true).unwrap();
            if own { *minted.lock().unwrap() = Some(next.clone()); }
            hub.tick("bot",9001).await.unwrap();
            let retained = hub.drivers.contains_key(key) && hub.awaiting.contains_key(key)
                && !hub.drivers[key].sched.pending.is_empty();
            let adopted = hub.tokens.get(key).cloned();
            for active in hub.active.values() { active.task.abort(); }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            if own {
                assert!(retained, "the capped turn's own rotation invalidated its buffered work");
                assert_eq!(adopted.as_deref(), Some(next.as_str()));
            } else {
                assert!(!retained, "a rotation the turn did not mint must still invalidate");
            }
        }
    }

    #[tokio::test]
    async fn hf129_fix3_successful_cli_completion_consumes_the_base_followup_once() {
        let (root, mut hub) = queued_hub("hf129-fix3-cli-scheduler").await;
        let mut cfg = hub.context.load_config().unwrap();
        cfg["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
        hub.context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let now = crate::now_ms() as i64;
        let previous = hub.state().unwrap()["conversations"][key].clone();
        let (release, released) = tokio::sync::oneshot::channel();
        hub.active.insert(key.into(), Active {
            task:tokio::spawn(async move { let _ = released.await; DispatchResult::new(Ok(1), false) }),
            inbound:vec![],reason:"atMention".into(),previous:previous.clone(),at:now,
            token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),
            minted:Default::default(),cap_abort_at:None,route: std::sync::Arc::new(std::sync::Mutex::new(pipeline::Reserved2Route::Cli)), admitted: Default::default(),
        });
        hub.drivers.get_mut(key).unwrap().sched.prompt_running = true;
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":129501,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":"urgent during CLI turn"}}]});
        let event = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&event,&frame.to_string(),false,9001,"bot",&HashMap::new()).await.unwrap();
        release.send(()).unwrap();
        while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
        hub.tick("bot",9001).await.unwrap();
        let mut followup = hub.active.remove(key).expect("the base queue must dispatch one followup");
        assert_eq!(followup.inbound.len(),1);
        assert!(followup.inbound[0].ends_with("message:129501"));
        // Test the scheduler completion boundary: isolated CLI processes cannot run on Linux.
        followup.task.abort();
        let _ = (&mut followup.task).await;
        followup.task = tokio::spawn(async { DispatchResult::new(Ok(1), false) });
        *followup.route.lock().unwrap() = pipeline::Reserved2Route::Cli;
        while !followup.task.is_finished() { tokio::task::yield_now().await; }
        hub.active.insert(key.into(),followup);
        hub.tick("bot",9001).await.unwrap();
        let extra = hub.active.contains_key(key) || !hub.ready.is_empty()
            || hub.drivers[key].sched.pending_wake_at.is_some() || !hub.drivers[key].sched.pending.is_empty();
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert!(!extra,"successful CLI followup must not recreate urgent work");
    }

    // #132: exercise real ingress, token persistence/publication and completion/dispatch.
    // Hooks fix the ordering; the observation is the base queue's exactly-once handoff.
    async fn collector_completion_interleaving(capped: bool, reset: bool, twice: bool) {
        let (root, mut hub) = queued_hub("hf132-collector-completion").await;
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let mut cfg = hub.context.load_config().unwrap();
        cfg["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
        hub.context.write_atomic(RuntimeArea::Config,"config.json",cfg.to_string().as_bytes()).unwrap();
        let now = crate::now_ms() as i64;
        let previous = hub.state().unwrap()["conversations"][key].clone();
        let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let (snapshot, observed) = tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
        hub.tick_snapshot_hook = Some(snapshot);
        let (contended, contention) = std::sync::mpsc::channel();
        if capped { hub.epoch_contended_hook = Some(contended); }
        std::fs::write(root.join("user/dsh/.credentials.yaml"), "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n").unwrap();
        let mut pipe = pipeline::Pipeline::new_with_runtime(&hub.context,&cfg,true).unwrap();
        pipe.share_minted_epoch(minted.clone());
        let context = hub.context.clone();
        let task = tokio::spawn(async move {
            let done = observed.await.unwrap();
            tokio::task::spawn_blocking(move || {
                let mut snapshot_done = Some(done);
                if capped {
                    let done = snapshot_done.take().unwrap();
                    pipe.rotation_locked_hook = Some(Box::new(move || {
                        // The tick saw T0. Hold publication across slow persistence until
                        // its real adoption path witnesses contention on this mutex.
                        done.send(()).unwrap();
                        contention.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
                    }));
                }
                if twice {
                    assert!(pipe.pause_unknown_for_test(key).is_err());
                } else {
                    pipe.test_rotate_turn_token(key).unwrap();
                }
                if reset {
                    let (status, reply) = console::admin_request(&context,"POST","/api/socialV2/reset",Some(&json!({})));
                    assert_eq!(status,200,"{reply}");
                }
                if let Some(done) = snapshot_done { done.send(()).unwrap(); }
            }).await.unwrap();
            DispatchResult::new(Err("collector failed after the tick snapshot".into()), false)
        });
        // Ensure completion collection sees the finished collector, without timing sleeps.
        let collector = task.abort_handle();
        let (finished, observed_finish) = tokio::sync::oneshot::channel();
        hub.collector_finished_hook = Some(observed_finish);
        let observer = tokio::spawn(async move {
            while !collector.is_finished() { tokio::task::yield_now().await; }
            let _ = finished.send(());
        });
        hub.active.insert(key.into(), Active {
            task,inbound:vec![],reason:"atMention".into(),previous,at:now,
            token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),
            minted:minted.clone(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
        });
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.prompt_running = true;
        driver.sched.turn_started_at = Some(now);
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":132501,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":"unseen urgent after last read"}}]});
        let event = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&event,&frame.to_string(),false,9001,"bot",&HashMap::new()).await.unwrap();
        if twice {
            // A failed abandonment still persists/publishes T1. Tick must snapshot it
            // while the base queue still belongs to T0; pause_unknown then publishes T2.
            let mut abandon = pipeline::Pipeline::new_with_runtime(&hub.context,&cfg,true).unwrap();
            abandon.share_minted_epoch(minted.clone());
            assert!(abandon.abandon_attempt_for_test(key,"missing-session",|| {}).is_err());
            assert_ne!(social_state::token(&hub.context,key).unwrap().as_deref(),Some("test-token"));
            assert_eq!(hub.tokens[key],"test-token");
        }
        hub.tick("bot",9001).await.unwrap();
        observer.await.unwrap();
        let contended = capped && hub.epoch_contended_hook.is_none();
        let mut followup = hub.active.remove(key);
        let dispatched = followup.as_ref().map(|a|a.inbound.clone()).unwrap_or_default();
        if let Some(active) = followup.as_mut() {
            active.task.abort();
            let _ = (&mut active.task).await;
            // Finish the queued follow-up at the scheduler seam; no external backend is used.
            active.task = tokio::spawn(async { DispatchResult::new(Ok(1), false) });
            *active.route.lock().unwrap() = pipeline::Reserved2Route::Cli;
            while !active.task.is_finished() { tokio::task::yield_now().await; }
        }
        if let Some(active) = followup { hub.active.insert(key.into(),active); }
        hub.tick("bot",9001).await.unwrap();
        let duplicate = hub.active.contains_key(key) || hub.ready.iter().any(|(k,_)|k == key)
            || hub.drivers.get(key).is_some_and(|d|d.sched.pending_wake_at.is_some() || !d.sched.pending.is_empty());
        let wakes = std::fs::read_to_string(root.join("data/logs/rbridge-live-wakes.jsonl")).unwrap_or_default()
            .lines().filter_map(|l|serde_json::from_str::<Value>(l).ok()).filter(|v|v["key"] == key).count();
        let stage = hub.journal.run(move |j|j.stage_of(&format!("{key}|message:132501"))).await.unwrap();
        println!("HF132_INTERLEAVING capped={capped} reset={reset} contended={contended} dispatched={dispatched:?} wakes={wakes} duplicate={duplicate} stage={stage:?}");
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(contended,capped,"capped case must witness mutex contention");
        assert_eq!(dispatched,if reset {vec![]} else {vec![format!("{key}|message:132501")]},"exactly one base follow-up must survive unless explicitly reset");
        assert_eq!(wakes,usize::from(!reset));
        assert!(!duplicate,"completion must not recreate queued work");
        assert_eq!(stage.as_deref(),Some("handled"));
    }

    #[tokio::test]
    async fn hf129_fix4_collector_finishes_between_tick_snapshot_and_collection() {
        for reset in [true,false] { collector_completion_interleaving(false,reset,false).await; }
    }

    #[tokio::test]
    async fn hf132_capped_collector_rotation_contends_with_tick_adoption() {
        for reset in [true,false] { collector_completion_interleaving(true,reset,false).await; }
    }

    #[tokio::test]
    async fn hf129_fix1_two_owned_rotations_preserve_one_urgent_followup() {
        collector_completion_interleaving(false,false,true).await;
    }

    #[tokio::test]
    async fn hf129_fix4_failed_wait_write_keeps_the_urgent_followup() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for fail_write in [true, false] {
            let (root, mut hub) = queued_hub("hf129-fix4-wait-write").await;
            let key = "group:420042";
            hub.ready.clear();
            hub.awaiting.clear();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let mut cfg = hub.context.load_config().unwrap();
            cfg["consolePort"] = json!(listener.local_addr().unwrap().port());
            cfg["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
            cfg["socialV2"]["wait"] = json!({"minMs":1,"maxMs":1,"maxPerTurn":2});
            hub.context.write_atomic(RuntimeArea::Config,"config.json",cfg.to_string().as_bytes()).unwrap();
            let mut state = hub.state().unwrap();
            state["conversations"][key]["lastUnreadSeq"] = json!(1);
            state["conversations"][key]["lastReadThroughSeq"] = json!(1);
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
            let now = crate::now_ms() as i64;
            let turn = "wait-write-turn";
            crate::turn_ledger::append_record(&hub.context,&json!({"k":"begun","turn":turn,"key":key,
                "epoch":crate::send_outbox::token_epoch("test-token"),"inbound":[],"reason":"atMention","t":now})).unwrap();
            let (finish, finished) = tokio::sync::oneshot::channel();
            hub.active.insert(key.into(), Active {
                task:tokio::spawn(async move { let _ = finished.await; DispatchResult::new(Ok(0), false) }),
                inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),at:now,
                token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),
                minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
            });
            hub.drivers.get_mut(key).unwrap().sched.prompt_running = true;
            let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
                "message_id":129601,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":"unseen until a successful wait"}}]});
            let event = serde_json::from_value(frame.clone()).unwrap();
            hub.handle(&event,&frame.to_string(),false,9001,"bot",&HashMap::new()).await.unwrap();
            if fail_write {
                let lock = root.join("data/locks/authority.lock");
                let mut writes = 0;
                crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(move || {
                    writes += 1;
                    // Admission is write 1; fail only the final result/observation write.
                    if writes == 2 {
                        std::fs::remove_file(&lock).unwrap();
                        std::fs::create_dir(&lock).unwrap();
                    }
                })));
            }
            let headers = HashMap::from([("x-agent-token".into(),"test-token".into())]);
            let wait = console::handle_wait_post(&hub.context,&cfg,&headers,
                &json!({"key":key,"purpose":"messages","timeoutMs":1}).to_string());
            crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|slot| *slot.borrow_mut() = None);
            if fail_write { std::fs::remove_dir(root.join("data/locks/authority.lock")).unwrap(); }
            if fail_write { assert_eq!(wait.as_ref().unwrap_err().0,500,"{wait:?}"); }
            else { assert_eq!(wait.as_ref().unwrap()["newMessages"][0]["seq"],2); }
            // Serve the real authenticated evidence handler in the same process as the wait.
            let context = hub.context.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                let input = loop {
                    let mut buf = [0;4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    raw.extend_from_slice(&buf[..n]);
                    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&raw[..split]);
                    let length = head.lines().find_map(|line| {
                        let (name,value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap();
                    if raw.len() >= split + 4 + length { break serde_json::from_slice::<Value>(&raw[split+4..split+4+length]).unwrap(); }
                };
                let (status, packet) = console::admin_request(&context,"POST","/api/socialV2/wake-snapshot",Some(&input));
                assert_eq!(status,200);
                let body = packet.to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
                packet
            });
            crate::turn_ledger::append_record(&hub.context,&json!({"k":"ended","turn":turn,"key":key,"outcome":"completed","t":now+1})).unwrap();
            finish.send(()).unwrap();
            while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
            hub.tick("bot",9001).await.unwrap();
            let evidence = tokio::time::timeout(std::time::Duration::from_secs(5),server).await.unwrap().unwrap();
            let followup = hub.active.get(key).is_some_and(|a|a.inbound.iter().any(|k|k.ends_with("message:129601")));
            println!("HF129_FIX4_WAIT fail_write={fail_write} followup={followup} evidence={evidence}");
            for active in hub.active.values() { active.task.abort(); }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(followup,fail_write,"failed wait must retain the unseen @; successful wait must cover it");
            assert_eq!(evidence["urgentDeliveredSeqs"],if fail_write {json!([])} else {json!([2])});
        }
    }

    // The send runs on a blocking thread; its first hold yields to real hub ingress.
    // Each persistence fault occurs after that arrival and before a response can offer it.
    async fn revision_response_coverage(resuming: bool, fault: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (root, mut hub) = queued_hub("hf129-fix1-revision").await;
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut cfg = hub.context.load_config().unwrap();
        cfg["consolePort"] = json!(listener.local_addr().unwrap().port());
        cfg["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
        cfg["socialV2"]["reply"] = json!({"quietMedianMs":10000,"quietMinMs":10000,"quietMaxMs":10000});
        cfg["socialV2"]["send"] = json!({"firstGapEnabled":false,"duplicateWindowMs":0});
        hub.context.write_atomic(RuntimeArea::Config,"config.json",cfg.to_string().as_bytes()).unwrap();
        let now = crate::now_ms() as i64;
        let mut state = hub.state().unwrap();
        state["conversations"][key]["lastUnreadSeq"] = json!(1);
        state["conversations"][key]["lastReadThroughSeq"] = json!(1);
        state["conversations"][key]["lastIncomingAt"] = json!(now);
        hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
        let turn = "revision-response-turn";
        let epoch = crate::send_outbox::token_epoch("test-token");
        crate::turn_ledger::append_record(&hub.context,&json!({"k":"begun","turn":turn,"key":key,
            "epoch":epoch,"inbound":[],"reason":"atMention","t":now})).unwrap();
        let (finish, finished) = tokio::sync::oneshot::channel();
        hub.active.insert(key.into(), Active {
            task:tokio::spawn(async move { let _ = finished.await; DispatchResult::new(Ok(0), false) }),
            inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),at:now,
            token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),
            minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
        });
        hub.drivers.get_mut(key).unwrap().sched.prompt_running = true;
        // Batch claims are process-wide, even when fixtures have separate runtime roots.
        let resume_batch_id = crate::turn_ledger::mint_id("revision-resume-");
        if resuming {
            crate::send_outbox::append_record(&hub.context,&json!({"k":"intent","batch":resume_batch_id,
                "key":key,"epoch":epoch,"messages":["obsolete"],"delays":[],"t":now})).unwrap();
        }
        let (arrive, arrival) = tokio::sync::oneshot::channel();
        let (continue_send, continued) = std::sync::mpsc::channel();
        let context = hub.context.clone();
        let fault_root = root.clone();
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fault_reached = reached.clone();
        let sending = tokio::task::spawn_blocking(move || {
            struct Cleanup;
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    console::test_clock::uninstall();
                    crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|h| *h.borrow_mut() = None);
                    crate::runtime::APPEND_TEST_HOOK.with(|h| *h.borrow_mut() = None);
                }
            }
            let _cleanup = Cleanup;
            let mut arrive = Some(arrive);
            console::test_clock::install(now, move |_| {
                let Some(arrive) = arrive.take() else { return };
                arrive.send(()).unwrap();
                continued.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
                if fault == "budget" || fault == "settlement" {
                    let lock = fault_root.join("data/locks/authority.lock");
                    let reached = fault_reached.clone();
                    let mut writes = 0;
                    crate::runtime::AUTHORITY_WRITE_TEST_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(move || {
                        writes += 1;
                        if writes == if fault == "budget" {1} else {2} {
                            reached.store(true,std::sync::atomic::Ordering::SeqCst);
                            std::fs::remove_file(&lock).unwrap();
                            std::fs::create_dir(&lock).unwrap();
                        }
                    })));
                } else if fault == "locked" || fault == "sync" {
                    let reached = fault_reached.clone();
                    crate::runtime::APPEND_TEST_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(move |stage,bytes| {
                        let record: Value = serde_json::from_slice(bytes).unwrap();
                        if stage == fault && record["k"] == "superseded" {
                            reached.store(true,std::sync::atomic::Ordering::SeqCst);
                            return Err("injected revision discard failure".into());
                        }
                        Ok(())
                    })));
                }
            });
            let request = if resuming {json!({"key":key,"resumeBatchId":resume_batch_id})}
                else {json!({"key":key,"messages":["obsolete"]})};
            console::agent_request(&context,"POST","/api/socialV2/send-message",Some(&request),"test-token")
        });
        tokio::time::timeout(std::time::Duration::from_secs(10),arrival).await.unwrap().unwrap();
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
            "message_id":129701,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":"urgent during held send"}}]});
        let event = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&event,&frame.to_string(),false,9001,"bot",&HashMap::new()).await.unwrap();
        continue_send.send(()).unwrap();
        let response = sending.await.unwrap();
        if matches!(fault,"budget"|"settlement") { std::fs::remove_dir(root.join("data/locks/authority.lock")).unwrap(); }
        let failed = fault != "none";
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst),failed,"fault not exercised: {fault}: {response:?}");
        if failed {
            assert!(response.0 >= 500,"{fault}: {response:?}");
            assert!(response.1["newMessages"].is_null(),"{response:?}");
        } else {
            assert_eq!(response.0,200,"{response:?}");
            assert_eq!(response.1["newMessages"][0]["seq"],2,"{response:?}");
            assert_eq!(response.1["readThroughSeq"],2,"{response:?}");
        }
        let context = hub.context.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let input = loop {
                let mut buf = [0;4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&buf[..n]);
                let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&raw[..split]);
                let length = head.lines().find_map(|line| {
                    let (name,value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                }).unwrap();
                if raw.len() >= split + 4 + length { break serde_json::from_slice::<Value>(&raw[split+4..split+4+length]).unwrap(); }
            };
            let (status, packet) = console::admin_request(&context,"POST","/api/socialV2/wake-snapshot",Some(&input));
            assert_eq!(status,200);
            let body = packet.to_string();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            packet
        });
        crate::turn_ledger::append_record(&hub.context,&json!({"k":"ended","turn":turn,"key":key,"outcome":"completed","t":now+1})).unwrap();
        finish.send(()).unwrap();
        while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
        hub.tick("bot",9001).await.unwrap();
        let evidence = tokio::time::timeout(std::time::Duration::from_secs(5),server).await.unwrap().unwrap();
        let followup = hub.active.get(key).is_some_and(|a|a.inbound.iter().any(|k|k.ends_with("message:129701")));
        println!("HF129_FIX1_REVISION resuming={resuming} fault={fault} response={response:?} followup={followup} evidence={evidence}");
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(followup,failed,"{fault}: failed revision must preserve the base follow-up");
        assert_eq!(evidence["urgentDeliveredSeqs"],if failed {json!([])} else {json!([2])});
    }

    #[tokio::test]
    async fn hf129_fix1_revision_fresh_budget_coverage() {
        revision_response_coverage(false,"budget").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_fresh_locked_coverage() {
        revision_response_coverage(false,"locked").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_fresh_sync_coverage() {
        revision_response_coverage(false,"sync").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_fresh_settlement_coverage() {
        revision_response_coverage(false,"settlement").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_fresh_none_coverage() {
        revision_response_coverage(false,"none").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_resume_budget_coverage() {
        revision_response_coverage(true,"budget").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_resume_locked_coverage() {
        revision_response_coverage(true,"locked").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_resume_sync_coverage() {
        revision_response_coverage(true,"sync").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_resume_settlement_coverage() {
        revision_response_coverage(true,"settlement").await;
    }

    #[tokio::test]
    async fn hf129_fix1_revision_resume_none_coverage() {
        revision_response_coverage(true,"none").await;
    }

    #[tokio::test]
    async fn missing_token_before_first_wake_preserves_queued_ingress() {
        let (root, mut hub) = queued_hub("missing-token-first-wake").await;
        let mut state = hub.state().unwrap();
        for key in ["group:420042", "group:420043"] {
            state["conversations"][key].as_object_mut().unwrap().remove("agentToken");
            hub.tokens.insert(key.into(), String::new());
        }
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        let active_count = hub.active.len();
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(active_count, 2, "an absent, unchanged token must not discard first-wake ingress");
    }

    #[tokio::test]
    async fn a_running_turn_keeps_its_budget_and_the_next_dispatch_adopts_reloaded_settings() {
        let (root, mut hub) = queued_hub("budget-reload").await;
        let key = "group:420042";
        hub.ready.clear();
        let now = crate::now_ms() as i64;
        let state = hub.state().unwrap();
        let (finish, finished) = tokio::sync::oneshot::channel();
        hub.active.insert(key.into(), Active {
            task:tokio::spawn(async move { let _ = finished.await; DispatchResult::new(Err("回合已取消".into()), false) }),
            inbound:vec![],reason:"atMention".into(),previous:state["conversations"][key].clone(),
            at:now,token:"test-token".into(),mode_stamp:hub.mode_stamp.clone(),session:Default::default(),minted:Default::default(),cap_abort_at:None,route: Default::default(), admitted: Default::default(),
        });
        hub.drivers.get_mut(key).unwrap().sched.turn_started_at = Some(now);
        let mut cfg = hub.context.load_config().unwrap();
        cfg["socialV2"]["wake"]["preSleepWaitMs"] = json!(450000);
        hub.context.write_atomic(RuntimeArea::Config,"config.json",cfg.to_string().as_bytes()).unwrap();
        hub.tick("bot",9001).await.unwrap();
        let running_cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        finish.send(()).unwrap();
        while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
        hub.ready.push((key.into(),"atMention".into()));
        hub.tick("bot",9001).await.unwrap();
        let next_cap = hub.drivers[key].sched.cfg.turn_hard_cap_ms;
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(running_cap,515000,"a reload must not change a running turn's dispatched budget");
        assert_eq!(next_cap,665000,"a same-tick replacement must use the new collector's budget");
    }

    #[tokio::test]
    async fn settings_invalidation_records_the_fired_predicate_without_tokens() {
        for predicate in ["suspended", "notTaken", "notAllowed", "missing", "tokenChanged"] {
            let (root, mut hub) = queued_hub("invalidation-predicate").await;
            let key = "group:420042";
            hub.ready.clear();
            let mut state = hub.state().unwrap();
            match predicate {
                "suspended" => state["paused"] = json!(true),
                "notTaken" => { hub.live_all = false; hub.groups = vec!["group:420043".into()]; },
                "notAllowed" => {
                    let mut cfg = hub.context.load_config().unwrap();
                    cfg["allow"]["groups"] = json!([420043]);
                    hub.context.write_atomic(RuntimeArea::Config,"config.json",cfg.to_string().as_bytes()).unwrap();
                },
                "missing" => { state["conversations"].as_object_mut().unwrap().remove(key); },
                "tokenChanged" => state["conversations"][key]["agentToken"] = json!("rotated-test-token"),
                _ => unreachable!(),
            }
            hub.context.write_atomic(RuntimeArea::Data,"social-v2.json",state.to_string().as_bytes()).unwrap();
            hub.tick("bot",9001).await.unwrap();
            let log = std::fs::read_to_string(root.join("data/logs/rbridge-live.jsonl")).unwrap();
            let record = log.lines().filter_map(|l|serde_json::from_str::<Value>(l).ok())
                .find(|v|v["key"] == key && v["reason"] == "settings-invalidated").unwrap();
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(record["predicate"],predicate);
            assert!(!log.contains("test-token"));
        }
    }

    #[tokio::test]
    async fn t48_pause_during_claim_keeps_queued_ingress() {
        let fixture = queued_hub("t48-pause-before-claim").await;
        let _cleanup = CleanupRoot(fixture.0.clone());
        let (_, mut hub) = fixture;
        let key = "group:420042";
        let event = format!("{key}|message:1");
        hub.ready.truncate(1);
        let context = hub.context.clone();
        let (snapshot, reached) = tokio::sync::oneshot::channel();
        hub.tick_snapshot_hook = Some(snapshot);
        let pause = async {
            let resume = reached.await.unwrap();
            crate::conversation_state::update_raw(&context, |state| {
                state["paused"] = json!(true);
                Ok(())
            }).unwrap();
            resume.send(()).unwrap();
        };
        let (tick, ()) = tokio::join!(hub.tick("bot", 9001), pause);
        tick.unwrap();

        for _ in 0..2 {
            assert_eq!(hub.awaiting.get(key).map(Vec::as_slice), Some(std::slice::from_ref(&event)),
                "pausing during the claim must preserve queued ingress");
            assert!(hub.active.is_empty());
            let check = event.clone();
            assert_eq!(hub.journal.run(move |j| j.stage_of(&check)).await.unwrap().as_deref(), Some("received"));
            hub.tick("bot", 9001).await.unwrap();
        }
        crate::conversation_state::update_raw(&hub.context, |state| {
            state["paused"] = json!(false);
            Ok(())
        }).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        let active = hub.active.remove(key).expect("the queued wake must dispatch after unpause");
        assert_eq!(active.inbound, [event]);
        active.task.abort();
        let _ = active.task.await;
        drop(hub);
    }

    #[tokio::test]
    async fn review_claim_failure_requeues_all_drained_jobs_with_bounded_retry() {
        let (root, mut hub) = queued_hub("claim-fault").await;
        std::fs::create_dir_all(root.join("data/locks/social-v2.lock")).unwrap();
        assert_predispatch_retry(&mut hub).await;
        for attempt in 2..=4 {
            for (at, _) in hub.retries.values_mut() { *at = 0; }
            hub.tick("bot", 9001).await.unwrap();
            assert_eq!(hub.failures["group:420042"], attempt);
        }
        assert!(hub.retries.is_empty(), "retry must stop after three retries");
        assert!(hub.ready.is_empty());
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn review_log_failure_rolls_back_claim_and_preserves_every_drained_job() {
        let (root, mut hub) = queued_hub("log-fault").await;
        let log = root.join("data/logs/rbridge-live-wakes.jsonl");
        std::fs::create_dir(&log).unwrap();
        assert_predispatch_retry(&mut hub).await;
        std::fs::remove_dir(log).unwrap();
        for (at, _) in hub.retries.values_mut() { *at = 0; }
        hub.tick("bot", 9001).await.unwrap();
        assert_eq!(hub.active.len(), 2, "at-only jobs must dispatch after the fault clears without ingress");
        for active in hub.active.values() { active.task.abort(); }
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn review_crash_after_finite_claim_before_task_poll_recovers_timer_without_ingress() {
        let (root, mut hub) = queued_hub("finite-claim-crash").await;
        let key = "group:420042";
        hub.ready = vec![(key.into(), "timeout".into())];
        hub.awaiting.clear();
        let mut state = hub.state().unwrap();
        let deadline = json!("2000-01-01T00:00:00.000Z");
        state["conversations"][key]["wakeConfig"]["infinite"] = json!(false);
        state["conversations"][key]["wakeConfig"]["sleepUntil"] = deadline.clone();
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        // The current-thread runtime cannot poll the spawned pipeline until this task yields.
        // Abort at the actual persisted claim / task-start boundary, just as a process kill does.
        assert!(crate::turn_ledger::read_records(&hub.context).unwrap().is_empty());
        hub.active.remove(key).unwrap().task.abort();
        let context = hub.context.clone();
        let journal = hub.journal.clone();
        drop(hub);
        assert_eq!(context.read_social_state().unwrap()["conversations"][key]["wakeConfig"]["sleepUntil"], deadline, "crash consumed the only durable timer before any turn existed");
        let mut restarted = Hub::new(&context, journal, vec![], true);
        restarted.tick("bot", 9001).await.unwrap();
        assert_eq!(restarted.active[key].reason, "timeout", "restart must recover a timer-only wake with proactive scheduling disabled");
        restarted.active[key].task.abort();
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn review_recovery_restored_deadline_rearms_an_existing_idle_driver() {
        let (root, mut hub) = queued_hub("restored-timer").await;
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let mut state = hub.state().unwrap();
        state["conversations"][key]["wakeConfig"]["confirmedAt"] = json!(2);
        state["conversations"][key]["wakeConfig"]["infinite"] = json!(false);
        state["conversations"][key]["wakeConfig"]["sleepUntil"] = json!("2000-01-01T00:00:00.000Z");
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        let at = crate::now_ms() as i64;
        let (previous, token) = social_state::claim_reserved2_wake(&hub.context, key, "timeout", at, None).unwrap();
        social_state::consume_reserved2_deadline(&hub.context, key, &token, at, &previous).unwrap();
        // Startup can create the idle driver before asynchronous recovery restores this lease.
        hub.tick("bot", 9001).await.unwrap();
        assert!(!hub.active.contains_key(key));
        social_state::rollback_reserved2_wake(&hub.context, key, at, &previous).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        assert!(hub.active.contains_key(key), "recovery restored a deadline but the already-synced driver never noticed it");
        hub.active[key].task.abort();
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn refreshed_default_wake_config_reaches_a_running_driver_without_a_new_confirmation() {
        // Default wake-config refreshes preserve `confirmedAt`, so `WakeSync` diffs the whole config
        // rather than relying on `confirmedAt` alone (pinned by this test).
        let (root, mut hub) = queued_hub("refreshed-default").await;
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let mut state = hub.state().unwrap();
        state["conversations"][key]["wakeConfig"]["confirmedBy"] = json!("default");
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        assert_eq!(hub.drivers[key].sched.wc.batch_window_ms, None);
        assert!(!hub.drivers[key].sched.wc.mode_active);
        // The operator changes a default: same record, same `confirmedAt`, new settings.
        let mut state = hub.state().unwrap();
        state["conversations"][key]["wakeConfig"]["batchWindowMs"] = json!(4500);
        state["conversations"][key]["wakeConfig"]["mode"] = json!("active");
        state["conversations"][key]["wakeConfig"]["triggers"]["keywords"] = json!(["refreshed"]);
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        let wc = &hub.drivers[key].sched.wc;
        assert_eq!(wc.batch_window_ms, Some(4500), "the live driver kept the old batch window");
        assert!(wc.mode_active);
        assert_eq!(wc.keywords, vec!["refreshed".to_string()]);
        // A counter the bridge itself bumps is not a settings change and must not reset timers.
        hub.drivers.get_mut(key).unwrap().sched.pending_wake_at = Some(crate::now_ms() as i64 + 60_000);
        let mut state = hub.state().unwrap();
        state["conversations"][key]["wakeConfig"]["wakeCount"] = json!(7);
        state["conversations"][key]["wakeConfig"]["lastWakeAt"] = json!(123);
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        hub.tick("bot", 9001).await.unwrap();
        assert!(hub.drivers[key].sched.pending_wake_at.is_some(), "a counter update dropped the pending wake");
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn review_busy_reply_timer_rearms_and_current_reply_suppresses_extra_turn() {
        let (root, mut hub) = queued_hub("busy-reply-timer").await;
        let key = "group:420042";
        hub.ready.clear();
        hub.awaiting.clear();
        let now = crate::now_ms() as i64;
        let mut state = hub.state().unwrap();
        state["conversations"][key]["lastAiReplyAt"] = json!(now - 5000);
        state["conversations"][key]["lastIncomingAt"] = json!(now - 4000);
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        let driver = hub.drivers.get_mut(key).unwrap();
        driver.sched.cfg.auto_reply_check_ms = 1000;
        driver.apply(session::Input::ReplySent { at_ms: now - 5000, text: "first send".into() });
        let (release, wait) = tokio::sync::oneshot::channel();
        hub.active.insert(key.into(), Active {
            task: tokio::spawn(async move { wait.await.unwrap(); DispatchResult::new(Ok(0), false) }),
            inbound: vec![], reason: "atMention".into(), previous: state["conversations"][key].clone(),
            at: now - 3000, token: "test-token".into(), mode_stamp: hub.mode_stamp.clone(), session: Default::default(), minted: Default::default(), cap_abort_at: None, route: Default::default(), admitted: Default::default(),
        });
        driver.sched.turn_started_at = Some(now - 3000);
        hub.tick("bot", 9001).await.unwrap();
        // The first send's check expires while the new message's turn is still running.
        assert!(hub.active.contains_key(key));
        state["conversations"][key]["lastAiReplyAt"] = json!(now + 1);
        state["conversations"][key]["unread"] = json!([]);
        state["conversations"][key]["wakeConfig"]["confirmedAt"] = json!(2);
        state["conversations"][key]["wakeConfig"]["confirmedBy"] = json!("mark_read");
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        release.send(()).unwrap();
        while !hub.active[key].task.is_finished() { tokio::task::yield_now().await; }
        hub.tick("bot", 9001).await.unwrap();
        assert!(!hub.active.contains_key(key), "expired busy reply-check became an unconditional extra turn after an actual reply and read mark");
        let d = hub.drivers.get_mut(key).unwrap();
        assert!(d.pump(now + 120_000).is_empty(), "rearmed reply-check must inspect current reply state");
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_replayed_inbound_event_does_not_write_a_second_activity_line() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-replay-activity-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), json!({"consoleToken":"synthetic-replay-token","allow":{"groups":[420042]},"slang":{"enabled":false}}).to_string()).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","onebot_ws":"ws://127.0.0.1:1"}"#).unwrap();
        let key = "group:420042";
        let state = json!({"conversations":{key:{"agentToken":"test-token","bootstrapSent":true,"unread":[],"recentMessages":[],"wakeConfig":{"confirmedAt":1,"mode":"diving","infinite":true,"triggers":{"atMention":true}}}}});
        std::fs::write(root.join("data/social-v2.json"), state.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&context).unwrap());
        let mut hub = Hub::new(&context, journal, vec![key.into()], false);
        hub.tick("bot", 9001).await.unwrap();
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,"message_id":16201,"message":[{"type":"text","data":{"text":"logged once"}}]});
        let ev: crate::Event = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
        // After a restart the journal hands the same event back with `replayed` set.
        hub.handle(&ev, &frame.to_string(), true, 9001, "bot", &HashMap::new()).await.unwrap();
        let log = std::fs::read_to_string(root.join("data/qq-activity.log")).unwrap_or_default();
        assert_eq!(log.matches("[reserved2] 收到一则新的会话来信，已经顺手记入未读待办列表以便稍后处理：logged once").count(), 1, "{log}");
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn configuration_written_just_before_ingress_keeps_the_new_batch() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-config-ingress-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), json!({"consoleToken":"synthetic-config-token","allow":{"groups":[420042]},"slang":{"enabled":false},"socialV2":{"wake":{"batchWindowMs":1000}}}).to_string()).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","onebot_ws":"ws://127.0.0.1:1"}"#).unwrap();
        let key = "group:420042";
        let mut state = json!({"conversations":{key:{"agentToken":"test-token","bootstrapSent":true,"unread":[],"recentMessages":[],"wakeConfig":{"confirmedAt":1,"mode":"diving","infinite":true,"batchWindowMs":1000,"triggers":{"atMention":true}}}}});
        let path = root.join("data/social-v2.json");
        std::fs::write(&path, state.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&context).unwrap());
        let mut hub = Hub::new(&context, journal, vec![key.into()], false);
        hub.tick("bot", 9001).await.unwrap();
        state["conversations"][key]["wakeConfig"]["confirmedAt"] = json!(2);
        state["conversations"][key]["wakeConfig"]["mode"] = json!("active");
        state["conversations"][key]["wakeConfig"]["triggers"]["anyMessage"] = json!(true);
        std::fs::write(&path, state.to_string()).unwrap();
        let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,"message_id":16102,"message":[{"type":"text","data":{"text":"active message"}}]});
        let ev = serde_json::from_value(frame.clone()).unwrap();
        hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
        hub.tick("bot", 9001).await.unwrap();
        assert_eq!(hub.drivers[key].sched.pending_wake_reason.as_deref(), Some("anyMessage"), "configuration adoption discarded the first active-mode message");
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn t48_denied_ingress_leaves_bootstrap_unclaimed() {
        let (root, hub) = queued_hub("t48-denied-ingress").await;
        let _cleanup = CleanupRoot(root);
        let mut hub = hub;
        hub.ready.clear();
        let key = "group:420042";
        let mut state = hub.state().unwrap();
        state["conversations"][key]["bootstrapSent"] = json!(false);
        hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
        for (id, allow, deny) in [
            (48, json!([]), json!([])),
            (49, json!([420042]), json!([420042])),
        ] {
            let mut cfg = hub.context.load_config().unwrap();
            cfg["allow"]["groups"] = allow;
            cfg["deny"] = json!({"groups":deny});
            cfg["allowAllWhenEmpty"] = json!(true);
            cfg["slang"] = json!({"enabled":false});
            let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,
                "message_id":id,"message":[{"type":"text","data":{"text":"fixture wake"}}]});
            let raw = frame.to_string();
            let ev = serde_json::from_value(frame).unwrap();
            hub.handle_with_config(cfg, &ev, &raw, false, 9001, "bot", &HashMap::new()).await.unwrap();
            let event_key = inbound::event_key(key, "message", Some(&id.to_string()), &raw);
            assert_eq!(hub.journal.run(move |j| j.stage_of(&event_key)).await.unwrap(), None,
                "refused ingress must never enter wake evaluation");
            assert_eq!(hub.state().unwrap()["conversations"][key]["bootstrapSent"], false);
            assert!(hub.ready.is_empty() && hub.active.is_empty());
        }
        assert!(crate::turn_ledger::read_records(&hub.context).unwrap().is_empty());
        assert!(crate::send_outbox::read_records(&hub.context).unwrap().is_empty());
    }

    /// #186: the reserved2 ingress gate reads the allow and deny lists of each event's configuration.
    #[tokio::test]
    async fn t186_ingress_reads_allow_and_deny_from_each_events_configuration() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-t186-ingress-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let write_config = |allow: Value, deny: Value| {
            let config = json!({"consoleToken":"synthetic-scope-token","allow":{"groups":allow},"deny":{"groups":deny},"slang":{"enabled":false}});
            std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        };
        write_config(json!([420042]), json!([]));
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","onebot_ws":"ws://127.0.0.1:1"}"#).unwrap();
        let mut state = json!({"conversations":{}});
        for key in ["group:420042", "group:420044"] {
            state["conversations"][key] = json!({"agentToken":"test-token","bootstrapSent":true,"unread":[],"recentMessages":[],"wakeConfig":{"confirmedAt":1,"mode":"diving","infinite":true,"triggers":{"atMention":true}}});
        }
        std::fs::write(root.join("data/social-v2.json"), state.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&context).unwrap());
        // Both conversations are served (`--live-groups`); only the configuration decides.
        let mut hub = Hub::new(&context, journal, vec!["group:420042".into(), "group:420044".into()], false);
        hub.tick("bot", 9001).await.unwrap();
        async fn recorded(hub: &mut Hub, message_id: i64) -> bool {
            let frame = json!({"post_type":"message","message_type":"group","group_id":420044,"user_id":1001,"message_id":message_id,"message":[{"type":"text","data":{"text":"scope probe"}}]});
            let ev: crate::Event = serde_json::from_value(frame.clone()).unwrap();
            hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
            let id = message_id.to_string();
            let event = inbound::event_key("group:420044", "message", Some(id.as_str()), &frame.to_string());
            // Recording the same event again is a duplicate only when the hub recorded it.
            hub.journal.record_received(event, "group:420044".into(), frame.to_string(), 2).await.unwrap() == inbound::Receipt::Duplicate
        }
        assert!(!recorded(&mut hub, 18601).await, "an unlisted conversation must be refused before the journal");
        write_config(json!([420042, 420044]), json!([]));
        assert!(recorded(&mut hub, 18602).await, "a conversation added to the allow list mid-run must be admitted and recorded");
        write_config(json!([420042, 420044]), json!([420044]));
        assert!(!recorded(&mut hub, 18603).await, "a denied conversation must be refused even when it is listed");
        write_config(json!([420042]), json!([]));
        assert!(!recorded(&mut hub, 18604).await, "a conversation removed from the allow list mid-run must be refused");
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// #192 (F3): the hub handles an event with the configuration its caller read, even when `config.json` cannot be
    /// read any more by the time the event reaches it; `handle`, which reads the file itself, refuses then.
    #[tokio::test]
    async fn t192_handle_with_config_uses_the_callers_read_while_the_file_is_unreadable() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-t192-callers-read-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let config = json!({"consoleToken":"synthetic-hold-token","allow":{"groups":[420042]},"slang":{"enabled":false}});
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","onebot_ws":"ws://127.0.0.1:1"}"#).unwrap();
        let state = json!({"conversations":{"group:420042":{"agentToken":"test-token","bootstrapSent":true,"unread":[],"recentMessages":[],"wakeConfig":{"confirmedAt":1,"mode":"diving","infinite":true,"triggers":{"atMention":true}}}}});
        std::fs::write(root.join("data/social-v2.json"), state.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = inbound::SharedJournal::new(inbound::Journal::open(&context).unwrap());
        let mut hub = Hub::new(&context, journal, vec!["group:420042".into()], false);
        hub.tick("bot", 9001).await.unwrap();
        let read = context.load_config().unwrap();
        std::fs::write(root.join("config/config.json"), "{ not json").unwrap();
        let frame = |message_id: i64| json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,"message_id":message_id,"message":[{"type":"text","data":{"text":"held probe"}}]});
        let first = frame(19201);
        let ev: crate::Event = serde_json::from_value(first.clone()).unwrap();
        assert!(
            hub.handle(&ev, &first.to_string(), false, 9001, "bot", &HashMap::new()).await.is_err(),
            "a hub that reads the unreadable file itself cannot decide"
        );
        let second = frame(19202);
        let ev: crate::Event = serde_json::from_value(second.clone()).unwrap();
        hub.handle_with_config(read, &ev, &second.to_string(), false, 9001, "bot", &HashMap::new())
            .await
            .expect("the caller's read decides the event");
        let event = inbound::event_key("group:420042", "message", Some("19202"), &second.to_string());
        assert_eq!(
            hub.journal.record_received(event, "group:420042".into(), second.to_string(), 2).await.unwrap(),
            inbound::Receipt::Duplicate,
            "the event was recorded with the caller's read"
        );
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// #186: the administrator adds `deny` for one group and keeps it in `allow`.
    fn t186_deny_keeping_allow(root: &std::path::Path, group: i64) {
        let path = root.join("config/config.json");
        let mut cfg: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cfg["deny"] = json!({"groups": [group]});
        std::fs::write(&path, cfg.to_string()).unwrap();
    }

    /// #186: a wake queued while the conversation was allowed never dispatches once it is denied (allow unchanged).
    /// The allowed run is the control: the same queue dispatches.
    #[tokio::test]
    async fn t186_a_wake_queued_while_allowed_does_not_dispatch_once_denied() {
        for denied in [false, true] {
            let (root, mut hub) = queued_hub(if denied { "t186-tick-denied" } else { "t186-tick-allowed" }).await;
            let key = "group:420042";
            hub.ready = vec![(key.into(), "timeout".into())];
            hub.awaiting.clear();
            let mut state = hub.state().unwrap();
            state["conversations"][key]["wakeConfig"]["infinite"] = json!(false);
            state["conversations"][key]["wakeConfig"]["sleepUntil"] = json!("2000-01-01T00:00:00.000Z");
            hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
            if denied {
                t186_deny_keeping_allow(&root, 420042);
            }
            hub.tick("bot", 9001).await.unwrap();
            let dispatched = hub.active.contains_key(key);
            for active in hub.active.values() {
                active.task.abort();
            }
            drop(hub);
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(dispatched, !denied, "denied={denied}: a queued wake must dispatch only while the conversation is admitted");
        }
    }

    /// #186: wake claims and wake snapshots refuse a denied conversation whose allow entry stays.
    #[tokio::test]
    async fn t186_wake_claims_and_wake_snapshots_refuse_a_denied_conversation() {
        let (root, hub) = queued_hub("t186-deny-claim").await;
        let key = "group:420042";
        let snapshot = || {
            console::admin_request(&hub.context, "POST", "/api/socialV2/wake-snapshot", Some(&json!({"key": key, "token": "test-token", "readOnly": true})))
        };
        let (status, reply) = snapshot();
        assert_eq!(status, 200, "allowed: the wake snapshot answers: {reply}");
        social_state::claim_reserved2_wake(&hub.context, key, "timeout", crate::now_ms() as i64, None).expect("allowed: the wake claim succeeds");
        t186_deny_keeping_allow(&root, 420042);
        let (status, reply) = snapshot();
        assert_eq!(status, 403, "a denied conversation's wake snapshot must be refused: {reply}");
        let refused = social_state::claim_reserved2_wake(&hub.context, key, "timeout", crate::now_ms() as i64, None);
        assert_eq!(refused.unwrap_err().message(), social_state::WAKE_ACCESS_REFUSED);
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }

    mod ticket24_base { include!("reserved2_ticket24_base_tests.rs"); }
    mod ticket24 { include!("reserved2_ticket24_tests.rs"); }

    /// Ticket 25 review 2: wakes waiting for DSH are bounded per conversation (`dsh_health::QUEUE_MAX` per key, the oldest of that
    /// conversation dropped); no process-wide cap ever throws one conversation's work away for another's.
    #[test]
    fn waiting_wakes_are_bounded_per_conversation_not_per_process() {
        let mut wait = DshWait::default();
        for n in 0..120 {
            assert_eq!(wait.push(&format!("group:{n}"), n, "batch".into()), None, "conversation {n} has its own queue");
        }
        for n in 0..crate::dsh_health::QUEUE_MAX as i64 {
            assert_eq!(wait.push("group:full", 1_000 + n, format!("r{n}")), None);
        }
        assert_eq!(wait.push("group:full", 2_000, "r-new".into()), Some((1_000, "r0".to_string())), "that conversation's oldest wake goes");
        assert_eq!(wait.len("group:full"), crate::dsh_health::QUEUE_MAX);
        assert_eq!(wait.len("group:0"), 1, "other conversations lost nothing");
        let released = wait.drain();
        assert_eq!(released.len(), 120 + crate::dsh_health::QUEUE_MAX, "every waiting wake is released in the order it came");
        assert_eq!(released.last().unwrap().1, "r-new");
        assert!(wait.is_empty());
    }

    #[test]
    fn a_conversation_that_ended_its_epoch_forgets_its_waiting_wakes_only() {
        let mut wait = DshWait::default();
        wait.push("group:1", 1, "a".into());
        wait.push("group:2", 2, "b".into());
        wait.remove("group:1");
        assert_eq!(wait.len("group:1"), 0);
        assert_eq!(wait.len("group:2"), 1);
    }
}

enum MaintenanceResult {
    Digest(crate::session_rotation::DigestResult),
    Archived(Result<(),String>),
}
struct Maintenance {
    plan: crate::session_rotation::Plan,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<MaintenanceResult>,
    digested: bool,
}

/// None lets idle ingress evaluate its ordinary triggers. Some never rolls probability.
pub(crate) fn in_turn_decision(in_turn: bool, at_self: bool, is_owner: bool, owner: &str) -> Option<(bool, String)> {
    if !in_turn { return None; }
    Some(if at_self { (true, "atMention".into()) }
        else if is_owner { (true, format!("speaker:{owner}")) }
        else { (false, "in-turn".into()) })
}

pub struct Hub {
    #[cfg(test)]
    state_read_failure_after: std::cell::Cell<Option<usize>>,
    #[cfg(test)]
    tick_snapshot_hook: Option<tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(test)]
    epoch_contended_hook: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(test)]
    collector_finished_hook: Option<tokio::sync::oneshot::Receiver<()>>,
    context: RuntimeContext,
    journal: inbound::SharedJournal,
    drivers: HashMap<String, session::Driver>,
    synced: HashMap<String, session::WakeSync>,
    awaiting: HashMap<String, Vec<String>>,
    /// Message wake provenance until dispatch, independent of the bounded recent buffer.
    queued_messages: HashMap<String, Vec<(i64, String)>>,
    active: HashMap<String, Active>,
    /// Recovery owns these DSH tasks outside Hub::active: (turn id, token).
    recovered: HashMap<String, (String, String)>,
    maintenance: HashMap<String, Maintenance>,
    maintenance_scan_at: i64,
    ready: Vec<(String, String)>,
    /// Batches whose claim detected suspension after the tick read its settings.
    suspended_claims: HashSet<String>,
    groups: Vec<String>,
    live_all: bool,
    mode_stamp: String,
    /// `social-v2.json` `resetEpoch`: a console whole reset (`POST /api/socialV2/reset`).
    reset_stamp: Value,
    held: HashMap<String, String>,
    /// Wakes that fell due while DSH was down, or whose turn could not reach it, per conversation. Replayed through the
    /// normal dispatch (every validity check) once DSH is back.
    dsh_wait: DshWait,
    tokens: HashMap<String, String>,
    retries: HashMap<String, (i64, String)>,
    rollback_pending: HashMap<String, (i64, Value)>,
    rearm_pending: HashMap<String, (i64, Value)>,
    failures: HashMap<String, u32>,
    closing_seen: HashMap<String, Value>,
    closing_misses: HashMap<String, u32>,
    slang: crate::slang_learn::SlangLearning,
    #[cfg(test)]
    slang_catch_up: Option<std::thread::JoinHandle<()>>,
    /// Member-name and quoted-message lookups (and their caches) for inbound text; built on first use.
    lookups: Option<chat::Lookups>,
    /// Ticket 33: when a Schedule store error was last logged (at most one line a minute; the hub ticks every 500 ms).
    schedule_log_at: i64,
}

#[cfg(test)]
impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(handle) = self.slang_catch_up.take() {
            let _ = handle.join();
        }
    }
}

impl Hub {
    pub fn new(
        context: &RuntimeContext,
        journal: inbound::SharedJournal,
        groups: Vec<String>,
        live_all: bool,
    ) -> Self {
        let mode_stamp = context
            .read_optional_text(RuntimeArea::Data, "mode.json")
            .ok()
            .flatten()
            .unwrap_or_default();
        let slang = crate::slang_learn::SlangLearning::new(context);
        #[cfg(test)]
        let slang_catch_up = slang.start_catch_up();
        #[cfg(not(test))]
        let _ = slang.start_catch_up();
        Self {
            #[cfg(test)]
            state_read_failure_after: std::cell::Cell::new(None),
            #[cfg(test)]
            tick_snapshot_hook: None,
            #[cfg(test)]
            epoch_contended_hook: None,
            #[cfg(test)]
            collector_finished_hook: None,
            context: context.clone(),
            journal,
            drivers: HashMap::new(),
            synced: HashMap::new(),
            awaiting: HashMap::new(),
            queued_messages: HashMap::new(),
            active: HashMap::new(),
            recovered: HashMap::new(),
            maintenance: HashMap::new(),
            maintenance_scan_at: 0,
            ready: Vec::new(),
            suspended_claims: HashSet::new(),
            groups,
            live_all,
            mode_stamp,
            reset_stamp: crate::conversation_state::ConversationState::for_runtime(context)
                .read_optional_document()
                .map(|s| s.reset_epoch())
                .unwrap_or(Value::Null),
            held: HashMap::new(),
            dsh_wait: DshWait::default(),
            tokens: HashMap::new(),
            retries: HashMap::new(),
            rollback_pending: HashMap::new(),
            rearm_pending: HashMap::new(),
            failures: HashMap::new(),
            closing_seen: crate::conversation_state::ConversationState::for_runtime(context)
                .read_optional_document()
                .map(|s| {
                    s.conversations()
                        .map(|(k, c)| {
                            (
                                k.to_string(),
                                c.field("wakeConfig")
                                    .and_then(|w| w.get("confirmedAt"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            closing_misses: HashMap::new(),
            slang,
            #[cfg(test)]
            slang_catch_up,
            lookups: None,
            schedule_log_at: 0,
        }
    }

    /// Render an inbound message via `inbound_text::render_inbound` using gateway lookups; a failed
    /// lookup falls back to `@<qq>` / `[引用消息]` (pinned by `inbound_text` tests).
    fn render(&mut self, cfg: &Value, key: &str, message: &Value, self_id: i64) -> inbound_text::Rendered {
        if self.lookups.is_none() {
            match chat::onebot_of(&self.context, cfg) {
                Ok(ob) => self.lookups = Some(chat::Lookups::new(ob)),
                Err(error) => println!("[reserved2] OneBot lookups unavailable: {error}"),
            }
        }
        let (kind, conv) = key.split_once(':').unwrap_or(("", key));
        match self.lookups.as_mut() {
            Some(lookups) => {
                let mut res = chat::GroupResolver {
                    lookups,
                    group: (kind == "group").then(|| conv.to_string()),
                    kind: kind.to_string(),
                    conv: conv.to_string(),
                    self_id,
                };
                inbound_text::render_inbound(message, self_id, &mut res)
            }
            None => inbound_text::render_inbound(message, self_id, &mut inbound_text::NoLookups),
        }
    }

    fn state_doc(&self) -> Result<crate::conversation_state::SocialDocument, String> {
        #[cfg(test)]
        if let Some(remaining) = self.state_read_failure_after.take() {
            if remaining == 0 { return Err("synthetic social-state read failure".into()); }
            self.state_read_failure_after.set(Some(remaining - 1));
        }
        crate::conversation_state::ConversationState::for_runtime(&self.context)
            .read_optional_document()
            .map_err(|(_, e)| e)
    }

    fn state(&self) -> Result<Value, String> {
        self.state_doc().map(crate::conversation_state::SocialDocument::into_value)
    }

    /// Keep read backoff separate from scan scheduling: a deferred recovery still blocks dispatch.
    fn rotation_state_doc(&self, cfg: &Value, step: &'static str, now: i64) -> Result<crate::conversation_state::SocialDocument, String> {
        let keys: std::collections::BTreeSet<_> = self.maintenance.keys().chain(self.drivers.keys()).collect();
        if keys.iter().any(|key| crate::session_rotation::read_retry_at(&self.context, key, step) > now) {
            return Err("session rotation read retry deferred".into());
        }
        let result = self.state_doc();
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        for key in keys {
            let _ = crate::session_rotation::rotation_read(&self.context, cfg, key, step, now, status.clone());
        }
        result
    }

    /// Keep read backoff separate from scan scheduling: a deferred recovery still blocks dispatch.
    fn rotation_state(&self, cfg: &Value, step: &'static str, now: i64) -> Result<Value, String> {
        self.rotation_state_doc(cfg, step, now)
            .map(crate::conversation_state::SocialDocument::into_value)
    }

    /// Forget every queued, held, retried and awaited job (their ingress is settled).
    async fn drop_everything(&mut self, now: i64) -> Result<(), String> {
        self.ready.clear();
        self.suspended_claims.clear();
        self.recovered.clear();
        self.queued_messages.clear();
        self.held.clear();
        self.dsh_wait.clear();
        self.drivers.clear();
        self.synced.clear();
        self.tokens.clear();
        self.retries.clear();
        self.rollback_pending.clear();
        self.rearm_pending.clear();
        self.failures.clear();
        self.closing_seen.clear();
        self.closing_misses.clear();
        for m in self.maintenance.values() {m.cancel.store(true,std::sync::atomic::Ordering::SeqCst);}
        let keys = self.awaiting.drain().flat_map(|(_, keys)| keys).collect();
        self.journal.mark_handled(keys, now).await
    }

    /// Forget queued work whose epoch ended, settling its awaited ingress.
    async fn invalidate_key(&mut self, key: &str, now: i64) -> Result<(), String> {
        if let Some(m)=self.maintenance.remove(key) {
            m.cancel.store(true,std::sync::atomic::Ordering::SeqCst);
            m.task.abort();
        }
        self.queued_messages.remove(key);
        self.recovered.remove(key);
        self.closing_seen.remove(key);
        self.closing_misses.remove(key);
        self.drivers.remove(key);
        self.synced.remove(key);
        self.tokens.remove(key);
        self.held.remove(key);
        self.dsh_wait.remove(key);
        self.retries.remove(key);
        self.rollback_pending.remove(key);
        self.rearm_pending.remove(key);
        self.failures.remove(key);
        self.ready.retain(|(k, _)| k != key);
        self.suspended_claims.remove(key);
        if let Some(keys) = self.awaiting.remove(key) {
            self.journal.mark_handled(keys, now).await?;
        }
        Ok(())
    }

    fn drain_rearm_pending(&mut self, cfg: &Value) {
        let keys: Vec<String> = self.rearm_pending.keys().cloned().collect();
        for key in keys {
            let Some((at, previous)) = self.rearm_pending.get(&key).cloned() else {
                continue;
            };
            match rearm_dropped_provisional_sleep(&self.context, cfg, &key, at, &previous) {
                Ok(()) => {
                    self.rearm_pending.remove(&key);
                }
                Err(error) => {
                    println!("[reserved2] {key} provisional sleep rearm failed: {error}");
                }
            }
        }
    }

    /// Driver control events never become prompts. Our cancellation's minted epoch keeps
    /// ordinary queued ingress; an external rotation still invalidates ordinary work.
    /// A cancellation that fails is logged and never aborts the ingress or tick that routed it:
    /// a failed DSH stop still revoked the old epoch, so its minted token is adopted all the same.
    fn route_driver_event(&mut self, cfg: &Value, key: &str, event: session::WakeEvent) {
        if event.reason != "turnTimeout" {
            self.ready.push((key.to_string(), event.reason));
            return;
        }
        if self.active.get(key).is_some_and(Active::preserve_harness_cap) {
            // The worker refuses an expired admission or settles the running harness's cap.
            return;
        }
        let dispatch = self.active.get(key).map(|active| (active.token.as_str(), &active.minted));
        let cancelled = pipeline::Pipeline::cancel_reserved2_cap(&self.context, cfg, key, dispatch);
        match cancelled {
            Ok(cancelled) => {
                if let Some(token) = cancelled.token {
                    self.tokens.insert(key.to_string(), token);
                }
                if let Some(error) = cancelled.stop_error {
                    // DSH may never end this turn: the collector is aborted after a fixed headroom.
                    let abort_at = (crate::now_ms() as i64).saturating_add(crate::wake::COLLECTOR_HEADROOM_MS);
                    if let Some(active) = self.active.get_mut(key) {
                        active.cap_abort_at.get_or_insert(abort_at);
                    }
                    println!("[reserved2] {key} turn cap: stopping the DSH turn failed: {error}");
                }
            }
            Err(error) => println!("[reserved2] {key} turn cap cancellation failed: {error}"),
        }
    }

    /// Read state and the turn's publication together. Both token adoption and
    /// invalidation/admission must use this observation, even across repeated rotations.
    fn observe_turn_epoch(&mut self, key: &str) -> Result<(Value, bool), String> {
        let active = self.active.get(key).filter(|a| a.mode_stamp == self.mode_stamp);
        #[cfg(test)]
        if active.is_some_and(|a| a.minted.try_lock().is_err()) {
            if let Some(hook) = self.epoch_contended_hook.take() { let _ = hook.send(()); }
        }
        let minted = active.map(|a| a.minted.lock().unwrap_or_else(|p| p.into_inner()));
        let doc = self.state_doc()?;
        let current = doc
            .conversation(key)
            .and_then(|c| c.token().map(str::to_string))
            .unwrap_or_default();
        let valid = doc.reset_epoch() == self.reset_stamp && !current.is_empty();
        let published = minted.as_ref().is_some_and(|m| m.as_deref() == Some(current.as_str()));
        if valid && published {
            self.tokens.insert(key.to_string(), current.clone());
        }
        let in_turn = valid && (active.is_some_and(|a| a.token == current || published)
            || self.recovered.get(key).is_some_and(|(_, owned)| owned == &current));
        Ok((doc.into_value(), in_turn))
    }

    /// Queue a wake that cannot be dispatched while DSH is down. A full conversation drops its oldest wake
    /// (`dsh_health::QUEUE_MAX` cap); the messages themselves stay in the conversation state and are served by the wakes that remain.
    fn park_for_dsh(&mut self, key: &str, now: i64, reason: String) {
        if let Some((_, dropped)) = self.dsh_wait.push(key, now, reason) {
            println!("[reserved2] {key} 等待 DSH 的唤醒已满（{}），丢弃最早的一条（{dropped}）", crate::dsh_health::QUEUE_MAX);
            crate::recovery_log::record(
                &self.context,
                crate::run_record::Trigger::Turn,
                crate::run_record::Component::Queue,
                "dropped-oldest",
                crate::run_record::Outcome::Failed,
                &format!("{key}: queue of {} waiting wakes is full", crate::dsh_health::QUEUE_MAX),
            );
        }
    }

    fn taken(&self, key: &str) -> bool {
        self.live_all || self.groups.iter().any(|k| k == key)
    }

    /// Ticket 33: a Schedule store error is logged at most once a minute (the hub ticks every 500 ms).
    fn log_schedule_error(&mut self, what: &str, error: &crate::schedule::StoreError, now: i64) {
        if now.saturating_sub(self.schedule_log_at) >= 60_000 {
            self.schedule_log_at = now;
            println!("[reserved2] schedule {what}: {}", error.message());
        }
    }

    /// Ticket 33: apply the console's timer cancels. Runs before the timers are pumped, so a timer cancelled before it
    /// fell due never fires in the tick that takes its cancel. A cancel applies only to the timer the owner saw (same
    /// `timer_at`); one that fired or was re-armed meanwhile is left alone. A reach-out cancel skips that chance.
    fn apply_timer_cancels(&mut self, now: i64) -> bool {
        let cancels = match crate::schedule::ScheduleStore::new(&self.context).take_timer_cancels(now) {
            Ok(cancels) => cancels,
            Err(error) => {
                self.log_schedule_error("cancels", &error, now);
                return false;
            }
        };
        for cancel in cancels {
            let Some(d) = self.drivers.get_mut(&cancel.conversation) else { continue };
            match cancel.kind {
                crate::schedule::TimerKind::ReplyCheck => {
                    if d.sched.reply_check_at == Some(cancel.timer_at) {
                        d.sched.reply_check_at = None;
                    }
                }
                crate::schedule::TimerKind::ReachOut => d.skip_proactive(cancel.timer_at),
                crate::schedule::TimerKind::Sleep => {}
            }
        }
        true
    }

    /// Fire due timers, after applying the console's cancels (in that order). When the cancels cannot be read, the due
    /// reply checks and reach-outs wait for a tick whose cancels can be read, so a cancelled one never fires.
    fn pump_timers(&mut self, cfg: &Value, now: i64) {
        let cancels_ok = self.apply_timer_cancels(now);
        let deferred: Vec<(String, Option<i64>, Option<i64>)> = if cancels_ok {
            Vec::new()
        } else {
            self.drivers
                .iter_mut()
                .filter_map(|(key, d)| {
                    let reply = d.sched.reply_check_at.filter(|at| *at <= now);
                    let proactive = d.sched.proactive_at.filter(|at| *at <= now);
                    if reply.is_none() && proactive.is_none() {
                        return None;
                    }
                    if reply.is_some() {
                        d.sched.reply_check_at = None;
                    }
                    if proactive.is_some() {
                        d.sched.proactive_at = None;
                    }
                    Some((key.clone(), reply, proactive))
                })
                .collect()
        };
        for (key, e) in session::pump_all(&mut self.drivers, now) {
            self.route_driver_event(cfg, &key, e);
        }
        for (key, reply, proactive) in deferred {
            let Some(d) = self.drivers.get_mut(&key) else { continue };
            if reply.is_some() && d.sched.reply_check_at.is_none() {
                d.sched.reply_check_at = reply;
            }
            if proactive.is_some() && d.sched.proactive_at.is_none() {
                d.sched.proactive_at = proactive;
            }
        }
    }

    /// Ticket 33: publish this hub's in-memory timers for the Schedule page, then claim due reminders for idle
    /// conversations. A conversation is not idle while DSH is down (unless a CLI is pinned for it) or while a wake of it
    /// is queued, parked for DSH, held or waiting for a retry: its reminder stays due and runs once, late, when the
    /// conversation is free, so an outage never piles up a burst of missed repeats.
    fn sync_schedule(&mut self, cfg: &Value, suspended: bool, now: i64) {
        let store = crate::schedule::ScheduleStore::new(&self.context);
        let mut timers = Vec::new();
        for (key, d) in &self.drivers {
            if !d.sched.wc.infinite {
                if let Some(at) = d.sched.wc.sleep_until.as_deref().and_then(crate::wake::parse_iso_ms) {
                    timers.push(crate::schedule::TimerView { conversation: key.clone(), kind: crate::schedule::TimerKind::Sleep, at });
                }
            }
            if let Some(at) = d.sched.reply_check_at {
                timers.push(crate::schedule::TimerView { conversation: key.clone(), kind: crate::schedule::TimerKind::ReplyCheck, at });
            }
            if let Some(at) = d.sched.proactive_at {
                timers.push(crate::schedule::TimerView { conversation: key.clone(), kind: crate::schedule::TimerKind::ReachOut, at });
            }
        }
        if let Err(error) = store.publish_timers(timers, now) {
            self.log_schedule_error("timers", &error, now);
        }
        if suspended {
            return;
        }
        let dsh_down = crate::dsh_health::is_down(&self.context);
        let idle: std::collections::HashSet<String> = self
            .drivers
            .iter()
            .filter(|(key, d)| {
                let key = key.as_str();
                self.taken(key)
                    && social_state::inbound_admitted(cfg, key)
                    && (!dsh_down || !wake_needs_dsh(&self.context, cfg, key))
                    && !self.active.contains_key(key)
                    && !self.recovered.contains_key(key)
                    && !self.maintenance.contains_key(key)
                    && !d.sched.is_busy(now)
                    && !self.ready.iter().any(|(queued, _)| queued.as_str() == key)
                    && self.dsh_wait.len(key) == 0
                    && !self.held.contains_key(key)
                    && !self.retries.contains_key(key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let zone = crate::local_time::configured_zone(cfg);
        match store.claim_due(now, &zone, |key| idle.contains(key)) {
            Ok(items) => {
                for item in items {
                    println!("[reserved2] {} 日程提醒到期（{}）", item.conversation, item.id);
                    let reason = format!("{}{}", crate::schedule::REMINDER_REASON, item.id);
                    self.ready.push((item.conversation, reason));
                }
            }
            Err(error) => self.log_schedule_error("claim", &error, now),
        }
    }

    fn record(&self, name: &str, value: &Value) -> Result<(), String> {
        crate::append_runtime_json_line(&self.context, name, "", value).map_err(|e| e.to_string())
    }

    /// Cancel each running turn's own DSH session (captured when the turn learned it). A session
    /// that replaced it meanwhile (a new turn after a switch) is never touched; a turn that has no
    /// session yet cannot bind one across the change (`Pipeline::ensure_session`).
    fn cancel_running(&self, cfg: &Value, when: &str) {
        for (key, active) in &self.active {
            let Some(session) = active.session() else { continue };
            if let Err(error) = pipeline::Pipeline::new_with_runtime(&self.context, cfg, true).and_then(|mut p| p.cancel_session(key, &session)) {
                println!("[reserved2] {key} {when}取消在途回合失败：{error}");
            }
        }
    }

    async fn adopt_mode(&mut self, cfg: &Value, now: i64) -> Result<(), String> {
        let stamp = self
            .context
            .read_optional_text(RuntimeArea::Data, "mode.json")
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let reset = self.rotation_state(cfg, "guard-read", now)?["resetEpoch"].clone();
        if reset != self.reset_stamp {
            // The console already stopped DSH work, cancelled the ledger turns and removed every
            // token; nothing queued or held here survives. A running turn stays attached, as on a
            // mode change: it is cancelled, ends through the normal path (its token is gone, so
            // it is not retried and its ingress is settled then), and no second turn of its
            // conversation starts while it runs.
            self.reset_stamp = reset;
            self.drop_everything(now).await?;
            self.cancel_running(cfg, "重置后");

            self.record(
                "logs/rbridge-live.jsonl",
                &json!({"ts":now,"reason":"socialV2-reset","action":"invalidated"}),
            )?;
        }
        if stamp != self.mode_stamp {
            self.mode_stamp = stamp;
            self.drop_everything(now).await?;
            self.cancel_running(cfg, "模式切换后");
            self.record(
                "logs/rbridge-live.jsonl",
                &json!({"ts":now,"reason":"mode-changed","action":"invalidated"}),
            )?;
        }
        Ok(())
    }

    pub async fn handle(
        &mut self,
        ev: &crate::Event,
        raw: &str,
        replayed: bool,
        self_id: i64,
        nick: &str,
        cards: &HashMap<i64, String>,
    ) -> Result<(), String> {
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        self.handle_with_config(cfg, ev, raw, replayed, self_id, nick, cards).await
    }

    /// #192 (F3): `handle` with the configuration the caller read for this event: the main loop reads it once, holds
    /// the event while it cannot, and hands the same read here, so the gate and the handling never see two reads.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_with_config(
        &mut self,
        cfg: Value,
        ev: &crate::Event,
        raw: &str,
        replayed: bool,
        self_id: i64,
        nick: &str,
        cards: &HashMap<i64, String>,
    ) -> Result<(), String> {
        self.adopt_mode(&cfg, crate::now_ms() as i64).await?;
        self.refresh_recovered(&cfg, crate::now_ms() as i64).await?;
        let key = match (ev.post_type.as_str(), ev.message_type.as_str()) {
            ("message", "group") => ev.group_id.map(|id| format!("group:{id}")),
            ("message", "private") => ev.user_id.map(|id| format!("private:{id}")),
            ("notice", _) if ev.sub_type == "poke" => {
                ev.group_id.map(|id| format!("group:{id}")).or_else(|| {
                    ev.user_id
                        .or(ev.sender_id)
                        .or_else(|| ev.sender.as_ref().and_then(|s| s.user_id))
                        .map(|id| format!("private:{id}"))
                })
            }
            _ => None,
        };
        // #186: this event's configuration decides (allow and deny); `taken` is the `--live-groups` list of this process.
        let Some(key) = key.filter(|k| self.taken(k) && social_state::inbound_admitted(&cfg, k)) else {
            return Ok(());
        };
        let is_poke = ev.sub_type == "poke";
        let inbound_text::Rendered { text: rendered_text, plain, quote_self, at_self } = if is_poke {
            inbound_text::Rendered { text: String::new(), plain: String::new(), quote_self: false, at_self: false }
        } else {
            self.render(&cfg, &key, &ev.message, self_id)
        };
        let body = if is_poke && plain.is_empty() {
            ev.raw_message.clone()
        } else {
            plain.clone()
        };
        if let Some(m)=self.maintenance.get(&key) {m.cancel.store(true,std::sync::atomic::Ordering::SeqCst);}
        let now = crate::now_ms() as i64;
        let inbound_key = inbound::event_key(
            &key,
            &ev.post_type,
            crate::event_message_id(ev).as_deref(),
            raw,
        );
        if !replayed
            && self
                .journal
                .record_received(inbound_key.clone(), key.clone(), raw.to_string(), now)
                .await?
                == inbound::Receipt::Duplicate
        {
            return Ok(());
        }
        // Messages with no words of their own that do not quote the bot (e.g. quoting someone else or whitespace)
        // are settled without storage.
        if !is_poke && plain.is_empty() && !quote_self {
            self.journal.mark_handled(vec![inbound_key], now).await?;
            return Ok(());
        }
        // #170: the voice record of an opted-in conversation is read as its local transcript in place of the `[语音]`
        // label (`voice_stt`); without one the label stays. The transcript goes to the stored message and the model
        // only: the activity log (`preview`) keeps the label.
        let activity_text = rendered_text.clone();
        let (rendered_text, plain, body) = match crate::voice_stt::for_message(&self.context, &cfg, &key, &inbound_key, &ev.message).await {
            Some(label) => {
                let plain = crate::voice_stt::apply(&plain, &label);
                (crate::voice_stt::apply(&rendered_text, &label), plain.clone(), plain)
            }
            None => (rendered_text, plain, body),
        };
        let sender = ev
            .sender
            .as_ref()
            .map(|s| {
                if s.card.is_empty() {
                    &s.nickname
                } else {
                    &s.card
                }
            })
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                ev.user_id
                    .or(ev.sender_id)
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            });
        let text = if ev.sub_type == "poke" {
            format!(
                "[拍一拍] {sender} 拍了拍 {}",
                if ev.target_id == Some(self_id) {
                    "你".into()
                } else {
                    ev.target_id.map(|id| id.to_string()).unwrap_or_default()
                }
            )
        } else {
            rendered_text
        };
        let (stored_text, stored_plain, stored_tail) = if is_poke {
            let text = crate::social_mem::truncate_text(&text, 200);
            (text.clone(), text.clone(), text)
        } else {
            inbound_text::stored_texts(&text, &plain)
        };
        // The previews above are all most reads show; a longer message keeps its readable full text
        // for `qq_get_message_detail` (model views never carry it).
        let full = if is_poke { None } else { inbound_text::stored_full_text(&text, inbound_text::full_text_cap(&cfg)) };
        let (media, forward_ids) = crate::forward_fmt::collect_segment_meta(ev.message.as_array().map(Vec::as_slice).unwrap_or(&[]));
        // Private messages store sender as `'私聊'` (display names apply only to groups);
        // `record_reserved2_incoming` adds `ownerLabel` for the owner (`String(cfg.ownerQQ ?? '')`).
        let stored_sender = if !is_poke && key.starts_with("private:") { "私聊".to_string() } else { sender.clone() };
        let owner_qq = cfg.get("ownerQQ").filter(|v| !v.is_null()).map(crate::aux_text::js_string).unwrap_or_default();
        let is_owner = ev.user_id.map(|id| id.to_string()).is_some_and(|id| id == owner_qq);
        let owner_label = if is_owner { format!("管理员（ownerQQ {owner_qq}）") } else { String::new() };
        let neutral = !is_poke && inbound_text::quiet_neutral(&ev.message);
        // Bridge hint for the model, classified against the conversation before this message.
        // An unreadable state only loses the exchange evidence; the message is still recorded.
        let before = self.state().unwrap_or_else(|error| {
            println!("[reserved2] {key} 来信归类读取会话失败（按无交流记录处理）：{error}");
            Value::Null
        });
        let poke = is_poke.then(|| ev.target_id == Some(self_id));
        let for_you = crate::question::ingress_for_you(ev, &key, poke, quote_self, at_self,
            !is_poke && crate::names_self(ev, &body.to_lowercase(), nick, cards),
            &before["conversations"][&key], &cfg, now);
        let mut message = json!({"messageId":crate::event_message_id(ev), "sender":stored_sender, "userId":ev.user_id.or(ev.sender_id).map(|id| id.to_string()), "text":stored_text, "plain":stored_plain, "tail":stored_tail, "quoteTargetIsSelf":quote_self, "atSelf":at_self, "quietNeutral":neutral, "isSelf":false, "isOwner":is_owner, "ownerLabel":owner_label, "time":now, "hasMedia":!media.is_empty(), "media":media, "hasForward":!forward_ids.is_empty(), "forwardIds":forward_ids,
            "forYou":for_you.is_some(), "forYouReason":for_you.unwrap_or(crate::question::NOT_FOR_YOU)});
        if let Some((full_text, chars)) = full {
            message["fullText"] = json!(full_text);
            message["textChars"] = json!(chars);
            message["textTruncated"] = json!(true);
        }
        let seq =
            social_state::record_reserved2_incoming(&self.context, &cfg, &key, &inbound_key, message)?;
        if ev.post_type == "message" && ev.message_type == "group" && cfg.pointer("/slang/enabled") != Some(&json!(false)) {
            crate::feed_slang(&self.slang, true, &cfg, &key, &sender, &body);
        }
        let (state, in_turn) = self.observe_turn_epoch(&key)?;
        let conv = &state["conversations"][&key];
        let preview = crate::activity::slice(if is_poke { &text } else { &activity_text }, 80);
        // The activity line belongs to the event's first arrival: a journal replay after a restart
        // must not log it a second time.
        if is_poke && !replayed {
            crate::activity::append(&self.context, &format!("{key} 拍一拍事件：{preview}"));
        }
        let muted = crate::group_mute::active(&self.context, &key).is_some();
        if muted || state["paused"] == true || cfg.pointer("/socialV2/enabled") == Some(&json!(false)) {
            if !is_poke && !replayed {
                crate::activity::append(
                    &self.context,
                    &if muted {
                        format!("{key} [reserved2] 当前所在群聊正处于禁言阶段，消息已记入待阅档案而暂不唤起：{preview}")
                    } else if state["paused"] == true {
                        format!("{key} [reserved2] 当前会话的自动处理处于暂停状态，来信仅作留存记档而暂不唤起：{preview}")
                    } else {
                        format!("{key} [reserved2] 收到一则新的会话来信，已经顺手记入未读待办列表以便稍后处理：{preview}")
                    },
                );
            }
            self.journal.mark_handled(vec![inbound_key], now).await?;
            return Ok(());
        }
        let token = crate::conversation_state::Conversation::from_value(conv)
            .and_then(|c| c.token().map(str::to_string))
            .unwrap_or_default();
        if self.tokens.get(&key).is_some_and(|old| old != &token) {
            self.invalidate_key(&key, now).await?;
        }
        self.tokens.insert(key.clone(), token);
        self.closing_seen.entry(key.clone()).or_insert_with(|| conv["wakeConfig"]["confirmedAt"].clone());
        self.synced
            .entry(key.clone())
            .or_insert_with(|| session::WakeSync::of(&conv["wakeConfig"]));
        let mut tr: crate::Triggers = serde_json::from_value(conv["wakeConfig"]["triggers"].clone()).unwrap_or_default();
        // The persona's names wake her like keywords, except the ones she dropped (`wake_words.rs`).
        tr.keywords = crate::wake_words::effective(&self.context, &key, &tr.keywords, &tr.drop_names);
        // Do not even evaluate probability or question triggers for an active conversation.
        // Urgent candidates survive only if no later read covers them before completion.
        let (mut wake, mut reason) = in_turn_decision(in_turn, at_self, is_owner && ev.post_type == "message", &owner_qq)
            .unwrap_or_else(|| crate::decide(ev, &body, at_self || quote_self, nick, cards, &tr,
                crate::question::directed(ev, &body, quote_self, conv, &cfg, now)));
        if !in_turn && social_state::claim_first_wake(&self.context, &cfg, &key)? {
            wake = true;
            reason = "bootstrap".into();
        }
        if !in_turn && (is_owner || at_self) && !wake {
            wake = true;
            reason = if at_self {"atMention".into()} else {format!("speaker:{owner_qq}")};
        }
        let driver = self.drivers.entry(key.clone()).or_insert_with(|| {
            session::Driver::new(
                session::scheduler_from_state(crate::wake_cfg_of(&cfg), conv),
                crate::text_seed(&key, None),
            )
        });
        if let Some(next) = session::WakeSync::changed(self.synced.get(&key), &conv["wakeConfig"]) {
            driver.adopt_wake_config(session::wake_config_from_json(Some(&conv["wakeConfig"])), now);
            self.synced.insert(key.clone(), next);
        }
        driver.sched.last_incoming_at = now;
        driver.sched.prompt_running = self.active.contains_key(&key) || self.recovered.contains_key(&key) || self.maintenance.contains_key(&key);
        self.record(
            "logs/rbridge-live.jsonl",
            &json!({"ts":now,"key":key,"shouldWake":wake,"reason":reason,"shadow":false}),
        )?;
        if wake {
            self.retries.remove(&key);
            self.failures.remove(&key);
            self.queued_messages.entry(key.clone()).or_default().push((seq, inbound_key.clone()));
            self.awaiting
                .entry(key.clone())
                .or_default()
                .push(inbound_key);
        } else {
            self.journal.mark_handled(vec![inbound_key], now).await?;
        }
        let (events, preempted) = {
            let driver = self.drivers.get_mut(&key).unwrap();
            let events = driver.apply(session::Input::Incoming { reason: wake.then_some(reason), seq, at_ms: now, neutral, urgent: is_owner || at_self });
            (events, driver.preempted.take().is_some())
        };
        for event in events { self.route_driver_event(&cfg, &key, event); }
        let harness = self.active.get(&key).filter(|active| active.uses_harness());
        if let Some(active) = harness.filter(|_| preempted) {
            // Invalidate only this turn. Its runner observes the change and remains
            // attached until any started handler has durably settled.
            if let Err(error) = pipeline::revoke_reserved2_epoch(
                &self.context, &key, &active.admitted_token(), Some(&active.minted),
            ) {
                println!("[reserved2] {key} preemption revocation failed: {error}");
            }
        } else if preempted {
            let cancelled = pipeline::Pipeline::new_with_runtime(&self.context, &cfg, true)
                .and_then(|mut control| control.cancel_with_reason(&key, "superseded"));
            match cancelled {
                Ok(cancelled) => {
                    if let Some(token) = cancelled.token {
                        self.tokens.insert(key.clone(), token);
                    }
                    if let Some(error) = cancelled.stop_error {
                        println!("[reserved2] {key} preemption: stopping the DSH turn failed: {error}");
                    }
                }
                Err(error) => println!("[reserved2] {key} preemption cancellation failed: {error}"),
            }
        }

        if !is_poke && !replayed {
            crate::activity::append(&self.context, &format!("{key} [reserved2] 收到一则新的会话来信，已经顺手记入未读待办列表以便稍后处理：{preview}"));
        }
        Ok(())
    }

    async fn refresh_recovered(&mut self, cfg: &Value, now: i64) -> Result<(), String> {
        let doc = self.rotation_state_doc(cfg, "guard-read", now)?;
        // An unreadable ledger cannot prove that an attached turn ended.
        let Ok(records) = crate::turn_ledger::read_records(&self.context) else { return Ok(()) };
        let (turns, _) = crate::turn_ledger::fold(&records);
        let mut running = HashMap::new();
        if social_state::mode(&self.context) == "reserved2" && !doc.paused() {
            for turn in turns.iter().filter(|t| t.reattaching && t.state.is_open()) {
                let conv = doc.conversation(&turn.key);
                let token = conv.as_ref().and_then(|c| c.token()).unwrap_or("");
                if !token.is_empty() && social_state::inbound_admitted(cfg, &turn.key)
                    && turn.epochs.contains(&crate::send_outbox::token_epoch(token))
                    && crate::session_map::lookup(&self.context, &turn.key)?.as_deref() == Some(turn.session_id.as_str()) {
                    running.insert(turn.key.clone(), (turn.id.clone(), token.to_string()));
                }
            }
        }
        let ended: Vec<_> = self.recovered.iter().filter(|(key, owned)| running.get(*key) != Some(*owned))
            .map(|(key, owned)| (key.clone(), owned.clone())).collect();
        self.recovered = running;
        for (key, (turn_id, token)) in ended {
            if doc.conversation(&key).and_then(|c| c.token().map(str::to_string)).as_deref() == Some(token.as_str())
                && turns.iter().any(|t| t.id == turn_id && t.state == crate::turn_ledger::State::Completed) {
                self.settle_turn_coverage(cfg, &key, &token, &turn_id, now).await?;
            }
            if let Some(driver) = self.drivers.get_mut(&key) {
                driver.sched.prompt_running = false;
                let events = driver.apply(session::Input::TurnEnd { at_ms: now });
                for event in events.into_iter().filter(|e|e.reason != "turnTimeout") {
                    self.route_driver_event(cfg, &key, event);
                }
            }
        }
        Ok(())
    }

    /// Coverage can only remove work already owned by the base queue, after success.
    async fn settle_turn_coverage(&mut self, cfg: &Value, key: &str, token: &str, turn: &str, now: i64) -> Result<(), String> {
        if self.queued_messages.get(key).is_none_or(Vec::is_empty) { return Ok(()); }
        let (context, cfg, query_key, token, turn) =
            (self.context.clone(), cfg.clone(), key.to_string(), token.to_string(), turn.to_string());
        // Missing evidence (including console restart) preserves the base follow-up.
        let delivered = tokio::task::spawn_blocking(move ||
            crate::qq_tools::wake_delivered_urgent(&context, &cfg, &query_key, &token, &turn)
        ).await.map_err(|e|e.to_string())?.unwrap_or_default();
        let covered: Vec<_> = self.queued_messages.get(key).into_iter().flatten()
            .filter(|(seq, _)| delivered.contains(seq)).cloned().collect();
        if covered.is_empty() { return Ok(()); }
        let covered_keys: std::collections::HashSet<_> = covered.iter().map(|(_, k)| k.clone()).collect();
        self.journal.mark_handled(covered_keys.iter().cloned().collect(), now).await?;
        if let Some(items) = self.awaiting.get_mut(key) { items.retain(|k| !covered_keys.contains(k)); }
        if let Some(items) = self.queued_messages.get_mut(key) { items.retain(|(seq, _)| !delivered.contains(seq)); }
        if let Some(d) = self.drivers.get_mut(key) {
            d.sched.pending.retain(|item| crate::wake::is_timer_reason(&item.reason) || !delivered.contains(&item.seq));
        }
        if self.queued_messages.get(key).is_none_or(|items|items.is_empty()) {
            self.ready.retain(|(k,reason)| k != key || crate::wake::is_timer_reason(reason) || reason == "wakeReminder");
            if let Some(d) = self.drivers.get_mut(key) {
                if d.sched.pending_wake_reason.as_ref().is_some_and(|reason| !crate::wake::is_timer_reason(reason) && reason != "wakeReminder") {
                    d.sched.pending_wake_at = None;
                    d.sched.pending_wake_reason = None;
                }
            }
        }
        for (seq, _) in covered {
            self.record("logs/rbridge-live.jsonl", &json!({"ts":now,"key":key,"seq":seq,"shouldWake":false,"reason":"covered","shadow":false}))?;
        }
        Ok(())
    }

    async fn maintain_sessions(&mut self, cfg: &Value, suspended: bool, now: i64) -> Result<(),String> {
        use std::sync::atomic::Ordering;
        for (key,m) in &self.maintenance {
            let current=self.rotation_state(cfg,"guard-read",now)?["conversations"][key].clone();
            if suspended || !self.taken(key) || !social_state::inbound_admitted(cfg,key)
                || m.plan.policy != json!({"rotation":cfg.pointer("/dsh/rotation"),"cacheTtlMinutes":cfg.pointer("/dsh/cacheTtlMinutes")})
                || ["lastIncomingAt","lastUnreadSeq","adminWakes"].iter().any(|field|current[*field]!=m.plan.guard[*field])
                || self.ready.iter().any(|(k,_)|k==key) {
                m.cancel.store(true,Ordering::SeqCst);
            }
        }
        let done:Vec<_>=self.maintenance.iter().filter(|(_,m)|m.task.is_finished()).map(|(k,_)|k.clone()).collect();
        for key in done {
            // Keep the completed task owned and unconsumed if its retirement guard
            // cannot be read. The next tick retries it even while scans are throttled.
            let current=self.rotation_state(cfg,"retirement-read",now)?["conversations"][&key].clone();
            let mut m=self.maintenance.remove(&key).unwrap();
            let outcome=match m.task.await {Ok(o)=>o,Err(_)=>MaintenanceResult::Digest(crate::session_rotation::DigestResult::Pending)};
            match outcome {
                MaintenanceResult::Archived(result)=>{
                    crate::session_rotation::recovery_complete(&self.context,&key);
                    if result.is_err() {crate::session_rotation::failure(&self.context,cfg,&m.plan,"archive");}
                    else {crate::activity::append(&self.context,&format!("{} session rotation tokens={} digest={}",key,m.plan.input,if m.digested {"yes"} else {"no"}));}
                }
                MaintenanceResult::Digest(crate::session_rotation::DigestResult::Pending)=>{
                    let (context,cfg,plan,cancel)=(self.context.clone(),cfg.clone(),m.plan.clone(),m.cancel.clone());
                    let retry_ms=crate::session_rotation::read_retry_ms(&context,&key);
                    m.task=tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                        MaintenanceResult::Digest(crate::session_rotation::digest(context,cfg,plan,cancel).await)
                    });
                    self.maintenance.insert(key,m);
                }
                MaintenanceResult::Digest(crate::session_rotation::DigestResult::Done{ran,retire_unknown})=>{
                    let current_typed = crate::conversation_state::Conversation::from_value(&current);
                    if current_typed.as_ref().and_then(|c| c.token()).is_some_and(|token| token != m.plan.epoch) {
                        crate::session_rotation::recovery_complete(&self.context,&key);
                        continue;
                    }
                    if !retire_unknown && (m.cancel.load(Ordering::SeqCst) || suspended) {continue;}
                    // Unverifiable work must lose its old token before queued ingress can
                    // use a fresh session, even when the old DSH cancel route is broken.
                    let guard=if retire_unknown {current} else {m.plan.guard.clone()};
                    let guard_token = crate::conversation_state::Conversation::from_value(&guard)
                        .and_then(|c| c.token().map(str::to_string));
                    if retire_unknown && guard_token.as_deref().unwrap_or("") != m.plan.epoch.as_str() {continue;}
                    let retired=social_state::rotate_idle_session(&self.context,&key,&m.plan.sid,&guard);
                    if retired.is_err() || (retire_unknown && !retired.as_ref().is_ok_and(|token|token.is_some())) {
                        if retired.is_err() {crate::session_rotation::failure(&self.context,cfg,&m.plan,"reset");}
                        // A changed message guard or storage refusal is not permission to
                        // reuse an unverified session. Re-plan under its retained slot.
                        let (context,cfg,plan,cancel)=(self.context.clone(),cfg.clone(),m.plan.clone(),m.cancel.clone());
                        m.task=tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            MaintenanceResult::Digest(crate::session_rotation::digest(context,cfg,plan,cancel).await)
                        });
                        self.maintenance.insert(key,m);
                        continue;
                    }
                    match retired {
                        Ok(Some(token))=>{
                            self.tokens.insert(key.clone(),token);
                            let (context,cfg,key2,sid)=(self.context.clone(),cfg.clone(),key.clone(),m.plan.sid.clone());
                            m.digested=ran;
                            m.task=tokio::spawn(async move {
                                let result=tokio::task::spawn_blocking(move ||social_state::stop_and_archive_session(&context,&cfg,&key2,&sid)).await.unwrap_or_else(|e|Err(e.to_string()));
                                MaintenanceResult::Archived(result)
                            });
                            self.maintenance.insert(key,m);
                        }
                        Ok(None)=>{},
                        Err(_)=>{
                            let _=crate::session_map::update_context(&self.context,&key,&m.plan.sid,&m.plan.epoch,|r| {
                                r["rotation"]["phase"]=json!("done");
                                r["rotation"]["attemptedTurn"]=json!(m.plan.turn_seq);
                            });
                            crate::session_rotation::failure(&self.context,cfg,&m.plan,"reset");
                        }
                    }
                }
            }
        }
        if suspended {return Ok(());}
        let dsh_down=crate::dsh_health::is_down(&self.context);
        if now < self.maintenance_scan_at {return Ok(());}
        let state=self.rotation_state(cfg,"scan-state",now)?;
        let keys:Vec<_>=state["conversations"].as_object().map(|convs|convs.keys().filter(|key|self.taken(key) && social_state::inbound_admitted(cfg,key)).cloned().collect()).unwrap_or_default();
        for key in keys {
            if self.active.contains_key(&key) || self.recovered.contains_key(&key) || self.maintenance.contains_key(&key) {continue;}
            if crate::session_rotation::read_retry_at(&self.context,&key,"recovery-scan") > now {
                return Err("session rotation read retry deferred".into());
            }
            let candidate=crate::session_rotation::candidate(&self.context,cfg,&key,now);
            let Some(plan)=crate::session_rotation::rotation_read(&self.context,cfg,&key,"recovery-scan",now,candidate)? else {continue;};
            if !plan.recovering && (dsh_down
                || crate::turn_engine::Engine::read_choice(&self.context, cfg, &key, "reserved2").is_ok_and(|engine| !engine.uses_dsh())
                || self.awaiting.get(&key).is_some_and(|v|!v.is_empty())
                || self.ready.iter().any(|(k,_)|k==&key)
                || self.retries.contains_key(&key) || self.held.contains_key(&key)
                || pipeline::pinned_cli_backend(cfg,&key).is_some()) {continue;}
            let cancel=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (context,cfg2,plan2,cancel2)=(self.context.clone(),cfg.clone(),plan.clone(),cancel.clone());
            let task=tokio::spawn(async move {MaintenanceResult::Digest(crate::session_rotation::digest(context,cfg2,plan2,cancel2).await)});
            self.maintenance.insert(key,Maintenance{plan,cancel,task,digested:false});
        }
        // A failed recovery read must be retried before the next tick can dispatch.
        self.maintenance_scan_at=now+5000;
        Ok(())
    }

    pub async fn tick(&mut self, nick: &str, self_id: i64) -> Result<(), String> {
        let now = crate::now_ms() as i64;
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        // No scheduler error spam or dispatch while a failed recovery read is cooling down.
        if self.maintenance.keys().chain(self.drivers.keys()).any(|key|
            ["guard-read", "retirement-read", "scan-state", "recovery-scan"].iter()
                .any(|step| crate::session_rotation::read_retry_at(&self.context,key,step) > now)) {
            return Ok(());
        }
        self.adopt_mode(&cfg, now).await?;
        self.refresh_recovered(&cfg, now).await?;
        let state = self.rotation_state(&cfg,"guard-read",now)?;
        let suspended = social_state::mode(&self.context) != "reserved2"
            || state["paused"] == true
            || cfg.pointer("/socialV2/enabled") == Some(&json!(false));
        self.maintain_sessions(&cfg,suspended,now).await?;
        self.drain_rearm_pending(&cfg);
        let mut state=self.state()?;
        let suspended = suspended || state["paused"] == true;
        #[cfg(test)]
        if let Some(hook) = self.tick_snapshot_hook.take() {
            let (done, finished) = tokio::sync::oneshot::channel();
            let _ = hook.send(done);
            let _ = finished.await;
        }
        let keys: Vec<String> = self.drivers.keys().cloned().collect();
        let mut invalid = Vec::new();
        for key in keys {
            // Never compare a newly published token against tick's earlier snapshot.
            // A second owned rotation can finish before this lock is acquired.
            (state, _) = self.observe_turn_epoch(&key)?;
            let doc = crate::conversation_state::SocialDocument::from_value(state.clone())
                .map_err(|(_, e)| e)?;
            let predicate = if !self.taken(&key) { "notTaken" }
                else if !social_state::inbound_admitted(&cfg, &key) { "notAllowed" }
                else if !doc.has_conversation(key.as_str()) { "missing" }
                else if self.tokens.get(&key).is_some_and(|old| {
                    old.as_str() != doc.conversation(&key).and_then(|c| c.token().map(str::to_string)).unwrap_or_default()
                }) { "tokenChanged" }
                // Only a batch already refused by the claim can survive a suspended tick.
                else if suspended && !self.suspended_claims.contains(&key) { "suspended" }
                else { continue; };
            invalid.push((key, predicate));
        }
        for (key, predicate) in invalid {
            self.invalidate_key(&key, now).await?;
            self.record(
                "logs/rbridge-live.jsonl",
                &json!({"ts":now,"key":key,"reason":"settings-invalidated","predicate":predicate}),
            )?;
        }
        if social_state::mode(&self.context) == "reserved2"
            && state["paused"] != true
            && cfg.pointer("/socialV2/enabled") != Some(&json!(false))
        {
            if let Some(conversations) = state["conversations"].as_object() {
                for (key, conv) in conversations {
                    if self.taken(key) && social_state::inbound_admitted(&cfg, key) {
                        self.closing_seen.entry(key.clone()).or_insert_with(|| conv["wakeConfig"]["confirmedAt"].clone());
                        self.drivers.entry(key.clone()).or_insert_with(|| {
                            session::Driver::new(
                                session::scheduler_from_state(crate::wake_cfg_of(&cfg), conv),
                                crate::text_seed(key, None),
                            )
                        });
                    }
                }
            }
        }
        crate::sync_wake_configs(
            &self.context,
            &cfg,
            &state,
            &mut self.drivers,
            &mut self.synced,
            now,
        );
        for (key, d) in self.drivers.iter_mut() {
            let conv = &state["conversations"][key];
            let persisted_wake = session::wake_config_from_json(Some(&conv["wakeConfig"]));
            // Recovery may restore a consumed lease after this driver was initialized. This is
            // not a tool configuration change, so confirmedAt alone cannot detect it. Local
            // retries/holds already own their deadline and must retain their bounded backoff.
            if !self.active.contains_key(key) && !self.held.contains_key(key)
                && !self.failures.contains_key(key) && !persisted_wake.infinite
                && persisted_wake.sleep_until.is_some()
                && persisted_wake.sleep_until != d.sched.wc.sleep_until {
                d.adopt_wake_config(persisted_wake, now);
            }
            let mut next_cfg = crate::wake_cfg_of(&cfg);
            if self.active.contains_key(key) {
                next_cfg.turn_hard_cap_ms = d.sched.cfg.turn_hard_cap_ms;
                next_cfg.turn_ceiling_ms = d.sched.cfg.turn_ceiling_ms;
            }
            if d.last_rate_limited.is_some()
                && (d.sched.cfg.max_wake_per_minute != next_cfg.max_wake_per_minute
                    || d.sched.cfg.max_wake_per_hour != next_cfg.max_wake_per_hour)
            {
                d.sched.pending_wake_at = Some(now);
            }
            if d.sched.cfg.proactive_enabled != next_cfg.proactive_enabled
                || d.sched.cfg.proactive_min_ms != next_cfg.proactive_min_ms
                || d.sched.cfg.proactive_max_ms != next_cfg.proactive_max_ms
            {
                d.sched.proactive_at = None;
            }
            d.sched.cfg = next_cfg;
            d.refresh_observed_state(conv, now);
            self.tokens
                .entry(key.clone())
                .or_insert_with(|| {
                    crate::conversation_state::Conversation::from_value(conv)
                        .and_then(|c| c.token().map(str::to_string))
                        .unwrap_or_default()
                });
            d.arm_proactive_now(now);
            d.sched.prompt_running = self.active.contains_key(key) || self.recovered.contains_key(key) || self.maintenance.contains_key(key);
        }
        #[cfg(test)]
        if let Some(finished) = self.collector_finished_hook.take() { let _ = finished.await; }
        for (key, active) in self.active.iter().filter(|(_, a)| a.cap_abort_at.is_some_and(|at| now >= at)) {
            if !active.task.is_finished() {
                println!("[reserved2] {key} turn cap: DSH did not confirm the stop; aborting the collector");
                active.task.abort();
            }
        }
        let ended: Vec<String> = self
            .active
            .iter()
            .filter(|(_, a)| a.task.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in ended {
            // Observe publication again before releasing the wake or replaying queued work.
            let (state, owned) = self.observe_turn_epoch(&key)?;
            let active = &self.active[&key];
            if owned && active.route() == pipeline::Reserved2Route::ExpiredBeforeAdmission
                && active.reason != "wakeReminder" && active.previous["wakeConfig"]["sleepUntil"].is_string()
            {
                // A wake refused at the gate never got to consume its durable timer claim.
                let token = crate::conversation_state::SocialDocument::from_value(state.clone())
                    .ok()
                    .and_then(|doc| doc.conversation(&key))
                    .and_then(|c| c.token().map(str::to_string))
                    .unwrap_or_default();
                social_state::consume_reserved2_deadline(&self.context, &key, &token, active.at, &active.previous)?;
            }
            let active = self.active.remove(&key).unwrap();
            let admitted_token = active.admitted_token();
            let route = active.route();
            let uses_cli = route == pipeline::Reserved2Route::Cli;
            let DispatchResult { result, unsubmitted, failure_parks } = match active.task.await {
                Ok(ended) => ended,
                // Our own abort after a failed cap stop: the cap cancellation already settled the turn.
                Err(error) if error.is_cancelled() && active.cap_abort_at.is_some() =>
                    DispatchResult::new(Err("回合已取消：turnTimeout（DSH 未确认停止，已中止收集）".to_string()), false),
                Err(error) => return Err(error.to_string()),
            };
            println!("[reserved2] {key} turn ended: {result:?}");
            // The pipeline already wrote the turn-error line and told the administrator when the turn ended in a model error.
            let model_error = pipeline::take_model_error(&self.context, &key);
            let current_token = social_state::token(&self.context, &key).ok().flatten().unwrap_or_default();
            let in_scope = social_state::mode(&self.context) == "reserved2"
                && social_state::inbound_admitted(&cfg, &key)
                && self.mode_stamp == active.mode_stamp;
            let still_valid = in_scope && current_token == admitted_token;
            let owned_token = current_token == admitted_token
                || active.minted.lock().unwrap_or_else(|p|p.into_inner()).as_deref() == Some(current_token.as_str());
            let completed_in_scope = result.is_ok() && in_scope && owned_token;
            let clean_sleep = result.is_ok() && crate::turn_ledger::read_records(&self.context).ok().is_some_and(|records| {
                let epoch = crate::send_outbox::token_epoch(&admitted_token);
                crate::turn_ledger::fold(&records).0.iter().any(|t|t.key == key && t.begun_at >= active.at
                    && t.epochs.contains(&epoch) && t.state == crate::turn_ledger::State::Completed && t.outcome_reason == "observationSleep")
            });
            if clean_sleep {
                if completed_in_scope {
                    social_state::rearm_observation_sleep(&self.context, &cfg, &key, &current_token, now)?;
                }
                println!("[reserved2] observation budget ended; keeping wake config");
            }
            if completed_in_scope && !uses_cli {
                if let Ok(records) = crate::turn_ledger::read_records(&self.context) {
                    let (turns, _) = crate::turn_ledger::fold(&records);
                    let epoch = crate::send_outbox::token_epoch(&admitted_token);
                    if let Some(turn) = turns.iter().rev().find(|t| t.key == key && t.begun_at >= active.at
                        && t.epochs.contains(&epoch) && t.state == crate::turn_ledger::State::Completed) {
                        self.settle_turn_coverage(&cfg, &key, &current_token, &turn.id, now).await?;
                    }
                }
            }
            let held = still_valid
                && result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.starts_with(pipeline::HELD_ERROR_PREFIX));
            let access_refused = result.as_ref().is_err_and(|error| pipeline::is_dsh_access_refusal(error));
            if still_valid && !held && !access_refused {
                if let (Err(message), None) = (&result, &model_error) {
                    let shown = if crate::pipeline::configured_outbound_violation(&cfg, message, &[]).is_some() {
                        "（涉及敏感内容，已遮蔽）".to_string()
                    } else {
                        crate::activity::slice(message, 500)
                    };
                    crate::activity::append(&self.context, &format!("{key} agent 处理出错：{}", crate::activity::slice(&shown, 80)));
                }
            }
            let retry = still_valid && result.is_err() && unsubmitted && !held && !access_refused
                && route != pipeline::Reserved2Route::ExpiredBeforeAdmission;
            // The prompt never reached DSH and DSH is down: wait for it (bounded) instead of burning the retries.
            let wait_for_dsh = retry && !uses_cli && failure_parks.unwrap_or_else(|| {
                crate::dsh_health::is_down(&self.context) && wake_needs_dsh(&self.context, &cfg, &key)
            });
            let is_reminder = active.reason == "wakeReminder";
            let had_inbound = !active.inbound.is_empty();
            let retry_exhausted = retry
                && !wait_for_dsh
                && self.failures.get(&key).copied().unwrap_or(0) + 1 > MAX_WAKE_RETRIES;
            let dropped_unadmitted = (route == pipeline::Reserved2Route::ExpiredBeforeAdmission && !held)
                || retry_exhausted
                || (held && had_inbound);
            let needs_provisional_rearm = still_valid
                && !access_refused
                && !is_reminder
                && dropped_unadmitted
                && active.previous["wakeConfig"]["infinite"] == false
                && active.previous["wakeConfig"]["sleepUntil"].is_null();
            if needs_provisional_rearm {
                self.rearm_pending
                    .insert(key.clone(), (active.at, active.previous.clone()));
            }
            if held || retry {
                social_state::rollback_reserved2_wake(&self.context, &key, active.at, &active.previous)?;
                if held && !had_inbound {
                    self.held.insert(key.clone(), active.reason.clone());
                }
            }
            self.journal
                .mark_handled(active.inbound.clone(), now)
                .await?;
            if wait_for_dsh {
                println!("[reserved2] {key} 引擎眼下离线，本轮唤起暂列队等候");
                let keys = active.inbound;
                self.awaiting.entry(key.clone()).or_default().extend(keys.clone());
                self.journal.run(move |j| j.settle_events(&keys, "received", now)).await?;
                self.park_for_dsh(&key, now, active.reason);
            } else if retry {
                let failures = self.failures.entry(key.clone()).or_default();
                *failures += 1;
                if *failures <= MAX_WAKE_RETRIES {
                    self.retries.insert(
                        key.clone(),
                        (now + 5_000 * (1_i64 << (*failures - 1)), active.reason),
                    );
                    let keys = active.inbound;
                    self.awaiting
                        .entry(key.clone())
                        .or_default()
                        .extend(keys.clone());
                    self.journal
                        .run(move |j| j.settle_events(&keys, "received", now))
                        .await?;
                }
            } else if result.is_ok() || (model_error.is_some() && !held) {
                // Run the wake-reminder check at every turn end, a model error included.
                if result.is_ok() {
                    self.failures.remove(&key);
                }
                if (still_valid || completed_in_scope) && !clean_sleep {
                    let records = crate::turn_ledger::read_records(&self.context)?;
                    let started = crate::turn_ledger::fold(&records).0.into_iter().rev()
                        .find(|turn| turn.key == key && turn.begun_at >= active.at
                            && turn.epochs.contains(&crate::send_outbox::token_epoch(&admitted_token)))
                        .and_then(|turn| records.iter().rev().find(|r| r["turn"] == turn.id && r["k"] == "started"))
                        .and_then(|r| r["t"].as_i64());
                    let miss = self.closing_misses.get(&key).copied().unwrap_or(0) + 1;
                    let seen = self.closing_seen.get(&key).cloned().unwrap_or(Value::Null);
                    let (remind, stamp) = social_state::complete_reserved2_turn(&self.context, &cfg, &key, &current_token, started, &seen, miss)?;
                    self.closing_seen.insert(key.clone(), stamp);
                    // A failed text-only CLI cannot configure its wake through MCP. Its failed
                    // ledger/input is settled: a closing reminder must not replay that prompt.
                    if remind && !(result.is_err() && uses_cli) {
                        self.closing_misses.insert(key.clone(), miss);
                        self.ready.push((key.clone(), "wakeReminder".into()));
                    } else {
                        self.closing_misses.remove(&key);
                    }
                }
            }
            self.drain_rearm_pending(&cfg);
            let completed_state = self.state()?;
            if clean_sleep {
                self.closing_seen.insert(key.clone(), completed_state["conversations"][&key]["wakeConfig"]["confirmedAt"].clone());
                self.closing_misses.remove(&key);
            }
            if let Some(d) = self.drivers.get_mut(&key) {
                let conv = &completed_state["conversations"][&key];
                if let Some(next) = session::WakeSync::changed(self.synced.get(&key), &conv["wakeConfig"]) {
                    d.adopt_wake_config(session::wake_config_from_json(Some(&conv["wakeConfig"])), now);
                    self.synced.insert(key.clone(), next);
                }
                d.refresh_observed_state(conv, now);
                d.sched.prompt_running = false;
                if held || retry {
                    d.sched.turn_started_at = None;
                    d.sched.pending_wake_key_at = None;
                    d.sched.wake_times.retain(|at| *at != active.at);
                    continue;
                }
                // The turn already ended: a cap that passed since the last tick must not cancel it.
                let events: Vec<_> = d.apply(session::Input::TurnEnd { at_ms: now })
                    .into_iter().filter(|e| e.reason != "turnTimeout").collect();
                for e in events {
                    self.route_driver_event(&cfg, &key, e);
                }
            }
        }
        if !self.dsh_wait.is_empty() {
            let dsh_down = crate::dsh_health::is_down(&self.context);
            // A new explicit CLI choice also releases that conversation while DSH stays down.
            // Every released wake still passes the normal mode, access, pause and epoch checks.
            let runnable: std::collections::HashSet<_> = self.dsh_wait.per_key.keys()
                .filter(|key| !dsh_down || !wake_needs_dsh(&self.context, &cfg, key)).cloned().collect();
            for (key, reason) in self.dsh_wait.drain_ready(|key| runnable.contains(key)) {
                println!("[reserved2] {key} 重新投递等待中的唤醒（先核对权限与会话状态）");
                self.ready.push((key, reason));
            }
        }
        if !self.held.is_empty() {
            let (turns, _) =
                crate::turn_ledger::fold(&crate::turn_ledger::read_records(&self.context)?);
            let released: Vec<String> = self
                .held
                .keys()
                .filter(|k| crate::turn_ledger::blocking_turn(&turns, k).is_none())
                .cloned()
                .collect();
            for key in released {
                self.ready
                    .push((key.clone(), self.held.remove(&key).unwrap()));
            }
        }
        let retry_due: Vec<String> = self
            .retries
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in retry_due {
            self.ready
                .push((key.clone(), self.retries.remove(&key).unwrap().1));
        }
        // Manual wakes from the console (`POST /api/socialV2/wake`): the pending batch wake is
        // replaced, a busy conversation buffers the reason, and a free one wakes now (rate limits still apply).
        // One request per conversation per tick.
        let requested: Vec<String> = state["conversations"]
            .as_object()
            .map(|c| c.iter().filter(|(_, v)| v["adminWakes"].as_array().is_some_and(|a| !a.is_empty())).map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        for key in requested {
            if !self.taken(&key) {
                continue;
            }
            let Some(reason) = social_state::take_admin_wake(&self.context, &key)? else {
                println!("[reserved2] {key} 手动唤醒已失效（令牌代际已变），丢弃");
                continue;
            };
            if suspended || !social_state::inbound_admitted(&cfg, &key) || !self.drivers.contains_key(&key) {
                println!("[reserved2] {key} 手动唤醒未执行：当前不允许唤醒（{reason}）");
                continue;
            }
            let running = self.active.contains_key(&key) || self.recovered.contains_key(&key) || self.maintenance.contains_key(&key);
            let d = self.drivers.get_mut(&key).expect("checked above");
            d.sched.pending_wake_at = None;
            d.sched.pending_wake_reason = None;
            d.sched.prompt_running = running;
            if let crate::wake::Outcome::Armed = d.sched.schedule_wake(&reason, now) {
                d.sched.pending_wake_at = Some(now);
            }
            println!("[reserved2] 收到后台操作指令，主动唤醒会话 {key}，原因说明：{reason}");
        }
        if !suspended {
            self.pump_timers(&cfg, now);
        }
        self.sync_schedule(&cfg, suspended, now);
        if suspended {
            self.ready.retain(|(key, _)| self.suspended_claims.contains(key));
            return Ok(());
        }
        for (key, reason) in std::mem::take(&mut self.ready) {
            if let Some(m)=self.maintenance.get(&key) {
                m.cancel.store(true,std::sync::atomic::Ordering::SeqCst);
                self.ready.push((key,reason));
                continue;
            }
            if self.active.contains_key(&key) || self.recovered.contains_key(&key) {
                if reason == "wakeReminder" {
                    self.ready.push((key, reason));
                } else if let Some(d) = self.drivers.get_mut(&key) {
                    d.sched.prompt_running = true;
                    d.sched.schedule_wake(&reason, now);
                }
                continue;
            }
            self.suspended_claims.remove(&key);
            if !self.taken(&key) || !social_state::inbound_admitted(&cfg, &key) {
                continue;
            }
            if crate::group_mute::active(&self.context, &key).is_some() {
                // #46: no wake while muted; its messages stay unread for the first wake after the mute.
                println!("{key} [reserved2] 该群当前依然处在管理员禁言期间之内，所以这次唤醒调度已自动跳过（触发原因为 {reason}）");
                continue;
            }
            if crate::dsh_health::is_down(&self.context) && wake_needs_dsh(&self.context, &cfg, &key) {
                // DSH-dependent work waits without a call. A pinned CLI is an independent backend
                // and must not be prevented from handling its conversation by a DSH outage.
                println!("[reserved2] {key} 引擎眼下离线，本轮唤起暂列队等候");
                self.park_for_dsh(&key, now, reason);
                continue;
            }
            let mut keys = Vec::new();
            let mut claimed = None;
            // Keep each drained job until a task or a bounded retry owns it.
            let prepared = async {
                if let Some((at, previous)) = self.rollback_pending.get(&key) {
                    social_state::rollback_reserved2_wake(&self.context, &key, *at, previous)?;
                    self.rollback_pending.remove(&key);
                }
                // Imported conversations can predate agent tokens. Mint their first epoch
                // before claiming a wake; a previously nonempty epoch still invalidates normally.
                if self.tokens.get(&key).is_some_and(String::is_empty) {
                    let token = social_state::ensure_or_rotate(&self.context, &key, false)?;
                    self.tokens.insert(key.clone(), token);
                }
                if reason != "wakeReminder" {
                    keys = self.awaiting.get(&key).cloned().unwrap_or_default();
                    self.journal.mark_processing(keys.clone(), now).await?;
                    self.awaiting.remove(&key);
                }
                // The token the queued work was seen under: a wake never takes a token minted after it
                // was queued (revoke and re-grant while it waited).
                let expected = self.tokens.get(&key).filter(|t| !t.is_empty()).cloned();
                let (previous, dispatch_token) = if reason == "wakeReminder" {
                    let conv_typed = self.state_doc()?.conversation(&key);
                    let token = conv_typed
                        .as_ref()
                        .and_then(|c| c.token())
                        .unwrap_or("")
                        .to_string();
                    let conv = conv_typed
                        .map(|c| c.to_value())
                        .unwrap_or(Value::Null);
                    if expected.as_ref().is_some_and(|t| *t != token) {
                        return Err(STALE_WAKE.to_string());
                    }
                    (conv, token)
                } else {
                    let claim = social_state::claim_reserved2_wake(&self.context, &key, &reason, now, expected.as_deref())?;
                    claimed = Some(claim.0.clone());
                    claim
                };
                let prompt = if reason == "wakeReminder" {
                    prompt::build_reminder_with_runtime(&self.context, &key, nick, &self_id.to_string())
                } else {
                    prompt::build_with_runtime(&self.context, &key, &reason, nick, &self_id.to_string())
                };
                let cli_prompt = prompt::build_text_only_for_wake(&self.context, &key, &reason, nick, &self_id.to_string());
                let target = crate::parse_target(&key, &cfg).map_err(|e| e.to_string())?;
                self.record("logs/rbridge-live-wakes.jsonl", &json!({"ts":now,"key":key,"reason":reason,"shadow":false}))?;
                Ok::<_, String>((previous, dispatch_token, prompt, cli_prompt, target))
            }.await;
            let (previous, dispatch_token, prompt, cli_prompt, target) = match prepared {
                Ok(prepared) => prepared,
                Err(error) if error == STALE_WAKE || error == social_state::WAKE_ACCESS_REFUSED => {
                    // Authority changed while the wake was queued. Settle its ingress without a retry.
                    self.invalidate_key(&key, now).await?;
                    self.journal.mark_handled(keys, now).await?;
                    let reason = if error == STALE_WAKE { "token-invalidated" } else { "settings-invalidated" };
                    self.record("logs/rbridge-live.jsonl", &json!({"ts":now,"key":key,"reason":reason}))?;
                    continue;
                }
                Err(error) => {
                    println!("[reserved2] {key} dispatch preparation failed: {error}");
                    if let Some(previous) = claimed.as_ref() {
                        if let Err(error) = social_state::rollback_reserved2_wake(&self.context, &key, now, previous) {
                            println!("[reserved2] {key} dispatch rollback failed: {error}");
                            self.rollback_pending.insert(key.clone(), (now, previous.clone()));
                        }
                    }
                    // A failed journal transition may have left the original entry in place.
                    if !keys.is_empty() { self.awaiting.insert(key.clone(), keys.clone()); }
                    if let Err(error) = self.journal.run(move |j| j.requeue_unsubmitted(&keys, now)).await {
                        println!("[reserved2] {key} dispatch journal rollback failed: {error}");
                    }
                    if let Some(d) = self.drivers.get_mut(&key) {
                        d.sched.prompt_running = false;
                        d.sched.turn_started_at = None;
                        d.sched.pending_wake_key_at = None;
                        d.sched.wake_times.retain(|at| *at != now);
                    }
                    if error == social_state::WAKE_SUSPENDED {
                        self.suspended_claims.insert(key.clone());
                        self.ready.push((key, reason));
                        continue;
                    }
                    let failures = self.failures.entry(key.clone()).or_default();
                    *failures += 1;
                    if *failures <= MAX_WAKE_RETRIES {
                        self.retries.insert(key, (now + 5_000 * (1_i64 << (*failures - 1)), reason));
                    } else if let Some(previous) = claimed.as_ref() {
                        if previous["wakeConfig"]["infinite"] == false
                            && previous["wakeConfig"]["sleepUntil"].is_null()
                        {
                            self.rearm_pending.insert(key, (now, previous.clone()));
                            self.drain_rearm_pending(&cfg);
                        }
                    }
                    continue;
                }
            };
            // Reminders do not consume awaiting messages or grant a wake snapshot.
            if reason != "wakeReminder" { self.queued_messages.remove(&key); }
            let (context, cfg2, reason2, keys2) = (self.context.clone(), cfg.clone(), reason.clone(), keys.clone());
            println!("[her] 定时唤醒 {key}（{reason}）：执行 live 回合");
            let hard_cap_ms = crate::wake::turn_budget(&cfg).hard_cap_ms;
            let d = self.drivers.get_mut(&key).unwrap();
            d.sched.prompt_running = true;
            d.sched.cfg.turn_hard_cap_ms = hard_cap_ms;
            d.sched.cfg.turn_ceiling_ms = crate::wake::turn_ceiling_ms(&cfg);
            d.sched.turn_started_at = Some(now);
            d.sched.pending_wake_key_at = None;
            if reason != "wakeReminder" {
                d.sched.wc.sleep_until = None;
                d.sched.sleep_at = None;
            }
            let token = dispatch_token.clone();
            let wake_claim = json!({"wakeConfig":previous["wakeConfig"],"lastWakeReason":previous["lastWakeReason"]});
            let session = std::sync::Arc::new(std::sync::Mutex::new(None));
            let shared = session.clone();
            let minted = std::sync::Arc::new(std::sync::Mutex::new(None));
            let minted_shared = minted.clone();
            let queued_route = match crate::turn_engine::Engine::selected(&context, &cfg2, &key, "reserved2") {
                Ok(crate::turn_engine::Engine::Harness) => pipeline::Reserved2Route::PendingHarness,
                _ if pipeline::pinned_cli_backend(&cfg2, &key).is_some() => pipeline::Reserved2Route::Cli,
                _ => pipeline::Reserved2Route::Dsh,
            };
            let route = std::sync::Arc::new(std::sync::Mutex::new(queued_route));
            let route_shared = route.clone();
            let admitted = pipeline::MintedEpoch::default();
            let admitted_shared = admitted.clone();
            let retries_remaining = self.failures.get(&key).copied().unwrap_or_default() < MAX_WAKE_RETRIES;
            let task = tokio::spawn(async move {
                match pipeline::Pipeline::new_without_dsh(&context, &cfg2, true) {
                    Err(error) => DispatchResult::new(Err(error), true),
                    Ok(mut pipe) => {
                        pipe.share_session(shared);
                        pipe.share_minted_epoch(minted_shared);
                        pipe.share_reserved2_route(route_shared, admitted_shared);
                        pipe.set_reserved2_retry_budget(retries_remaining);
                        pipe.note_turn(&reason2, &keys2);
                        pipe.expect_dispatch_token(dispatch_token);
                        pipe.expect_dispatch_deadline(now.saturating_add(hard_cap_ms));
                        if reason2 != "wakeReminder" { pipe.expect_dispatch_wake(now, wake_claim); }
                        let result = pipe.run_turn(&target, &prompt, &cli_prompt, &cfg2).await;
                        DispatchResult {
                            result,
                            unsubmitted: pipe.dispatch_definitely_unsubmitted(),
                            failure_parks: pipe.reserved2_failure_parks(),
                        }
                    }
                }
            });
            self.active.insert(
                key,
                Active {
                    task,
                    inbound: keys,
                    reason,
                    previous,
                    at: now,
                    token,
                    mode_stamp: self.mode_stamp.clone(),
                    session,
                    minted,
                    cap_abort_at: None,
                    route,
                    admitted,
                },
            );
        }
        Ok(())
    }
}
