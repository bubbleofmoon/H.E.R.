//! Ticket 27: the Rust watchdog (replaces `memory/gray-watchdog.mjs`).
//!
//! One tick = observe, [`decide`], [`apply`]. The decision is a pure function of what was observed, so every branch of
//! the frozen Node watchdog's table is tested here without processes. Rules kept from the Node watchdog: an explicit,
//! valid configuration is required before anything is started; the console is started only when its port is free and
//! never stopped when another program holds it; two consecutive bad heartbeats are needed before a hung live owner is
//! stopped; a second live instance is never started while the first is not proven gone. New rules: the persistent
//! desired state (`lifecycle::effective`) decides what may run, so a stop or rollback is never undone here; the live
//! owner is the holder of the OS owner lock (never a lock age or a deleted file); an owner is stopped only after its
//! recorded identity is re-verified against the OS; and every unknown observation (unreadable desired state, lock,
//! marker, process or heartbeat) leaves everything as it is and reports it.
#![allow(dead_code)]

use crate::lifecycle::{Effective, KillOutcome, Liveness, OwnerMarker, OwnerProbe, ProcessIdentity};
use serde_json::Value;

/// A heartbeat older than this is stale (Node watchdog: 90 s).
pub const HEARTBEAT_FRESH_MS: i64 = 90_000;
/// Consecutive bad heartbeats before a living owner is stopped and restarted.
pub const STALE_LIMIT: u32 = 2;
/// How long [`apply`] waits for a stopped owner to release its lock.
pub const RELEASE_POLLS: u32 = 20;
pub const RELEASE_POLL_MS: u64 = 250;

/// What the configuration asks the watchdog to keep running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTarget {
    pub groups: Vec<String>,
    /// Keep the console (MCP tools depend on it).
    pub console: bool,
    pub console_port: u16,
}

