//! Ticket 25 external-behaviour acceptance: dependency recovery, the candidate chain and the administrator alerts, against
//! real `her` processes (live bridge + console) and the fault-injecting fake DSH/OneBot world of
//! `her/tests/fixtures/fake-world.mjs`. Each test runs one case of `her/tests/fixtures/dependency-recovery-cases.mjs` (Node) and
//! passes only when that case printed `PASS <case>` and exited 0; a missing Node or a failed assertion is a failure, never
//! a skip. The fault seam of the crash case exists only in a `--features parity-clock` build, so run this target as
//!
//!   cargo test --test dependency_recovery_e2e --features parity-clock -- --test-threads=1
//!
//! No real QQ, DSH or model endpoint is contacted; every run uses synthetic roots under the system temp directory.

#[path = "../src/install_root.rs"]
mod install_root;

use std::path::PathBuf;
use std::process::Command;

fn run_case(case: &str) {
    let candidate = env!("CARGO_BIN_EXE_her");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dependency-recovery-cases.mjs");
    let out = crate::install_root::development_root(std::env::temp_dir().join(format!("her-dependency-recovery-{}-{case}", std::process::id())));
    let output = Command::new("node")
        .arg(&script)
        .args(["--candidate", candidate, "--case", case, "--out"])
        .arg(&out)
        .output()
        .expect("node must be installed to run the dependency recovery cases");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.lines().any(|l| l.starts_with(&format!("PASS {case}"))),
        "case {case} did not pass (exit {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );
    let _ = std::fs::remove_dir_all(out);
}

macro_rules! cases {
    ($($test:ident => $case:expr),+ $(,)?) => { $(#[test] fn $test() { run_case($case); })+ };
}

cases! {
    a_401_after_a_dsh_restart_heals_once_per_call_and_delivers_the_prompt_once => "reauth-401-rotation-call-counts",
    a_dsh_outage_queues_messages_bounds_the_calls_and_replays_after_recovery => "dsh-outage-queues-then-replays-through-the-access-checks",
    a_revocation_made_during_an_outage_is_honoured_by_the_replay => "dsh-outage-replay-honours-a-revocation-made-while-down",
    first_generation_reserved_waits_for_dsh_and_replays_after_recovery => "reserved-outage-waits-for-dsh-then-replays",
    reserved2_waits_for_dsh_without_calling_it_and_replays_after_recovery => "reserved2-outage-waits-for-dsh-then-replays",
    a_cli_choice_releases_a_waiting_reserved2_wake_without_calling_a_down_dsh => "reserved2-cli-choice-releases-waiting-work-during-dsh-outage",
    a_reply_timeout_never_redelivers_the_prompt_to_the_next_candidate => "turn-timeout-never-redelivers-the-prompt",
    a_selection_failure_after_a_failover_keeps_one_owner_alert_and_exhaustion_evidence => "selection-failure-after-a-failover-step-sends-the-exhausted-alert",
    a_stale_model_error_note_never_hides_a_later_different_failure => "a-stale-model-error-note-does-not-hide-a-later-different-failure",
    a_model_error_fails_over_alerts_the_owner_and_leaks_nothing => "model-error-fails-over-with-alerts-and-no-leaks",
    every_candidate_failing_sends_one_content_free_owner_alert => "all-candidates-fail-exhausted-alert-and-texts",
    a_second_failover_step_alert_inside_a_minute_is_deduplicated => "second-failover-step-alert-is-deduplicated-inside-a-minute",
    the_failover_retry_cap_holds_across_messages_and_the_degraded_candidate_is_sticky => "retry-cap-holds-across-messages-and-the-degraded-candidate-is-sticky",
    startup_failures_are_on_the_run_record_before_the_console_and_served_behind_the_token => "startup-failure-is-on-the-run-record-and-served-behind-the-token",
    two_rejected_fresh_login_cookies_alert_the_administrator_once => "dsh-login-rejected-twice-alerts-once-naming-the-likely-causes",
    a_long_gateway_outage_is_paced_by_backoff_not_a_storm => "gateway-reconnects-back_off-instead-of-storming",
    a_stale_snowluma_token_is_healed_into_the_selected_config_only => "snowluma-token-self-heal-writes-only-the-selected-config",
    follow_discovered_endpoint_false_never_persists_the_endpoint => "snowluma-follow-discovered-endpoint-false-never-writes-the-endpoint",
    a_crash_after_a_turn_ended_resumed_is_visible_and_never_reruns_it => "a-crash-after-a-turn-ended-resumed-is-visible-and-never-reruns-it",
}
