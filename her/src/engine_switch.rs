//! The selected runtime's per-conversation turn fence and durable engine handover.
use crate::{runtime::{RuntimeArea, RuntimeContext}, turn_engine::Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, fs::File, path::PathBuf, sync::{Mutex, OnceLock}, time::Duration};

const BUSY: &str = "runtime file is already locked";
const DRAIN_LIMIT: Duration = Duration::from_secs(180);
const HISTORY_ROWS: usize = 64;
const ROW_CHARS: usize = 2_000;
const HISTORY_CHARS: usize = 12_000;
const EMPTY_SUMMARY: &str = "没有可承接的早期对话记录。";
const CHANGED: &str = "conversation authority changed during the switch";
const NOT_ADMITTED: &str = "conversation is no longer admitted";
const UNSETTLED: &str = "conversation has an unsettled turn";
const FIELD: &str = "engineHandover";
const FIRST_RETRY_MS: i64 = 5_000;
const MAX_RETRY_MS: i64 = 600_000;
pub(crate) type TurnGates = (Option<File>, File);

#[derive(Debug)]
pub(crate) enum TurnPreparation {
    Ready(Engine),
    ParkUntil(i64),
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
struct Episode {
    from: Engine,
    to: Engine,
    epoch: String,
    mode_stamp: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Retry {
    episode: Episode,
    delay_ms: i64,
    retry_at_ms: i64,
    #[serde(default)]
    park: bool,
    #[serde(default)]
    notice_pending: bool,
}

fn retry_path(key: &str) -> String {
    format!("locks/engine-{}-retry.json", key.replace(':', "-"))
}

// If the retry file cannot be written, still enforce the delay in this process. An
// owner notice requires the durable record so a restart cannot duplicate an offer.
fn unsaved_retries() -> &'static Mutex<HashMap<PathBuf, Retry>> {
    static RETRIES: OnceLock<Mutex<HashMap<PathBuf, Retry>>> = OnceLock::new();
    RETRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_retry(context: &RuntimeContext, key: &str) -> Result<Option<Retry>, (u16, String)> {
    if let Some(root) = context.runtime_root() {
        if let Some(retry) = unsaved_retries().lock().unwrap_or_else(|p| p.into_inner()).get(&root.join(retry_path(key))).cloned() {
            return Ok(Some(retry));
        }
    }
    read_retry(context, key)
}

fn read_retry(context: &RuntimeContext, key: &str) -> Result<Option<Retry>, (u16, String)> {
    let raw = context.read_optional_text(RuntimeArea::Data, &retry_path(key))
        .map_err(|_| refused("engine switch retry state is unavailable".into()))?;
    raw.map(|raw| serde_json::from_str::<Retry>(&raw)
        .ok().filter(|retry| (FIRST_RETRY_MS..=MAX_RETRY_MS).contains(&retry.delay_ms))
        .ok_or_else(|| refused("engine switch retry state is invalid".into())))
        .transpose()
}

fn clear_retry(context: &RuntimeContext, key: &str) {
    if let Some(root) = context.runtime_root() {
        unsaved_retries().lock().unwrap_or_else(|p| p.into_inner()).remove(&root.join(retry_path(key)));
    }
    if let Ok(Some(_)) = read_retry(context, key) {
        if context.remove_file(RuntimeArea::Data, &retry_path(key)).is_err() {
            crate::safe_println!("[chat] {key} engine switch retry state could not be cleared");
        }
    }
}

fn next_delay(previous: Option<&Retry>) -> i64 {
    previous.map_or(FIRST_RETRY_MS, |retry| (retry.delay_ms * 2).min(MAX_RETRY_MS))
}

fn record_failure(context: &RuntimeContext, cfg: &Value, key: &str, mut retry: Retry, class: &str) {
    let offer_notice = std::mem::take(&mut retry.notice_pending);
    let saved = serde_json::to_vec(&retry).ok().is_some_and(|bytes| {
        context.write_atomic(RuntimeArea::Data, &retry_path(key), &bytes).is_ok()
    });
    if let Some(root) = context.runtime_root() {
        let slot = root.join(retry_path(key));
        let mut unsaved = unsaved_retries().lock().unwrap_or_else(|p| p.into_inner());
        if saved {
            unsaved.remove(&slot);
        } else {
            retry.notice_pending = offer_notice;
            unsaved.insert(slot, retry);
        }
    }
    if !saved {
        crate::safe_println!("[chat] {key} engine switch retry state could not be saved");
    } else if offer_notice {
        crate::admin_alert::notify_notice(context, cfg, &format!("[{key}] 会话引擎切换失败（{class}）"),
            &format!("engine-switch:{key}"));
    }
}

#[cfg(test)]
#[test]
fn switch_retry_state_is_scoped_to_the_conversation() {
    assert_ne!(retry_path("group:42"), retry_path("private:42"), "a group and private conversation with the same ID need separate retry state");
    assert_ne!(retry_path("group:42"), retry_path("group:43"));
}

// Order: release turn gate, conversation gate, configuration/social state gate, then the
// session, ledger or settings store gate. Access changes never take the conversation gate.
// The conversation gate spans the turn's final delivery or the whole switch. Provider and
// DSH calls run with only the release and conversation gates held.
fn try_gate(context: &RuntimeContext, key: &str) -> Result<File, String> {
    let key = crate::settings::canonical_conversation(key)
        .filter(|canonical| canonical == key).ok_or("invalid conversation key")?;
    // Keep this path stable while another process can have the file open or be waiting for it.
    context.open_locked_file(RuntimeArea::Data, &format!("locks/engine-{}.lock", key.replace(':', "-")))
}

pub(crate) async fn turn_gate(context: &RuntimeContext, key: &str) -> Result<File, String> {
    loop {
        match try_gate(context, key) {
            Err(error) if error == BUSY => tokio::time::sleep(Duration::from_millis(25)).await,
            result => return result,
        }
    }
}

pub(crate) fn drain(context: &RuntimeContext, key: &str) -> Result<(File, File), (u16, String)> {
    let release = context.open_shared_locked_file(RuntimeArea::Data, "locks/turn-update.lock")
        .map_err(|error| (503, error))?;
    let deadline = std::time::Instant::now() + DRAIN_LIMIT;
    loop {
        match try_gate(context, key) { // Wait for this conversation's admitted turn.
            Ok(gate) => return Ok((release, gate)),
            Err(error) if error == BUSY && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) if error == BUSY => return Err((409, "conversation is still draining".into())),
            Err(error) => return Err((503, error)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Pending {
    from: Engine,
    to: Engine,
    epoch: String,
    mode_stamp: String,
    session: Option<String>,
    summary: String,
    #[serde(default)]
    archive_started: bool,
}

#[derive(Default, Deserialize, Serialize)]
struct Handover {
    engine: Option<Engine>,
    epoch: Option<String>,
    summary: Option<String>,
    pending: Option<Pending>,
}

fn read_handover(conv: &Value) -> Result<Handover, String> {
    match conv.get(FIELD) {
        None | Some(Value::Null) => Ok(Handover::default()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|_| "invalid conversation handover".into()),
    }
}

fn validate_conversation(state: &Value, key: &str) -> Result<(), (u16, String)> {
    let conversations = &state["conversations"];
    let record = &conversations[key];
    if !(conversations.is_null() || conversations.is_object()) || !(record.is_null() || record.is_object()) {
        return Err((503, "invalid conversation state".into()));
    }
    Ok(())
}

fn epoch(conv: &Value) -> String {
    conv["agentToken"].as_str().map(crate::send_outbox::token_epoch).unwrap_or_default()
}

fn refused(error: String) -> (u16, String) { (503, error) }

fn admission_changed() -> (u16, String) { (409, crate::turn_engine::ADMISSION_CHANGED.into()) }

fn pending_conversation<'a>(
    state: &'a mut Value, key: &str, generation: &str, pending: &Pending,
) -> Result<&'a mut Value, (u16, String)> {
    validate_conversation(state, key)?;
    let conv = &mut state["conversations"][key];
    if epoch(conv) != generation || read_handover(conv).map_err(refused)?.pending.as_ref() != Some(pending) {
        return Err(admission_changed());
    }
    Ok(conv)
}

fn clear_pending(
    context: &RuntimeContext, key: &str, conv: &mut Value, pending: &Pending, continuing: Engine,
) -> Result<(), (u16, String)> {
    // An interrupted archive may already have succeeded. Never submit to its captured session.
    if let Some(sid) = pending.session.as_ref().filter(|_| pending.archive_started) {
        crate::session_map::retire_if(context, key, sid).map_err(refused)?;
    }
    if pending.archive_started {
        if conv["agentToken"].as_str().is_none_or(str::is_empty) {
            conv["agentToken"] = json!(crate::console::fresh_agent_token());
        }
        conv[FIELD] = json!(Handover {
            engine: Some(continuing), epoch: Some(epoch(conv)), summary: Some(pending.summary.clone()), pending: None,
        });
        return Ok(());
    }
    let same_epoch = pending.epoch == epoch(conv);
    if let Some(fields) = conv[FIELD].as_object_mut() {
        fields.remove("pending");
        if same_epoch {
            if fields.get("epoch").and_then(Value::as_str) != Some(pending.epoch.as_str())
                || fields.get("engine") != Some(&json!(pending.from)) {
                fields.remove("summary");
            }
            fields.insert("engine".into(), json!(pending.from));
            fields.insert("epoch".into(), json!(&pending.epoch));
        }
    }
    Ok(())
}

pub(crate) fn prepare_turn(context: &RuntimeContext, key: &str, mode: &str) -> Result<TurnPreparation, String> {
    prepare_admission(context, key, mode, None).map_err(|(_, error)| error)
}

pub(crate) fn prepare(context: &RuntimeContext, key: &str, mode: &str, requested: Engine) -> Result<(), (u16, String)> {
    match prepare_admission(context, key, mode, Some(requested))? {
        TurnPreparation::Ready(_) => Ok(()),
        TurnPreparation::ParkUntil(_) => Err(refused("engine switch preparation deferred".into())),
    }
}

/// Run with the conversation gate acquired. Admission calls this before opening its turn ledger row.
/// The previous turn is settled before any history or archive call. Only the final social-state
/// replacement publishes both the destination's generation and its handover.
fn prepare_admission(context: &RuntimeContext, key: &str, mode: &str, requested: Option<Engine>) -> Result<TurnPreparation, (u16, String)> {
    use TurnPreparation::{ParkUntil, Ready};
    let cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
    let wanted = Engine::selected(context, &cfg, key).map_err(refused)?;
    if requested.is_some_and(|requested| requested != wanted) {
        return Err(admission_changed());
    }
    if !wanted.supports(mode) {
        return Err((409, crate::turn_engine::UNSUPPORTED_MODE.into()));
    }
    let state = crate::console::load_social_state_for(context)?;
    validate_conversation(&state, key)?;
    let conversation = &state["conversations"][key];
    let generation = epoch(conversation);
    let mut record = read_handover(conversation).map_err(refused)?;
    let session = crate::session_map::lookup(context, key).map_err(refused);
    let published = record.engine.filter(|_| record.epoch.as_deref() == Some(generation.as_str()));
    let staged = record.pending.as_ref().filter(|pending| pending.epoch == generation).map(|pending| pending.from);
    let source = if let Some(engine) = published.or(staged) {
        engine
    } else if let Err(error) = &session {
        // A pending switch still needs its retry episode when the binding is unreadable.
        record.pending.as_ref().map(|pending| pending.from).ok_or_else(|| error.clone())?
    } else if session.as_ref().is_ok_and(Option::is_some) {
        Engine::Dsh
    } else if !generation.is_empty() && crate::harness::store::has_generation(context, key, &generation).map_err(refused)? {
        Engine::Harness
    } else {
        wanted
    };
    if record.pending.is_none() && source == wanted {
        session?;
        clear_retry(context, key);
        return Ok(Ready(wanted));
    }

    let stamp = crate::reserved::mode_stamp(context).map_err(refused);
    let automatic = requested.is_none() && matches!(mode, "chat" | "closed-agent");
    let episode = Episode {
        from: source, to: wanted, epoch: generation.clone(),
        mode_stamp: stamp.as_ref().ok().cloned()
            .or_else(|| record.pending.as_ref().map(|pending| pending.mode_stamp.clone()))
            .unwrap_or_default(),
    };
    let previous_retry = if automatic { load_retry(context, key) } else { Ok(None) };
    let retry = previous_retry.as_ref().ok().and_then(Option::as_ref).filter(|retry| retry.episode == episode);
    let cooldown = retry.filter(|retry| crate::parity_clock::now_ms() < retry.retry_at_ms);
    if let Some(retry) = cooldown.filter(|retry| record.pending.is_some() || retry.park) {
        return Ok(ParkUntil(retry.retry_at_ms));
    }

    let permitted = |current_cfg: &Value| -> Result<(), (u16, String)> {
        if !crate::settings::conversation_admitted(current_cfg, key) || !crate::console::access_allowed(current_cfg, mode, key) {
            return Err((409, NOT_ADMITTED.into()));
        }
        if crate::console::runtime_mode(context) != mode
            || crate::reserved::mode_stamp(context).map_err(refused)? != *stamp.as_ref().map_err(Clone::clone)? {
            return Err((409, CHANGED.into()));
        }
        Ok(())
    };
    let recheck_choice = |current_cfg: &Value| {
        permitted(current_cfg)?;
        if Engine::read_choice(context, current_cfg, key).map_err(refused)? != wanted {
            return Err(admission_changed());
        }
        Ok(())
    };
    let failed = |(status, error): (u16, String), can_serve: bool| {
        if !automatic || (status < 500 && error != UNSETTLED) {
            return Err((status, error));
        }
        let current_cfg = context.load_config().map_err(|error| (503, error.to_string()));
        let can_serve = match current_cfg.as_ref().map_err(Clone::clone).and_then(recheck_choice) {
            Ok(()) => can_serve,
            Err(error) if error.0 < 500 => return Err(error),
            Err(_) => false,
        };
        let delay_ms = next_delay(retry);
        let retry_at_ms = crate::parity_clock::now_ms().saturating_add(delay_ms);
        let next = Retry {
            episode: episode.clone(), delay_ms, retry_at_ms, park: !can_serve,
            notice_pending: retry.is_none_or(|retry| retry.notice_pending),
        };
        let class = if can_serve { "engine-switch-prepare" } else { "engine-switch-pending" };
        record_failure(context, current_cfg.as_ref().unwrap_or(&cfg), key, next, class);
        Ok(if can_serve { Ready(source) } else { ParkUntil(retry_at_ms) })
    };

    let (session, mode_stamp) = match (|| {
        let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(context).map_err(refused)?);
        if crate::turn_ledger::blocking_turn(&turns, key).is_some() {
            return Err((409, UNSETTLED.into()));
        }
        previous_retry.as_ref().map_err(Clone::clone)?;
        Ok((session?, stamp.as_ref().map_err(Clone::clone)?))
    })() {
        Ok(ready) => ready,
        Err(error) => return failed(error, false),
    };
    if let Err(error) = permitted(&cfg) {
        return failed(error, false);
    }
    if cooldown.is_some() {
        return Ok(Ready(source));
    }

    if let Some(pending) = record.pending.as_ref() {
        let stale = pending.epoch != generation || pending.mode_stamp != *mode_stamp
            || session.as_ref().is_some_and(|current| pending.session.as_ref() != Some(current));
        if stale || pending.to != wanted {
            let continuing = if stale { source } else { pending.from };
            let cleared = crate::console::mutate_social_state_for(context, |fresh| {
                let current_cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
                recheck_choice(&current_cfg)?;
                let conv = pending_conversation(fresh, key, &generation, pending)?;
                clear_pending(context, key, conv, pending, continuing)
            });
            if let Err(error) = cleared {
                return failed(error, false);
            }
            clear_retry(context, key);
            // Re-admit the same inbound after stale cleanup.
            return if stale { Err(admission_changed()) } else { Ok(Ready(continuing)) };
        }
    }

    let mut pending = match record.pending.take() {
        Some(pending) => pending,
        None => match (|| -> Result<Pending, (u16, String)> {
            let previous = record.summary.as_deref().filter(|_| record.epoch.as_deref() == Some(generation.as_str()));
            let rows = match source {
                Engine::Dsh => match &session {
                    Some(sid) => crate::dsh::switch_history(&crate::console::dsh_rpc_for(context, &cfg).map_err(refused)?, sid, HISTORY_ROWS).map_err(refused)?,
                    None => Vec::new(),
                },
                Engine::Harness => crate::harness::store::switch_history(context, key, &generation, HISTORY_ROWS).map_err(refused)?,
            };
            let summary = summarize(context, &cfg, previous, rows, source).map_err(refused)?;
            let pending = Pending { from: source, to: wanted, epoch: generation.clone(), mode_stamp: mode_stamp.clone(), session: session.clone(), summary, archive_started: false };
            crate::console::mutate_social_state_for(context, |fresh| {
                validate_conversation(fresh, key)?;
                let current_cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
                recheck_choice(&current_cfg)?; // Access may have changed while summarizing.
                if epoch(&fresh["conversations"][key]) != generation
                    || crate::session_map::lookup(context, key).map_err(refused)? != pending.session {
                    return Err(admission_changed());
                }
                fresh["conversations"][key][FIELD]["pending"] = json!(&pending);
                Ok(())
            })?;
            Ok(pending)
        })() {
            Ok(pending) => pending,
            Err(error) => {
                if !automatic || error.0 < 500 {
                    return Err(error);
                }
                // A failed write may have staged the switch. Serve the source only while
                // its selection, epoch and binding are still proven and no switch is staged.
                let can_serve = match (|| {
                    let current_cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
                    recheck_choice(&current_cfg)?;
                    let fresh = crate::console::load_social_state_for(context)?;
                    validate_conversation(&fresh, key)?;
                    let conv = &fresh["conversations"][key];
                    if epoch(conv) != generation
                        || crate::session_map::lookup(context, key).map_err(refused)? != session {
                        return Err(admission_changed());
                    }
                    Ok(read_handover(conv).map_err(refused)?.pending.is_none())
                })() {
                    Ok(can_serve) => can_serve,
                    Err(error) if error.0 < 500 => return Err(error),
                    Err(_) => false,
                };
                return failed(error, can_serve);
            }
        },
    };
    match finish_pending(context, key, &cfg, &generation, &mut pending, &permitted) {
        Ok(()) => {
            clear_retry(context, key); // Publication ends the retry episode.
            Ok(Ready(pending.to))
        }
        Err(error) => failed(error, false),
    }
}

fn finish_pending(
    context: &RuntimeContext, key: &str, cfg: &Value, generation: &str, pending: &mut Pending,
    permitted: impl Fn(&Value) -> Result<(), (u16, String)>,
) -> Result<(), (u16, String)> {
    if let Some(sid) = pending.session.clone() {
        let rpc = crate::console::dsh_rpc_for(context, cfg).map_err(refused)?;
        crate::console::mutate_social_state_for(context, |fresh| {
            let current_cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
            permitted(&current_cfg)?;
            if Engine::read_choice(context, &current_cfg, key).map_err(refused)? != pending.to {
                return Err(admission_changed());
            }
            let conv = pending_conversation(fresh, key, generation, pending)?;
            if crate::session_map::lookup(context, key).map_err(refused)?.is_some_and(|current| current != sid) {
                return Err(admission_changed());
            }
            pending.archive_started = true;
            conv[FIELD]["pending"] = json!(&*pending);
            Ok(())
        })?;
        crate::dsh::archive_for_switch(&rpc, &sid).map_err(refused)?;
    }

    crate::console::mutate_social_state_for(context, |fresh| {
        let current_cfg = context.load_config().map_err(|error| (503, error.to_string()))?;
        permitted(&current_cfg)?;
        if Engine::read_choice(context, &current_cfg, key).map_err(refused)? != pending.to {
            return Err(admission_changed());
        }
        let conv = pending_conversation(fresh, key, generation, pending)?;
        if let Some(current) = crate::session_map::lookup(context, key).map_err(refused)? {
            if pending.session.as_deref() != Some(current.as_str())
                || !crate::session_map::retire_if(context, key, &current).map_err(refused)? {
                return Err(admission_changed());
            }
        }
        let token = crate::console::fresh_agent_token();
        if conv["agentToken"].is_null() {
            *conv = crate::console::new_conversation(context);
        }
        conv[FIELD] = json!(Handover {
            engine: Some(pending.to), epoch: Some(crate::send_outbox::token_epoch(&token)),
            summary: Some(pending.summary.clone()), pending: None,
        });
        conv["agentToken"] = json!(token);
        conv["bootstrapSent"] = json!(false);
        Ok(())
    })
}

/// Read only a published handover of this exact destination generation.
pub(crate) fn summary(context: &RuntimeContext, key: &str, generation: &str, engine: Engine) -> Result<Option<String>, String> {
    let state = crate::console::load_social_state_for(context).map_err(|(_, error)| error)?;
    let record = read_handover(&state["conversations"][key])?;
    Ok(record.summary.filter(|_| record.engine == Some(engine) && record.epoch.as_deref() == Some(generation)))
}

/// Clip before the last model-view pass and refuse a delivery copy that still holds a denied term.
pub(crate) fn safe_text(context: &RuntimeContext, cfg: &Value, text: &str, max: usize) -> Option<String> {
    let known = handover_credentials(context).ok()?;
    filtered_text(context, cfg, text, max, &known)
}

fn handover_credentials(context: &RuntimeContext) -> Result<Vec<String>, String> {
    let mut known = crate::activity::known_session_tokens(context);
    known.extend(crate::settings::harness_credential(context));
    known.extend(crate::settings::stored_secrets(context)?);
    Ok(known)
}

fn filtered_text(context: &RuntimeContext, cfg: &Value, text: &str, max: usize, known: &[String]) -> Option<String> {
    let masked = crate::activity::mask_log_text(text, known);
    let limited: String = masked.chars().take(max).collect();
    let filtered = crate::model_view::strip_text_with_runtime(context, &limited, cfg);
    let filtered = crate::activity::mask_log_text(&filtered, known);
    let filtered = filtered.trim();
    (!filtered.is_empty() && crate::model_text_policy::denied_terms(filtered).is_empty()).then(|| filtered.to_owned())
}

fn history_row(context: &RuntimeContext, cfg: &Value, row: &str, max: usize, source: Engine, known: &[String]) -> Option<String> {
    const USER: &str = "〔对方〕";
    let masked = crate::activity::mask_log_text(row, known);
    let limited = if let Some(text) = masked.strip_prefix(USER).filter(|_| source == Engine::Dsh) {
        let keep = max.checked_sub(USER.chars().count())?.checked_sub(1)?;
        if keep == 0 { return None; }
        let start = text.char_indices().rev().nth(keep - 1).map_or(0, |(at, _)| at);
        format!("{USER} {}", &text[start..])
    } else {
        masked.chars().take(max).collect()
    };
    let filtered = crate::model_view::strip_text_with_runtime(context, &limited, cfg);
    let cleaned = crate::model_text_policy::remove_denied_spans(&filtered);
    let cleaned = cleaned.trim();
    (!cleaned.is_empty() && cleaned != USER && cleaned != "〔我〕"
        && crate::model_text_policy::denied_terms(cleaned).is_empty()).then(|| cleaned.to_owned())
}

fn transcript(context: &RuntimeContext, cfg: &Value, previous: Option<&str>, rows: Vec<String>, source: Engine, known: &[String]) -> String {
    let mut kept = Vec::new();
    let mut remaining = HISTORY_CHARS;
    for row in rows.into_iter().rev().take(HISTORY_ROWS) {
        let Some(line) = history_row(context, cfg, &row, ROW_CHARS.min(remaining.saturating_sub(1)), source, known) else { continue };
        if line.chars().count() >= remaining { break; }
        remaining -= line.chars().count() + 1;
        kept.push(line);
    }
    kept.reverse();
    if let Some(previous) = previous.and_then(|text| filtered_text(context, cfg, text, ROW_CHARS.min(remaining), known)) {
        if previous.chars().count() < remaining {
            kept.insert(0, previous);
        }
    }
    kept.join("\n")
}

fn summarize(context: &RuntimeContext, cfg: &Value, previous: Option<&str>, rows: Vec<String>, source: Engine) -> Result<String, String> {
    let known = handover_credentials(context)?;
    let data = transcript(context, cfg, previous, rows, source, &known);
    if data.is_empty() { return Ok(EMPTY_SUMMARY.into()); }
    let failure = |class| format!("conversation handover summary failed ({class})");
    // The joined delivery copy gets the last filter, including token and denied-term checks.
    let data = filtered_text(context, cfg, &data, HISTORY_CHARS, &known).ok_or_else(|| failure("wording"))?;
    let model = crate::digest::model_for(context, cfg).map_err(|error| failure(error.0))?;
    let answer = model.complete(crate::harness::handover::SUMMARY_PROMPT, &data).map_err(|error| failure(error.0))?;
    let accepted = crate::harness::handover::accept(&data, &answer).map_err(failure)?;
    filtered_text(context, cfg, &accepted, ROW_CHARS, &known).ok_or_else(|| failure("wording"))
}
