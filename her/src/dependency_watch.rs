//! The live bridge's dependency watch (ticket 25): the DSH probe loop, the SnowLuma status poll, the health snapshot the
//! console reports, the crash marker, and the startup findings the administrator hears about.
//!
//! One task per process (`run`), not per gateway connection. Everything here is best effort and bounded: a probe that
//! fails is an observation, never an error that stops the loop; every state change is an edge with one run-record entry
//! (see `dsh_health`, `gateway_health`).

use crate::gateway_health::{self, Report};
use crate::recovery_log;
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::time::Duration;

/// JS `lastPacket` kind: `post_type[/meta_event_type][/message_type]`.
pub fn note_frame(post_type: &str, meta_event_type: &str, status: Option<&Value>) {
    let now = crate::parity_clock::now_ms();
    let mut kind = if post_type.is_empty() { "event".to_string() } else { post_type.to_string() };
    if !post_type.is_empty() && !meta_event_type.is_empty() {
        kind.push('/');
        kind.push_str(meta_event_type);
    }
    gateway_health::with(|h| {
        h.note_packet(&kind, now);
        if post_type == "meta_event" && meta_event_type == "heartbeat" {
            h.note_good(status.and_then(|s| s.get("good")).and_then(Value::as_bool), "heartbeat", now);
            if let Some(online) = status.and_then(|s| s.get("online")).and_then(Value::as_bool) {
                h.account_online = Some(online);
            }
        }
    });
}

fn edge_report(context: &RuntimeContext, cfg: &Value) {
    let now = crate::parity_clock::now_ms();
    match gateway_health::with(|h| h.report(now)) {
        Report::Steady => {}
        Report::Degraded(hint) => {
            crate::safe_println!("[health] ⚠️ SnowLuma 上游不健康：{hint}");
            recovery_log::record(context, Trigger::Timer, Component::Snowluma, "degraded", Outcome::Failed, &hint);
            crate::admin_alert::notify_notice(context, cfg, &format!("SnowLuma 上游不健康：{hint}"), "snowluma-degraded");
        }
        Report::Recovered => {
            crate::safe_println!("[health] ✅ SnowLuma 上游已恢复正常（good≠false 且事件包在流动）");
            recovery_log::record(context, Trigger::Recovery, Component::Snowluma, "recovered", Outcome::Ok, "receive chain healthy again");
            crate::admin_alert::notify_notice(context, cfg, "SnowLuma 上游已恢复正常", "snowluma-recovered");
        }
    }
}

/// JS `pollSnowlumaStatus`: `get_status` for the account's online flag and the receive chain's `good`.
pub(crate) fn probe_status(context: &RuntimeContext, cfg: &Value, deadline: Option<std::time::Instant>) -> Result<Value, String> {
    crate::console::media_onebot(context, cfg).and_then(|ob| {
        match deadline {
            Some(deadline) => ob.request_with_deadline("get_status", json!({}), deadline),
            None => ob.request_with_timeout("get_status", json!({}), Duration::from_secs(8))
                .ok_or_else(|| "no answer from the gateway".to_string()),
        }
    })
}

fn poll_status(context: &RuntimeContext, cfg: &Value) {
    let result = probe_status(context, cfg, None);
    let now = crate::parity_clock::now_ms();
    gateway_health::with(|h| match &result {
        Ok(body) if body.get("status").and_then(Value::as_str) == Some("ok") => {
            h.note_good(body.pointer("/data/good").and_then(Value::as_bool), "get_status", now);
            if let Some(online) = body.pointer("/data/online").and_then(Value::as_bool) {
                h.account_online = Some(online);
            }
        }
        Ok(body) => h.note_poll_error(&format!(
            "status {}",
            body.get("wording").and_then(Value::as_str).unwrap_or_else(|| body.get("status").and_then(Value::as_str).unwrap_or("?"))
        )),
        Err(error) => h.note_poll_error(error),
    });
}