/// Port of the Node watchdog's config gate: `backends.mcpRustBridge` must be an explicit boolean, `rollout.rustGroups`
/// an explicit array of distinct non-empty strings and `consolePort` (default 3100) a valid port. Anything else refuses
/// to start any process.
pub fn live_target_from_config(cfg: &Value) -> Result<LiveTarget, String> {
    let console = cfg
        .get("backends")
        .and_then(|b| b.get("mcpRustBridge"))
        .and_then(Value::as_bool)
        .ok_or("mcpRustBridge must be explicit")?;
    let groups = cfg
        .get("rollout")
        .and_then(|r| r.get("rustGroups"))
        .and_then(Value::as_array)
        .ok_or("rustGroups must be an explicit array")?;
    let mut list = Vec::with_capacity(groups.len());
    for group in groups {
        let text = group.as_str().filter(|s| !s.trim().is_empty()).ok_or("rustGroups must hold non-empty strings")?;
        if list.iter().any(|g| g == text) {
            return Err("rustGroups must not repeat a group".to_string());
        }
        list.push(text.to_string());
    }
    if !list.is_empty() {
        crate::runtime::validate_canonical_live_groups(&list.join(",")).map_err(str::to_string)?;
    }
    let port = match cfg.get("consolePort") {
        None | Some(Value::Null) => 3100,
        Some(value) => value
            .as_f64()
            .filter(|p| p.fract() == 0.0 && (1.0..=65535.0).contains(p))
            .map(|p| p as u16)
            .ok_or("consolePort is invalid")?,
    };
    Ok(LiveTarget { groups: list, console, console_port: port })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleProbe {
    Absent,
    /// Listening, and the listener is this installation's console.
    Managed { pid: u32 },
    /// Listening, but by another program: never stopped.
    Foreign { pid: u32 },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heartbeat {
    Fresh { live: bool, groups: Vec<String> },
    Stale { age_ms: i64 },
    /// No heartbeat file: the owner has not written one.
    Missing,
    /// A heartbeat file that is not a heartbeat (the owner wrote garbage).
    Invalid,
    /// The file exists but could not be read: says nothing about the owner.
    Unknown(String),
}

/// Classify the result of reading `rust-heartbeat-live.json` (`{ts, pid, live, groups}`).
pub fn read_heartbeat(read: std::io::Result<Vec<u8>>, now_ms: i64) -> Heartbeat {
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Heartbeat::Missing,
        Err(error) => return Heartbeat::Unknown(format!("heartbeat is unreadable: {}", error.kind())),
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { return Heartbeat::Invalid };
    let Some(ts) = value.get("ts").and_then(Value::as_f64) else { return Heartbeat::Invalid };
    let age_ms = now_ms - ts as i64;
    if age_ms >= HEARTBEAT_FRESH_MS {
        return Heartbeat::Stale { age_ms };
    }
    let live = value.get("live").and_then(Value::as_bool) == Some(true);
    let groups = value
        .get("groups")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|g| g.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Heartbeat::Fresh { live, groups }
}

/// The live owner as seen through its OS lock, its marker and the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveObservation {
    Free,
    /// The lock is held and the marker names a process; `liveness` is that process checked against the OS.
    Held { marker: OwnerMarker, liveness: Liveness },
    /// The lock is held but the marker cannot be read.
    HeldUnidentified,
    Unknown(String),
}

/// Combine an owner-lock probe with an identity check of the marker it names.
pub fn observe_live(probe: OwnerProbe, check: impl FnOnce(&ProcessIdentity) -> Liveness) -> LiveObservation {
    match probe {
        OwnerProbe::Free { .. } => LiveObservation::Free,
        OwnerProbe::Unknown(reason) => LiveObservation::Unknown(reason),
        OwnerProbe::Held { marker: None } => LiveObservation::HeldUnidentified,
        OwnerProbe::Held { marker: Some(marker) } => {
            let liveness = check(&marker.identity);
            LiveObservation::Held { marker, liveness }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub desired: Effective,
    pub target: Result<LiveTarget, String>,
    pub console: ConsoleProbe,
    pub live: LiveObservation,
    pub heartbeat: Heartbeat,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchState {
    pub stale_count: u32,
    /// A heartbeat history belongs to one exact process, including PID start time and executable.
    pub stale_owner: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    StartConsole { port: u16 },
    StopConsole { pid: u32 },
    StartLive { groups: Vec<String> },
    /// Stop exactly this owner (identity re-verified first); start a new one with `restart` once its lock is free.
    StopLive { identity: ProcessIdentity, restart: Option<Vec<String>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Decision {
    pub actions: Vec<Action>,
    pub state: WatchState,
    pub notes: Vec<String>,
    /// For the administrator (ticket 25's alert limiter dedups them when wired).
    pub alerts: Vec<String>,
    /// The tick could not do what the configuration asks (Node watchdog: exit code 1).
    pub failed: bool,
}

impl Decision {
    fn fail(&mut self, note: impl Into<String>) {
        self.failed = true;
        self.notes.push(note.into());
    }

    fn alert(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.notes.push(text.clone());
        self.alerts.push(text);
        self.failed = true;
    }
}

fn same_groups(left: &[String], right: &[String]) -> bool {
    let mut a = left.to_vec();
    let mut b = right.to_vec();
    a.sort();
    b.sort();
    a == b
}

pub fn decide(obs: &Observation, previous: &WatchState) -> Decision {
    let mut d = Decision { state: previous.clone(), ..Decision::default() };
    match &obs.live {
        LiveObservation::Held { marker, .. } if d.state.stale_owner.as_ref() != Some(&marker.identity) => {
            d.state.stale_owner = Some(marker.identity.clone());
            d.state.stale_count = 0;
        }
        LiveObservation::Free => d.state = WatchState::default(),
        _ => {}
    }
    let target = match &obs.target {
        Ok(target) => target,
        Err(reason) => {
            d.fail(format!("configuration is missing or invalid ({reason}); no process is started or stopped"));
            return d;
        }
    };
    let wants = match &obs.desired {
        Effective::Unknown(reason) => {
            d.alert(format!("desired state is unknown ({reason}); the watchdog starts and stops nothing"));
            return d;
        }
        Effective::Act { wants, .. } => *wants,
    };

    // ---- console ----
    if !wants.console {
        if let ConsoleProbe::Managed { pid } = &obs.console {
            d.actions.push(Action::StopConsole { pid: *pid });
        }
    } else if target.console {
        match &obs.console {
            ConsoleProbe::Managed { .. } => {}
            ConsoleProbe::Absent => {
                d.notes.push(format!("console {} is not listening; starting it", target.console_port));
                d.actions.push(Action::StartConsole { port: target.console_port });
            }
            ConsoleProbe::Foreign { pid } => {
                d.fail(format!("console port {} is held by unmanaged process {pid}; it is not stopped", target.console_port))
            }
            ConsoleProbe::Unknown(reason) => {
                d.fail(format!("console listener state is unknown ({reason}); nothing is started or stopped"))
            }
        }
    }

    // ---- live owner ----
    let want_live = wants.live && !target.groups.is_empty();
    match (&obs.live, want_live) {
        (LiveObservation::Unknown(reason), _) => {
            d.fail(format!("live owner state is unknown ({reason}); no instance is started or stopped"));
        }
        (LiveObservation::HeldUnidentified, _) => {
            d.alert("the live owner lock is held but its owner cannot be identified; no takeover");
        }
        (LiveObservation::Held { liveness, .. }, _) if *liveness != Liveness::Matches => {
            d.alert(format!(
                "the live owner lock is held but its recorded owner is not verified ({liveness:?}); nothing is stopped"
            ));
        }
        (LiveObservation::Free, false) => {
            d.state.stale_count = 0;
        }
        (LiveObservation::Free, true) => {
            d.state.stale_count = 0;
            d.notes.push(format!("no live owner; starting one for {}", target.groups.join(",")));
            d.actions.push(Action::StartLive { groups: target.groups.clone() });
        }
        (LiveObservation::Held { marker, .. }, false) => {
            d.state.stale_count = 0;
            d.notes.push(format!("a live owner (pid {}) runs although none is wanted; stopping it", marker.identity.pid));
            d.actions.push(Action::StopLive { identity: marker.identity.clone(), restart: None });
        }
        (LiveObservation::Held { marker, .. }, true) => {
            let serving = marker.groups.clone().unwrap_or_default();
            if !same_groups(&serving, &target.groups) {
                d.state.stale_count = 0;
                d.notes.push(format!(
                    "live owner serves {} but the target is {}; reconciling",
                    serving.join(","),
                    target.groups.join(",")
                ));
                d.actions.push(Action::StopLive {
                    identity: marker.identity.clone(),
                    restart: Some(target.groups.clone()),
                });
                return d;
            }
            match &obs.heartbeat {
                Heartbeat::Fresh { live: true, groups } if same_groups(groups, &target.groups) => {
                    d.state.stale_count = 0;
                }
                Heartbeat::Unknown(reason) => {
                    d.fail(format!("heartbeat is unknown ({reason}); the owner is left running"));
                }
                bad => {
                    d.state.stale_count = d.state.stale_count.saturating_add(1);
                    d.notes.push(format!(
                        "live owner pid {} runs but its heartbeat is bad ({bad:?}), {} in a row",
                        marker.identity.pid, d.state.stale_count
                    ));
                    if d.state.stale_count >= STALE_LIMIT {
                        d.state.stale_count = 0;
                        d.actions.push(Action::StopLive {
                            identity: marker.identity.clone(),
                            restart: Some(target.groups.clone()),
                        });
                    }
                }
            }
        }
    }
    d
}

/// The side effects of a tick. The real implementation probes the owner lock, kills through
/// [`crate::lifecycle::kill_verified`] and spawns hidden children; tests use a fake.
pub trait Effects {
    fn kill_verified(&mut self, identity: &ProcessIdentity, restart: Option<&[String]>) -> KillOutcome;
    fn probe_live(&mut self) -> OwnerProbe;
    fn start_live(&mut self, groups: &[String]) -> Result<(), String>;
    fn start_console(&mut self, port: u16) -> Result<(), String>;
    fn stop_console(&mut self, pid: u32) -> Result<(), String>;
    fn wait_ms(&mut self, ms: u64);
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    pub notes: Vec<String>,
    pub alerts: Vec<String>,
    pub failed: bool,
}

impl Applied {
    fn alert(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.notes.push(text.clone());
        self.alerts.push(text);
        self.failed = true;
    }
}

/// Carry out a decision. A stop is followed by a start only after the old owner's lock is observed free.
pub fn apply(decision: &Decision, fx: &mut dyn Effects) -> Applied {
    let mut out = Applied { notes: Vec::new(), alerts: Vec::new(), failed: decision.failed };
    for action in &decision.actions {
        match action {
            Action::StartConsole { port } => match fx.start_console(*port) {
                Ok(()) => out.notes.push(format!("console {port} started")),
                Err(error) => out.alert(format!("console {port} could not be started: {error}")),
            },
            Action::StopConsole { pid } => match fx.stop_console(*pid) {
                Ok(()) => out.notes.push(format!("console pid {pid} stopped")),
                Err(error) => out.alert(format!("console pid {pid} could not be stopped: {error}")),
            },
            Action::StartLive { groups } => start_live(groups, fx, &mut out),
            Action::StopLive { identity, restart } => {
                match fx.kill_verified(identity, restart.as_deref()) {
                    KillOutcome::Killed | KillOutcome::Gone => {}
                    KillOutcome::Refused(reason) => {
                        out.alert(format!("live owner pid {} was not stopped: {reason}", identity.pid));
                        continue;
                    }
                    KillOutcome::Unknown(reason) => {
                        out.alert(format!("live owner pid {} could not be stopped: {reason}", identity.pid));
                        continue;
                    }
                }
                let mut released = false;
                for _ in 0..RELEASE_POLLS {
                    match fx.probe_live() {
                        OwnerProbe::Free { .. } => {
                            released = true;
                            break;
                        }
                        OwnerProbe::Held { .. } | OwnerProbe::Unknown(_) => fx.wait_ms(RELEASE_POLL_MS),
                    }
                }
                if !released {
                    out.alert(format!(
                        "the stopped live owner pid {} still holds (or may hold) its lock; no second instance is started",
                        identity.pid
                    ));
                    continue;
                }
                out.notes.push(format!("live owner pid {} stopped", identity.pid));
                if let Some(groups) = restart {
                    start_live(groups, fx, &mut out);
                }
            }
        }
    }
    out
}

fn start_live(groups: &[String], fx: &mut dyn Effects, out: &mut Applied) {
    match fx.start_live(groups) {
        Ok(()) => out.notes.push(format!("live owner started for {}", groups.join(","))),
        Err(error) => out.alert(format!("live owner could not be started: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{DesiredState, Wants};
    use serde_json::json;

    fn running() -> Effective {
        Effective::Act { state: DesiredState::Running, wants: Wants { live: true, console: true, jobs: true } }
    }

    fn stopped() -> Effective {
        Effective::Act { state: DesiredState::Stopped, wants: Wants { live: false, console: false, jobs: false } }
    }

    fn maintenance() -> Effective {
        Effective::Act { state: DesiredState::Maintenance, wants: Wants { live: false, console: true, jobs: false } }
    }

    fn groups(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn target(list: &[&str]) -> Result<LiveTarget, String> {
        Ok(LiveTarget { groups: groups(list), console: true, console_port: 3100 })
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity { pid, start: 1000 + pid as u64, exe: "/opt/her".into() }
    }

    fn held(pid: u32, serving: &[&str], liveness: Liveness) -> LiveObservation {
        LiveObservation::Held {
            marker: OwnerMarker {
                v: 1,
                role: "live".into(),
                identity: identity(pid),
                acquired_at: 0,
                refreshed_at: 0,
                groups: Some(groups(serving)),
            },
            liveness,
        }
    }

    fn fresh(list: &[&str]) -> Heartbeat {
        Heartbeat::Fresh { live: true, groups: groups(list) }
    }

    fn obs(desired: Effective, live: LiveObservation, heartbeat: Heartbeat) -> Observation {
        Observation { desired, target: target(&["g1", "g2"]), console: ConsoleProbe::Managed { pid: 7 }, live, heartbeat }
    }

    #[test]
    fn config_gate_matches_the_node_watchdog() {
        let ok = json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":["group:1","private:2"]}});
        assert_eq!(
            live_target_from_config(&ok),
            Ok(LiveTarget { groups: groups(&["group:1", "private:2"]), console: true, console_port: 3100 })
        );
        let port = json!({"backends":{"mcpRustBridge":false},"rollout":{"rustGroups":[]},"consolePort":3200});
        assert_eq!(live_target_from_config(&port).unwrap().console_port, 3200);
        for bad in [
            json!({"rollout":{"rustGroups":["a"]}}),
            json!({"backends":{"mcpRustBridge":"yes"},"rollout":{"rustGroups":["a"]}}),
            json!({"backends":{"mcpRustBridge":true}}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":["a",""]}}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":["a","a"]}}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":["not-a-key"]}}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[1]}}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[]},"consolePort":70000}),
            json!({"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[]},"consolePort":31.5}),
        ] {
            assert!(live_target_from_config(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn heartbeat_reads_distinguish_missing_stale_invalid_and_unknown() {
        let now = 1_000_000;
        let hb = |ts: i64| Ok(json!({"ts": ts, "pid": 1, "live": true, "groups": ["g1"]}).to_string().into_bytes());
        assert_eq!(read_heartbeat(hb(now - 1000), now), fresh(&["g1"]));
        assert_eq!(read_heartbeat(hb(now - 90_000), now), Heartbeat::Stale { age_ms: 90_000 });
        assert_eq!(read_heartbeat(Err(std::io::ErrorKind::NotFound.into()), now), Heartbeat::Missing);
        assert_eq!(read_heartbeat(Ok(b"{".to_vec()), now), Heartbeat::Invalid);
        assert!(matches!(
            read_heartbeat(Err(std::io::ErrorKind::PermissionDenied.into()), now),
            Heartbeat::Unknown(_)
        ));
    }

    #[test]
    fn invalid_configuration_starts_and_stops_nothing() {
        let mut o = obs(running(), LiveObservation::Free, Heartbeat::Missing);
        o.target = Err("rustGroups must be an explicit array".into());
        o.console = ConsoleProbe::Absent;
        let d = decide(&o, &WatchState::default());
        assert!(d.actions.is_empty() && d.failed);
    }

    #[test]
    fn unknown_desired_state_starts_and_stops_nothing() {
        let mut o = obs(Effective::Unknown("unreadable".into()), held(5, &["g1", "g2"], Liveness::Matches), Heartbeat::Missing);
        o.console = ConsoleProbe::Absent;
        let d = decide(&o, &WatchState { stale_count: 9, stale_owner: Some(identity(5)) });
        assert!(d.actions.is_empty());
        assert_eq!(d.alerts.len(), 1);
        assert_eq!(d.state.stale_count, 9, "an unknown tick does not count");
    }

    #[test]
    fn a_stop_is_never_undone_and_a_running_owner_is_stopped_once_verified() {
        // stopped + no owner: nothing is started, not even the console
        let mut o = obs(stopped(), LiveObservation::Free, Heartbeat::Missing);
        o.console = ConsoleProbe::Absent;
        let d = decide(&o, &WatchState::default());
        assert!(d.actions.is_empty() && !d.failed, "{d:?}");
        // stopped + a verified owner still running: stop it, start nothing after
        let d = decide(&obs(stopped(), held(5, &["g1", "g2"], Liveness::Matches), fresh(&["g1", "g2"])), &WatchState::default());
        assert_eq!(d.actions, vec![Action::StopConsole { pid: 7 }, Action::StopLive { identity: identity(5), restart: None }]);
        // maintenance keeps the console but not the live owner
        let mut o = obs(maintenance(), LiveObservation::Free, Heartbeat::Missing);
        o.console = ConsoleProbe::Absent;
        assert_eq!(decide(&o, &WatchState::default()).actions, vec![Action::StartConsole { port: 3100 }]);
    }

    #[test]
    fn empty_rollout_stops_the_owner_like_the_node_watchdog() {
        let mut o = obs(running(), held(5, &["g1"], Liveness::Matches), fresh(&["g1"]));
        o.target = target(&[]);
        let d = decide(&o, &WatchState::default());
        assert_eq!(d.actions, vec![Action::StopLive { identity: identity(5), restart: None }]);
        o.live = LiveObservation::Free;
        assert!(decide(&o, &WatchState::default()).actions.is_empty());
    }

    #[test]
    fn missing_owner_is_started_and_healthy_owner_is_left_alone() {
        let d = decide(&obs(running(), LiveObservation::Free, Heartbeat::Missing), &WatchState { stale_count: 1, stale_owner: Some(identity(5)) });
        assert_eq!(d.actions, vec![Action::StartLive { groups: groups(&["g1", "g2"]) }]);
        assert_eq!(d.state.stale_count, 0);
        let d = decide(&obs(running(), held(5, &["g2", "g1"], Liveness::Matches), fresh(&["g1", "g2"])), &WatchState { stale_count: 1, stale_owner: Some(identity(5)) });
        assert!(d.actions.is_empty() && !d.failed);
        assert_eq!(d.state.stale_count, 0);
    }

    #[test]
    fn a_hung_owner_is_restarted_only_after_two_bad_heartbeats() {
        for bad in [Heartbeat::Stale { age_ms: 100_000 }, Heartbeat::Missing, Heartbeat::Invalid, fresh(&["g1"]), Heartbeat::Fresh { live: false, groups: groups(&["g1", "g2"]) }] {
            let o = obs(running(), held(5, &["g1", "g2"], Liveness::Matches), bad.clone());
            let first = decide(&o, &WatchState::default());
            assert!(first.actions.is_empty(), "first bad heartbeat only observes: {bad:?}");
            assert_eq!(first.state.stale_count, 1);
            let second = decide(&o, &first.state);
            assert_eq!(
                second.actions,
                vec![Action::StopLive { identity: identity(5), restart: Some(groups(&["g1", "g2"])) }],
                "{bad:?}"
            );
            assert_eq!(second.state.stale_count, 0);
        }
    }

    #[test]
    fn replacement_owner_does_not_inherit_another_owners_bad_heartbeat() {
        let first = decide(
            &obs(running(), held(5, &["g1", "g2"], Liveness::Matches), Heartbeat::Missing),
            &WatchState::default(),
        );
        assert_eq!(first.state.stale_count, 1);
        let replacement = obs(running(), held(6, &["g1", "g2"], Liveness::Matches), Heartbeat::Missing);
        let second = decide(&replacement, &first.state);
        assert!(second.actions.is_empty(), "a replacement's first bad heartbeat must not stop it: {second:?}");
        assert_eq!(second.state.stale_count, 1);
        let third = decide(&replacement, &second.state);
        assert_eq!(third.actions, vec![Action::StopLive { identity: identity(6), restart: Some(groups(&["g1", "g2"])) }]);

        let mut recycled = obs(running(), held(5, &["g1", "g2"], Liveness::Matches), Heartbeat::Missing);
        if let LiveObservation::Held { marker, .. } = &mut recycled.live { marker.identity.start += 1; }
        let after_reuse = decide(&recycled, &first.state);
        assert!(after_reuse.actions.is_empty(), "a reused PID is a different owner: {after_reuse:?}");
        assert_eq!(after_reuse.state.stale_count, 1);
    }

    #[test]
    fn unreadable_heartbeat_neither_counts_nor_kills() {
        let o = obs(running(), held(5, &["g1", "g2"], Liveness::Matches), Heartbeat::Unknown("denied".into()));
        let d = decide(&o, &WatchState { stale_count: 1, stale_owner: Some(identity(5)) });
        assert!(d.actions.is_empty() && d.failed);
        assert_eq!(d.state.stale_count, 1);
    }

    #[test]
    fn a_mismatched_scope_is_reconciled_by_a_verified_restart() {
        let d = decide(&obs(running(), held(5, &["g1"], Liveness::Matches), fresh(&["g1"])), &WatchState::default());
        assert_eq!(d.actions, vec![Action::StopLive { identity: identity(5), restart: Some(groups(&["g1", "g2"])) }]);
    }

    #[test]
    fn unknown_or_unverifiable_owners_are_never_stopped_or_doubled() {
        let cases = [
            LiveObservation::Unknown("lock unreadable".into()),
            LiveObservation::HeldUnidentified,
            held(5, &["g1", "g2"], Liveness::Unknown("access denied".into())),
            held(5, &["g1", "g2"], Liveness::Mismatch("pid reused".into())),
            held(5, &["g1", "g2"], Liveness::Gone),
        ];
        for live in cases {
            for desired in [running(), stopped()] {
                for state in [WatchState::default(), WatchState { stale_count: 5, stale_owner: Some(identity(5)) }] {
                    let d = decide(&obs(desired.clone(), live.clone(), Heartbeat::Stale { age_ms: 1_000_000 }), &state);
                    assert!(
                        d.actions.iter().all(|a| matches!(a, Action::StartConsole { .. } | Action::StopConsole { .. })),
                        "no live start or stop for {live:?}: {d:?}"
                    );
                    assert!(d.failed);
                }
            }
        }
    }

    #[test]
    fn console_is_started_only_on_a_free_port() {
        let base = obs(running(), held(5, &["g1", "g2"], Liveness::Matches), fresh(&["g1", "g2"]));
        let with = |console| Observation { console, ..base.clone() };
        assert_eq!(decide(&with(ConsoleProbe::Absent), &WatchState::default()).actions, vec![Action::StartConsole { port: 3100 }]);
        let foreign = decide(&with(ConsoleProbe::Foreign { pid: 99 }), &WatchState::default());
        assert!(foreign.actions.is_empty() && foreign.failed);
        let unknown = decide(&with(ConsoleProbe::Unknown("netstat failed".into())), &WatchState::default());
        assert!(unknown.actions.is_empty() && unknown.failed);
        let mut no_console = with(ConsoleProbe::Absent);
        no_console.target = Ok(LiveTarget { groups: groups(&["g1", "g2"]), console: false, console_port: 3100 });
        assert!(decide(&no_console, &WatchState::default()).actions.is_empty());
    }

    #[test]
    fn observe_live_checks_the_marker_identity() {
        let marker = OwnerMarker { v: 1, role: "live".into(), identity: identity(5), acquired_at: 0, refreshed_at: 0, groups: None };
        let seen = observe_live(OwnerProbe::Held { marker: Some(marker.clone()) }, |id| {
            assert_eq!(id, &identity(5));
            Liveness::Matches
        });
        assert_eq!(seen, LiveObservation::Held { marker, liveness: Liveness::Matches });
        assert_eq!(observe_live(OwnerProbe::Held { marker: None }, |_| unreachable!()), LiveObservation::HeldUnidentified);
        assert_eq!(observe_live(OwnerProbe::Free { last: None }, |_| unreachable!()), LiveObservation::Free);
    }

    #[derive(Default)]
    struct FakeFx {
        kill: Option<KillOutcome>,
        probes: Vec<OwnerProbe>,
        log: Vec<String>,
        start_error: Option<String>,
    }

    impl Effects for FakeFx {
        fn kill_verified(&mut self, identity: &ProcessIdentity, _restart: Option<&[String]>) -> KillOutcome {
            self.log.push(format!("kill {}", identity.pid));
            self.kill.clone().unwrap_or(KillOutcome::Killed)
        }
        fn probe_live(&mut self) -> OwnerProbe {
            self.log.push("probe".into());
            if self.probes.is_empty() {
                OwnerProbe::Held { marker: None }
            } else {
                self.probes.remove(0)
            }
        }
        fn start_live(&mut self, groups: &[String]) -> Result<(), String> {
            self.log.push(format!("start live {}", groups.join(",")));
            self.start_error.clone().map_or(Ok(()), Err)
        }
        fn start_console(&mut self, port: u16) -> Result<(), String> {
            self.log.push(format!("start console {port}"));
            Ok(())
        }
        fn stop_console(&mut self, pid: u32) -> Result<(), String> {
            self.log.push(format!("stop console {pid}"));
            Ok(())
        }
        fn wait_ms(&mut self, _ms: u64) {
            self.log.push("wait".into());
        }
    }

    fn stop_and_restart() -> Decision {
        Decision {
            actions: vec![Action::StopLive { identity: identity(5), restart: Some(groups(&["g1"])) }],
            ..Decision::default()
        }
    }

    #[test]
    fn restart_starts_only_after_the_old_lock_is_free() {
        let mut fx = FakeFx {
            probes: vec![OwnerProbe::Held { marker: None }, OwnerProbe::Unknown("x".into()), OwnerProbe::Free { last: None }],
            ..FakeFx::default()
        };
        let out = apply(&stop_and_restart(), &mut fx);
        assert!(!out.failed, "{out:?}");
        assert_eq!(fx.log, ["kill 5", "probe", "wait", "probe", "wait", "probe", "start live g1"]);
    }

    #[test]
    fn no_second_instance_while_the_old_owner_may_still_hold_its_lock() {
        let mut fx = FakeFx::default(); // the lock stays held
        let out = apply(&stop_and_restart(), &mut fx);
        assert!(out.failed && out.alerts.len() == 1);
        assert!(!fx.log.iter().any(|l| l.starts_with("start live")), "{:?}", fx.log);
        assert_eq!(fx.log.iter().filter(|l| *l == "probe").count(), RELEASE_POLLS as usize);
    }

    #[test]
    fn refused_or_unknown_kills_start_nothing() {
        for outcome in [KillOutcome::Refused("pid reused".into()), KillOutcome::Unknown("access denied".into())] {
            let mut fx = FakeFx { kill: Some(outcome.clone()), probes: vec![OwnerProbe::Free { last: None }], ..FakeFx::default() };
            let out = apply(&stop_and_restart(), &mut fx);
            assert!(out.failed, "{outcome:?}");
            assert_eq!(fx.log, ["kill 5"], "{outcome:?}");
        }
        // an owner that had already exited counts as stopped once its lock is free
        let mut fx = FakeFx { kill: Some(KillOutcome::Gone), probes: vec![OwnerProbe::Free { last: None }], ..FakeFx::default() };
        assert!(!apply(&stop_and_restart(), &mut fx).failed);
        assert_eq!(fx.log.last().unwrap(), "start live g1");
    }

    #[test]
    fn start_failures_are_alerted() {
        let mut fx = FakeFx { start_error: Some("spawn failed".into()), ..FakeFx::default() };
        let d = Decision { actions: vec![Action::StartLive { groups: groups(&["g1"]) }], ..Decision::default() };
        let out = apply(&d, &mut fx);
        assert!(out.failed && out.alerts[0].contains("spawn failed"));
    }
}