/// The watch loop: a DSH probe every 5 s (JS `checkDsh`), the SnowLuma `get_status` poll every 60 s (first after 5 s),
/// the gateway edge report every 30 s, the pending run-record summaries published every minute, and the health snapshot
/// published after every probe for the console process.
pub async fn run(context: RuntimeContext) {
    let mut tick: u64 = 0;
    loop {
        let cfg = context.load_config().unwrap_or(Value::Null);
        gateway_health::with(|h| h.set_silence_warn(cfg.pointer("/snowluma/silenceWarnMs").and_then(Value::as_i64)));
        let probe_context = context.clone();
        let probed = tokio::task::spawn_blocking(move || crate::dsh_health::probe_login(&probe_context, None)).await;
        match probed {
            Ok(check) => {
                crate::dsh_health::observe_login(&context, &cfg, &check.login);
                match check.result {
                    Ok(()) => crate::dsh_health::observe(&context, &cfg, true, "", true),
                    Err(error) => crate::dsh_health::observe(&context, &cfg, false, &error, true),
                };
            }
            Err(error) => println!("[health] DSH 探测任务异常：{error}"),
        }
        if tick % 12 == 1 {
            let (poll_context, poll_cfg) = (context.clone(), cfg.clone());
            let _ = tokio::task::spawn_blocking(move || poll_status(&poll_context, &poll_cfg)).await;
        }
        if tick % 6 == 0 || tick % 12 == 1 {
            edge_report(&context, &cfg);
        }
        if tick % 12 == 0 {
            recovery_log::flush(&context);
        }
        crate::dsh_health::publish(&context);
        tick += 1;
        tokio::time::sleep(Duration::from_millis(crate::dsh_health::PROBE_INTERVAL_MS)).await;
    }
}

/// Adopt a successful SnowLuma token heal into the connection variables of the reconnect loop (token, endpoint, the
/// reloaded config). `true` when the WebSocket endpoint or token changed (the caller then reconnects at once).
pub async fn adopt_heal(
    context: &RuntimeContext,
    throttle: &mut crate::snowluma_heal::Throttle,
    reason: &str,
    ws: &mut String,
    token: &mut Option<String>,
    http_token: &mut Option<String>,
    cfg_raw: &mut Value,
) -> bool {
    if !context.is_isolated() || !throttle.allow(crate::parity_clock::now_ms()) {
        return false;
    }
    let base = match context.dependency_url("onebot") {
        Some(base) => base.to_string(),
        None => return false,
    };
    let (ctx, cfg, current_ws) = (context.clone(), cfg_raw.clone(), ws.clone());
    let healed = tokio::task::spawn_blocking(move || {
        crate::snowluma_heal::heal(&ctx, &cfg, &base, &current_ws, Duration::from_secs(4))
    })
    .await;
    let healed = match healed {
        Ok(Ok(healed)) => healed,
        Ok(Err(error)) => {
            println!("[her] ⚠️ token 自愈失败（{reason}）：{error}");
            recovery_log::record(context, Trigger::Recovery, Component::Snowluma, "token-heal", Outcome::Failed, &format!("{reason}: {error}"));
            return false;
        }
        Err(error) => {
            println!("[her] ⚠️ token 自愈异常：{error}");
            return false;
        }
    };
    let ws_changed = *ws != healed.ws_url || token.as_deref() != Some(healed.ws_token.as_str());
    *ws = healed.ws_url.clone();
    *token = Some(healed.ws_token.clone());
    *http_token = Some(healed.http_token.clone());
    if let Ok(fresh) = context.load_config() {
        *cfg_raw = fresh;
    }
    // Field names only: a token (even a prefix) is a credential and must not reach logs or the record.
    println!("[her] 🔄 SnowLuma 自愈完成（{reason}）：已更新 {}", if healed.persisted.is_empty() { "无需改写 config.json".to_string() } else { healed.persisted.join(" / ") });
    recovery_log::record(
        context,
        Trigger::Recovery,
        Component::Snowluma,
        "token-heal",
        Outcome::Ok,
        &format!("{reason}: connection settings refreshed; config.json fields written: {}", if healed.persisted.is_empty() { "none".to_string() } else { healed.persisted.join(", ") }),
    );
    ws_changed
}

// ---- crash marker (frozen JS `last-crash.json`) ---------------------------------------------------------------

const CRASH_FILE: &str = "last-crash.json";

/// Leave `<data>/last-crash.json` (`{time, message}`) when a thread panics, so the next start can tell the administrator.
/// A panic in a task the runtime survives leaves it too; it is reported once on the next start.
pub fn install_crash_marker(context: &RuntimeContext) {
    if !recovery_log::enabled(context) {
        return;
    }
    let context = context.clone();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_string());
        let location = info.location().map(|l| format!(" ({}:{})", l.file(), l.line())).unwrap_or_default();
        let known = crate::activity::known_session_tokens(&context);
        let safe = crate::activity::mask_log_text(&format!("{message}{location}"), &known);
        let marker = json!({
            "time": crate::parity_clock::now_ms(),
            "message": crate::social_mem::truncate_text(&safe, 300),
        });
        let _ = context.write_atomic(RuntimeArea::Data, CRASH_FILE, marker.to_string().as_bytes());
        // The default hook formats the original payload directly to stderr, bypassing
        // diagnostic macros. Render its useful context with the same complete-text mask.
        let thread = std::thread::current();
        let diagnostic = format!("thread '{}' panicked at{location}:\n{message}", thread.name().unwrap_or("<unnamed>"));
        crate::safe_eprintln!("{}", crate::activity::mask_log_text(&diagnostic, &known));
        if std::env::var("RUST_BACKTRACE").is_ok_and(|value| value != "0") {
            crate::safe_eprintln!("{}", crate::activity::mask_log_text(&std::backtrace::Backtrace::force_capture().to_string(), &known));
        }
    }));
}

/// JS `toLocaleString('zh-CN')`: `2026/10/30 18:30:12` in the host's zone (UTC+8 unless `TZ` is UTC).
fn zh_cn_local(ms: i64) -> String {
    zh_cn_at_offset(ms, crate::tasks::local_offset_ms())
}

fn zh_cn_at_offset(ms: i64, offset_ms: i64) -> String {
    let local = ms + offset_ms;
    let days = local.div_euclid(86_400_000);
    let secs = local.rem_euclid(86_400_000) / 1000;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y}/{m}/{d} {:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// JS `reportLastCrash`: if the last run left a crash marker, delete it and tell the administrator
/// (`上次运行崩了（<time>）：<message>`, dedupe key `crash`).
pub fn report_last_crash(context: &RuntimeContext, cfg: &Value) {
    let Ok(Some(text)) = context.read_optional_text(RuntimeArea::Data, CRASH_FILE) else { return };
    let _ = context.remove_file(RuntimeArea::Data, CRASH_FILE);
    let Ok(info) = serde_json::from_str::<Value>(&text) else { return };
    let when = info.get("time").and_then(Value::as_i64).map(zh_cn_local).unwrap_or_else(|| "未知时间".to_string());
    let message = info.get("message").and_then(Value::as_str).unwrap_or("");
    recovery_log::record(context, Trigger::Startup, Component::Process, "last-run-crashed", Outcome::Failed, message);
    crate::admin_alert::notify_owner(context, cfg, &format!("上次运行崩了（{when}）：{message}"), "crash");
}

/// Only newly interrupted events reach this startup notice. Use ledger evidence for later
/// turns, excluding the interrupted event's own turn; never include message text.
pub fn report_interrupted(
    context: &RuntimeContext,
    cfg: &Value,
    events: &[crate::inbound::InterruptedNotice],
) {
    if events.is_empty() {
        return;
    }
    let turns = crate::turn_ledger::read_records(context)
        .ok()
        .map(|records| crate::turn_ledger::fold(&records).0)
        .unwrap_or_default();
    let mut lines: Vec<String> = events
        .iter()
        .take(GAP_NOTICE_LISTED)
        .map(|event| {
            let later = turns.iter().any(|turn| {
                turn.key == event.conversation
                    && turn.begun_at > event.received_ms
                    && !turn.inbound.contains(&event.event_key)
            });
            format!(
                "{} {}{}",
                event.conversation,
                zh_cn_local(event.event_ms),
                if later {
                    "（该会话已有后续回合）"
                } else {
                    ""
                }
            )
        })
        .collect();
    let text = loop {
        let remainder = events.len() - lines.len();
        let extra = if remainder > 0 {
            format!("\n另有 {remainder} 条未列出")
        } else {
            String::new()
        };
        let text = format!("上次运行中断的事件 {} 条：\n{}{}\n若已获回复，无需操作；否则请重发。详见管理端 /api/inbound/gaps",
            events.len(), lines.join("\n"), extra);
        if lines.len() <= 1
            || crate::activity::redact(context, &text)
                .encode_utf16()
                .count()
                <= crate::admin_alert::BODY_MAX_CHARS as usize
        {
            break text;
        }
        lines.pop();
    };
    recovery_log::record(
        context,
        Trigger::Startup,
        Component::Queue,
        "interrupted-events",
        Outcome::Unknown,
        &text,
    );
    crate::admin_alert::notify_notice(context, cfg, &text, "interrupted-events");
}

/// Most conversations listed by name in one notice; the count always covers all windows.
const GAP_NOTICE_LISTED: usize = 10;

/// Preserve first-seen order, including windows interleaved with other conversations.
fn grouped_gaps(gaps: &[crate::inbound::Gap]) -> Vec<Vec<&crate::inbound::Gap>> {
    let mut groups: Vec<Vec<&crate::inbound::Gap>> = Vec::new();
    let mut positions = std::collections::HashMap::new();
    for gap in gaps {
        let next = groups.len();
        let index = *positions.entry(gap.conversation.as_str()).or_insert(next);
        if index == groups.len() {
            groups.push(Vec::new());
        }
        groups[index].push(gap);
    }
    groups
}

fn gap_line(gaps: &[&crate::inbound::Gap], offset_ms: i64, owner: Option<i64>) -> String {
    let first = gaps[0];
    let from = gaps
        .iter()
        .map(|g| g.from_ms)
        .min()
        .unwrap_or(first.from_ms);
    let to = gaps.iter().map(|g| g.to_ms).max().unwrap_or(first.to_ms);
    let recovered = gaps.iter().filter(|g| g.status == "recovered").count();
    let unrecovered = gaps.iter().filter(|g| g.status == "unrecovered").count();
    let retrying = gaps
        .iter()
        .filter(|g| {
            matches!(g.status.as_str(), "open" | "unknown")
                && g.attempts < crate::inbound::MAX_GAP_ATTEMPTS
        })
        .count();
    let mut outcomes = Vec::new();
    if recovered > 0 {
        outcomes.push(format!("已补收 {recovered}"));
    }
    if retrying > 0 {
        outcomes.push(format!("待重试 {retrying}"));
    }
    if unrecovered > 0 {
        outcomes.push(format!("无法补收 {unrecovered}"));
    }
    let capped = gaps.len() - recovered - retrying - unrecovered;
    if capped > 0 {
        outcomes.push(format!("待核查 {capped}（重试已达上限）"));
    }
    if gaps
        .iter()
        .any(|g| g.reason == "private-history-unverified")
    {
        outcomes.push("私聊无历史，消息可能漏收，重要内容请重发".into());
        if owner.is_some_and(|id| first.conversation == format!("private:{id}")) {
            outcomes.push("这是你与机器人的私聊".into());
        }
    } else if unrecovered > 0 {
        outcomes.push("重要内容请重发".into());
    } else if retrying > 0 {
        outcomes.push("退避到期后下次重连时重试".into());
    } else if capped == 0 {
        outcomes.push("仅存档，需回复请重发".into());
    }
    format!(
        "{}：{} 个，{}–{}；{}",
        first.conversation,
        gaps.len(),
        zh_cn_at_offset(from, offset_ms),
        zh_cn_at_offset(to, offset_ms),
        outcomes.join("，")
    )
}

fn gap_notice_with_lines(total: usize, lines: &[String], unlisted: usize) -> String {
    let mut parts: Vec<String> = lines.to_vec();
    if unlisted > 0 {
        parts.push(format!("另有 {unlisted} 个未列出"));
    }
    format!(
        "入站缺口 {total} 个：\n{}\n详见管理端 /api/inbound/gaps",
        parts.join("\n")
    )
}

/// Each line covers every unannounced window of one conversation. Fit after redaction,
/// keeping the true remainder count and console pointer within the alert body budget.
/// Return the number of listed conversations, whose windows may be interleaved in `gaps`.
fn gap_notice(
    gaps: &[crate::inbound::Gap],
    offset_ms: i64,
    owner: Option<i64>,
    redact: &dyn Fn(&str) -> String,
) -> (String, usize) {
    let groups = grouped_gaps(gaps);
    let lines: Vec<String> = groups
        .iter()
        .take(GAP_NOTICE_LISTED)
        .map(|g| gap_line(g, offset_ms, owner))
        .collect();
    let fits = |text: &str| {
        redact(text).encode_utf16().count() <= crate::admin_alert::BODY_MAX_CHARS as usize
    };
    for listed in (1..=lines.len()).rev() {
        let shown: usize = groups[..listed].iter().map(Vec::len).sum();
        let text = gap_notice_with_lines(gaps.len(), &lines[..listed], gaps.len() - shown);
        if fits(&text) {
            return (text, listed);
        }
    }
    let mut keep = lines[0].encode_utf16().count();
    loop {
        let cut = crate::social_mem::truncate_text(&lines[0], keep as i64);
        let text = gap_notice_with_lines(gaps.len(), &[cut], gaps.len() - groups[0].len());
        if keep == 0 || fits(&text) {
            return (text, 1);
        }
        keep -= keep / 8 + 1;
    }
}

/// Claim the listed windows before queue admission. Only the outbox worker can
/// mark confirmed delivery; refusal returns them to pending, uncertainty suppresses
/// automatic resend. Opening and recovery callbacks share these durable claims.
pub fn report_unresolved_gaps(context: &RuntimeContext, cfg: &Value, dedupe_key: &str) {
    report_gaps_with(
        context,
        crate::admin_alert::owner_id(cfg),
        &|text| crate::activity::redact(context, text),
        &|text, ids| crate::admin_alert::notify_gap_notice(context, cfg, text, dedupe_key, ids),
    );
}

fn report_gaps_with(context: &RuntimeContext, owner: Option<i64>, redact: &dyn Fn(&str) -> String, notify: &dyn Fn(&str, &[i64]) -> bool) {
    let journal = match crate::inbound::Journal::open(context) {
        Ok(journal) => journal,
        Err(error) => {
            crate::safe_println!("[her] ❌ 缺口通知读库失败：{error}");
            return;
        }
    };
    let gaps = match journal.unnotified_gaps() {
        Ok(gaps) if !gaps.is_empty() => gaps,
        Ok(_) => return,
        Err(error) => {
            crate::safe_println!("[her] ❌ 缺口通知读库失败：{error}");
            return;
        }
    };
    let (text, listed) = gap_notice(&gaps, crate::tasks::local_offset_ms(), owner, redact);
    recovery_log::record(context, Trigger::Recovery, Component::Queue, "gap-unresolved", Outcome::Unknown, &text);
    let ids: Vec<i64> = grouped_gaps(&gaps).into_iter().take(listed).flatten().map(|g| g.id).collect();
    match journal.claim_gap_notice(&ids) {
        Ok(true) => {
            if !notify(&text, &ids) {
                if let Err(error) = journal.release_gap_notice(&ids, false) {
                    crate::safe_println!("[her] ❌ 缺口通知释放失败：{error}");
                }
            }
        }
        Ok(false) => {} // A concurrent opening/recovery notice already represents these windows.
        Err(error) => crate::safe_println!("[her] ❌ 缺口通知认领失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotfix128_panic_diagnostics_and_marker_mask_the_complete_message() {
        const CHILD: &str = "HER_128_PANIC_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let context = RuntimeContext::from_root(std::path::Path::new(&root)).unwrap();
            install_crash_marker(&context);
            let message = format!("{}K128session\r\nVALUExyz shape 0123456789abcdef0123456789abcdef0123456789abcdef", "x ".repeat(145));
            assert!(std::panic::catch_unwind(|| panic!("{message}")).is_err());
            return;
        }
        let root = gap_root("panic-128");
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:1":{"agentToken":"K128sessionVALUExyz"}}}"#).unwrap();
        let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
            .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
            .env(CHILD, &root).env_remove("RUST_BACKTRACE").output().unwrap();
        let marker = std::fs::read_to_string(root.join("data/last-crash.json")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(output.status.success());
        for text in [String::from_utf8_lossy(&output.stderr).into_owned(), marker] {
            assert!(!text.contains("K128session") && !text.contains("VALUExyz") && !text.contains("0123456789abcdef"), "panic leaked: {text}");
            assert!(text.contains("***"), "{text}");
        }
    }

    #[test]
    fn the_crash_time_reads_like_javascript_zh_cn_local_time() {
        // 2026-10-30 10:30:12 UTC is 18:30:12 at UTC+8.
        let utc_ms = 1_793_356_212_000;
        let plus8 = 8 * 3_600_000;
        assert_eq!(zh_cn_at_offset(utc_ms, plus8), "2026/10/30 18:30:12");
        assert_eq!(zh_cn_at_offset(utc_ms - 10 * 24 * 3_600_000 - 8 * 3_600_000, plus8), "2026/10/20 10:30:12");
        assert_eq!(zh_cn_at_offset(1_767_214_803_000, plus8), "2026/1/1 05:00:03", "no zero padding of day or month");
        assert_eq!(zh_cn_at_offset(utc_ms, 0), "2026/10/30 10:30:12");
    }

    fn gap(id: i64, conversation: &str, from_ms: i64, to_ms: i64, status: &str, reason: &str) -> crate::inbound::Gap {
        crate::inbound::Gap {
            id,
            conversation: conversation.to_string(),
            from_ms,
            to_ms,
            status: status.to_string(),
            reason: reason.to_string(),
            detail: "SECRET-DETAIL".to_string(),
            recovered: 0,
            opened_ms: to_ms,
            updated_ms: to_ms,
            attempts: 1,
            next_retry_ms: 0,
            notified_ms: 0,
            notice_state: "pending".into(),
        }
    }

    #[test]
    fn the_gap_notice_lists_conversations_with_interval_count_and_action() {
        let plus8 = 8 * 3_600_000;
        let from = 1_793_356_212_000;
        let gaps = vec![
            gap(1, "private:7001", from, from + 300_000, "unrecovered", "private-history-unverified"),
            gap(2, "group:42", from, from + 60_000, "unknown", "history-empty"),
        ];
        let (text, listed) = gap_notice(&gaps, plus8, None, &|t| t.to_string());
        assert_eq!(listed, 2);
        assert!(text.contains("入站缺口 2 个"), "{text}");
        assert!(text.contains("private:7001"), "{text}");
        assert!(text.contains("2026/10/30 18:30:12"), "interval start: {text}");
        assert!(text.contains("2026/10/30 18:35:12"), "interval end: {text}");
        assert!(text.contains("私聊无历史") && text.contains("待重试 1"), "{text}");
        assert!(text.contains("group:42"), "{text}");
        assert!(text.contains("/api/inbound/gaps"), "{text}");
        assert!(!text.contains("SECRET-DETAIL"), "gap detail stays in the console: {text}");
        assert!(text.contains("无法补收") && text.contains("可能漏收"), "unverified history is not claimed as recovered: {text}");
    }

    #[test]
    fn a_long_gap_list_is_cut_but_the_count_stays_whole() {
        let gaps: Vec<_> = (0..14).map(|i| gap(i, &format!("private:{i}"), 0, 1000, "unrecovered", "private-history-unverified")).collect();
        let (text, listed) = gap_notice(&gaps, 0, None, &|t| t.to_string());
        assert!(text.contains("入站缺口 14 个"), "{text}");
        assert!(listed > 0 && listed <= GAP_NOTICE_LISTED, "{listed}");
        for i in 0..14 {
            assert_eq!(text.contains(&format!("private:{i}：")), i < listed, "gap {i} listed={listed}: {text}");
        }
        assert!(text.contains(&format!("另有 {} 个未列出", 14 - listed)), "{text}");
    }

    fn long_key_gaps(count: usize) -> Vec<crate::inbound::Gap> {
        (0..count)
            .map(|i| gap(i as i64 + 1, &format!("group:g{i:02}-REDACTME-{}", "x".repeat(40)), 0, 1000, "unrecovered", "private-history-unverified"))
            .collect()
    }

    /// A redactor that makes the text longer, like a path or token replaced by a longer placeholder.
    fn expanding_redact(text: &str) -> String {
        text.replace("REDACTME", "[redacted-secret-value]")
    }

    #[test]
    fn many_long_gaps_still_fit_the_alert_body_after_redaction_and_end_with_the_remainder_and_pointer() {
        let gaps = long_key_gaps(20);
        let (text, listed) = gap_notice(&gaps, 0, None, &expanding_redact);
        let body = crate::admin_alert::prepare_body(&text, expanding_redact);
        assert!(body.encode_utf16().count() <= 800, "{body}");
        assert!(body.contains("入站缺口 20 个"), "{body}");
        assert!(body.ends_with("详见管理端 /api/inbound/gaps"), "the pointer survives the cut: {body}");
        assert!(body.contains(&format!("另有 {} 个未列出", 20 - listed)), "the remainder carries the true count: {body}");
        assert!(listed >= 1 && listed < GAP_NOTICE_LISTED, "{listed}");
        for i in 0..20 {
            assert_eq!(body.contains(&format!("group:g{i:02}-")), i < listed, "gap {i} listed={listed}: {body}");
        }
    }

    #[test]
    fn a_single_gap_line_longer_than_the_budget_is_shortened_so_the_notice_still_lists_it() {
        let gaps = vec![gap(1, &format!("group:{}", "y".repeat(2000)), 0, 1000, "unknown", "history-empty"), gap(2, "group:2", 0, 1, "unknown", "history-empty")];
        let (text, listed) = gap_notice(&gaps, 0, None, &|t| t.to_string());
        assert_eq!(listed, 1);
        assert!(text.encode_utf16().count() <= 800 && text.contains("另有 1 个未列出") && text.ends_with("/api/inbound/gaps"), "{text}");
    }

    fn gap_root(tag: &str) -> std::path::PathBuf {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-gap-notice-{tag}-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        root
    }

    fn journal_with_unnotified_gaps(context: &RuntimeContext, count: usize) -> Vec<i64> {
        let journal = crate::inbound::Journal::open(context).unwrap();
        (0..count)
            .map(|i| {
                let id = journal.open_gap(&format!("group:g{i:02}-REDACTME-{}", "x".repeat(40)), 0, 1000, 1).unwrap();
                journal.set_gap_result(id, crate::inbound::GapStatus::Unrecovered, "private-history-unverified", "d", 0, 1).unwrap();
                id
            })
            .collect()
    }

    #[test]
    fn only_the_gaps_the_accepted_notice_lists_are_marked_notified() {
        let root = gap_root("marks");
        let context = RuntimeContext::from_root(&root).unwrap();
        let ids = journal_with_unnotified_gaps(&context, 20);
        let sent = std::cell::RefCell::new(String::new());
        report_gaps_with(&context, None, &expanding_redact, &|text, ids| {
            crate::inbound::Journal::open(&context).unwrap().mark_gaps_notified(ids, 1).unwrap();
            *sent.borrow_mut() = crate::admin_alert::prepare_body(text, expanding_redact);
            true
        });
        let body = sent.into_inner();
        let listed = (0..20).filter(|i| body.contains(&format!("group:g{i:02}-"))).count();
        assert!(listed >= 1 && listed < 20, "{body}");
        let journal = crate::inbound::Journal::open(&context).unwrap();
        let left: Vec<i64> = journal.unnotified_gaps().unwrap().iter().map(|g| g.id).collect();
        assert_eq!(left, ids[listed..].to_vec(), "exactly the unlisted gaps stay unnotified; body: {body}");
        // The next notice lists the rest and says how many more remain, if any.
        report_gaps_with(&context, None, &expanding_redact, &|_, ids| { crate::inbound::Journal::open(&context).unwrap().mark_gaps_notified(ids, 1).unwrap(); true });
        let after = journal.unnotified_gaps().unwrap().len();
        assert!(after < left.len(), "the next notice makes progress: {after} of {}", left.len());
        drop(journal);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_refused_notice_leaves_every_gap_unnotified() {
        let root = gap_root("refused");
        let context = RuntimeContext::from_root(&root).unwrap();
        let ids = journal_with_unnotified_gaps(&context, 3);
        report_gaps_with(&context, None, &|t| t.to_string(), &|_, _| false);
        let journal = crate::inbound::Journal::open(&context).unwrap();
        assert_eq!(journal.unnotified_gaps().unwrap().iter().map(|g| g.id).collect::<Vec<_>>(), ids);
        // The real path: without ownerQQ the alert outbox refuses, so nothing is marked either.
        report_unresolved_gaps(&context, &json!({}), "inbound-gap");
        assert_eq!(journal.unnotified_gaps().unwrap().len(), 3);
        drop(journal);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hotfix127_fix2_worker_uncertain_send_is_never_repeated() {
        use std::io::{BufRead, Read, Write};
        use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

        const RESTART_ROOT: &str = "HER_127_UNCERTAIN_NOTICE_RESTART_ROOT";
        let assert_unknown = |context: &RuntimeContext| {
            let journal = crate::inbound::Journal::open(context).unwrap();
            let gaps = journal.gaps().unwrap();
            assert_eq!(gaps.len(), 1);
            assert_eq!(gaps[0].conversation, "private:1");
            assert_eq!(gaps[0].notice_state, "unknown", "an uncertain send must retain its claim");
            assert_eq!(gaps[0].notified_ms, 0, "a lost response is not confirmed delivery");
        };
        let recover = |context: &RuntimeContext| {
            for _ in 0..3 {
                report_gaps_with(context, Some(127), &|text| text.to_string(), &|_, _| {
                    panic!("recovery must never reoffer an uncertain window")
                });
            }
        };
        if let Some(root) = std::env::var_os(RESTART_ROOT) {
            let context = RuntimeContext::from_root(std::path::Path::new(&root)).unwrap();
            crate::inbound::Journal::open(&context).unwrap().recover_queued_gap_notices().unwrap();
            assert_unknown(&context);
            report_unresolved_gaps(&context, &json!({"ownerQQ":127}), "uncertain-after-restart");
            recover(&context);
            assert_unknown(&context);
            return;
        }

        struct Gateway {
            requests: Arc<Mutex<Vec<Value>>>,
            uncertain: Arc<AtomicBool>,
            stop: Arc<AtomicBool>,
            worker: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for Gateway {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
            }
        }
        let root = gap_root("uncertain-127");
        eprintln!("uncertain notice fixture: {}", root.display());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(root.join("dependencies.json"), json!({
            "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:1",
            "onebot":format!("http://{}", listener.local_addr().unwrap())
        }).to_string()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut gateway = Gateway {
            requests: Arc::default(), uncertain: Arc::new(AtomicBool::new(true)),
            stop: Arc::default(), worker: None,
        };
        let (requests, uncertain, stop) = (gateway.requests.clone(), gateway.uncertain.clone(), gateway.stop.clone());
        gateway.worker = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fake gateway accept: {error}"),
                };
                // Windows accepted sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /send_private_msg "), "{line}");
                let mut length = 0;
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" { break; }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") { length = value.trim().parse::<usize>().unwrap(); }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                requests.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                // The POST arrived, but the gateway closes without returning its outcome.
                if uncertain.load(Ordering::SeqCst) { continue; }
                let reply = r#"{"status":"ok","retcode":0,"data":{"message_id":127}}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        }));
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = crate::inbound::Journal::open(&context).unwrap();
        let id = journal.open_gap("private:1", 0, 1000, 1).unwrap();
        journal.set_gap_result(id, crate::inbound::GapStatus::Unrecovered, "private-history-unverified", "d", 0, 1).unwrap();
        drop(journal);
        report_unresolved_gaps(&context, &json!({"ownerQQ":127}), "uncertain-first-attempt");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let failed = std::fs::read_to_string(root.join("data/run-record.jsonl")).unwrap_or_default()
                .lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|row| row["event"] == "owner-alert-uncertain-first-attempt" && row["outcome"] == "failed");
            if failed { break; }
            assert!(std::time::Instant::now() < deadline, "notice worker did not record its failed outcome");
            std::thread::sleep(Duration::from_millis(10));
        }
        let requests = gateway.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "the gateway must receive the POST before losing its response");
        assert_eq!(requests[0]["user_id"], 127);
        assert!(requests[0]["message"][0]["data"]["text"].as_str().unwrap().contains("入站缺口 1 个"));
        assert!(requests[0]["message"][0]["data"]["text"].as_str().unwrap().contains("private:1"));
        assert_unknown(&context);
        gateway.uncertain.store(false, Ordering::SeqCst);
        for key in ["uncertain-recovery", "uncertain-duplicate-callback"] {
            report_unresolved_gaps(&context, &json!({"ownerQQ":127}), key);
            recover(&context);
        }
        let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
            .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
            .env(RESTART_ROOT, &root).output().unwrap();
        assert!(output.status.success(), "restart failed: {output:?}");
        assert_unknown(&context);
        assert_eq!(gateway.requests.lock().unwrap().len(), 1, "callbacks and restart must never repeat this window");
        drop(gateway);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_and_recovery_notices_atomically_claim_their_overlapping_windows() {
        use std::sync::{Arc, Barrier, atomic::{AtomicUsize, Ordering}};
        let root = gap_root("overlap");
        let context = RuntimeContext::from_root(&root).unwrap();
        journal_with_unnotified_gaps(&context, 1);
        let selected = Arc::new(Barrier::new(2));
        let sends = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..2).map(|_| {
            let (context, selected, sends) = (context.clone(), selected.clone(), sends.clone());
            std::thread::spawn(move || {
                let first_redaction = std::cell::Cell::new(true);
                report_gaps_with(&context, Some(8808), &|text| {
                    // Both paths have selected the same unannounced window before
                    // either can claim it. Later formatting redactions do not wait.
                    if first_redaction.replace(false) { selected.wait(); }
                    text.to_string()
                }, &|_, _| { sends.fetch_add(1, Ordering::SeqCst); true });
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(sends.load(Ordering::SeqCst), 1, "opening and recovery must share one window claim");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retrying_gap_guidance_names_the_next_reconnect() {
        let text = gap_line(&[&gap(1, "group:42", 0, 1000, "unknown", "history-empty")], 0, None);
        assert!(text.contains("下次重连"), "recovery has no timer on a stable connection: {text}");
        assert!(!text.contains("稍后自动重试"), "{text}");
    }

    #[test]
    fn a_heartbeat_carries_the_gateways_own_receive_verdict() {
        note_frame("meta_event", "heartbeat", Some(&json!({"online": true, "good": false})));
        let snap = gateway_health::snapshot_json(crate::parity_clock::now_ms());
        assert_eq!(snap["good"], false);
        assert_eq!(snap["goodSource"], "heartbeat");
        assert_eq!(snap["lastPacketKind"], "meta_event/heartbeat");
        gateway_health::with(|h| *h = gateway_health::GatewayHealth::new());
    }
}
