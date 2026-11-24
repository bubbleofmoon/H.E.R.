//! Transactional configuration, live-setting, access-revocation, and audit-diff commits.
//!
//! Lock order (outermost to innermost):
//! 1. `locks/restore-writers.lock` (shared, taken by `memory_actions` before entering `config_tx`
//!    for chat settings writes; exclusive during `restore`)
//! 2. `console_auth::credential_mutation_gate` (for `/api/console/token` rotation)
//! 3. `closed_agent::lock` (`locks/closed-agent.lock`, for access changes and audit undo)
//! 4. `config_gate` (`locks/social-v2.lock`, via `conversation_state::acquire_config_gate` or
//!    `social_state::acquire_gate`)
//! 5. `store_gate` (`locks/settings-store.lock`, acquired inside `settings::*` writes and released
//!    before `config.json` publication or `social-v2.json` wake reconciliation)

use crate::conversation_state::{ConversationState, Gate};
use crate::runtime::{RuntimeArea, RuntimeContext};
use crate::social_state::{
    self, begin_access_change, canonical_key, config_write_error, default_stopper,
    finish_access_change, finish_access_change_with, finish_pending_before_change,
    finish_pending_before_change_with, forget_access_change, js_id_list, js_string, js_string_or,
    legacy_write_json_atomic, live_restart, normalize_owner, normalized_id, policy_list,
    raw_config_for_write, raw_config_object, reconcile_default_wake_configs, SessionStopper,
    SocialError, StopTracker, LEGACY_CONFIG_PATH,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Committed `config.json` before/after snapshots returned directly for request audit recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigDiff {
    pub(crate) before: String,
    pub(crate) after: String,
}

impl ConfigDiff {
    pub(crate) fn merge(existing: Option<Self>, next: Self) -> Self {
        match existing {
            Some(mut prev) => {
                prev.after = next.after;
                prev
            }
            None => next,
        }
    }
}

pub(crate) fn merge_diffs(
    first: Option<ConfigDiff>,
    second: Option<ConfigDiff>,
) -> Option<ConfigDiff> {
    match (first, second) {
        (None, other) => other,
        (other, None) => other,
        (Some(first), Some(second)) => Some(ConfigDiff::merge(Some(first), second)),
    }
}

/// Successful transactional commit value together with any published `config.json` audit diff.
#[derive(Clone, Debug)]
pub(crate) struct CommitOutcome<T> {
    pub(crate) value: T,
    pub(crate) config_diff: Option<ConfigDiff>,
}

/// Transactional commit error, retaining any already-published `config.json` diff for audit.
#[derive(Clone, Debug)]
pub(crate) struct CommitError {
    pub(crate) status: u16,
    pub(crate) message: String,
    pub(crate) config_diff: Option<ConfigDiff>,
}

impl CommitError {
    pub(crate) fn plain(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            config_diff: None,
        }
    }

    pub(crate) fn with_diff(
        status: u16,
        message: impl Into<String>,
        config_diff: Option<ConfigDiff>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            config_diff,
        }
    }

    pub(crate) fn from_social(error: SocialError) -> Self {
        let (status, message) = error.into_http();
        Self::plain(status, message)
    }

    pub(crate) fn into_http(self) -> (u16, String) {
        (self.status, self.message)
    }
}

impl From<(u16, String)> for CommitError {
    fn from((status, message): (u16, String)) -> Self {
        Self::plain(status, message)
    }
}

impl From<SocialError> for CommitError {
    fn from(error: SocialError) -> Self {
        Self::from_social(error)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ConfigBase {
    Canonical,
    RawObject,
}

pub(crate) fn acquire_config_gate(
    context: &RuntimeContext,
) -> Result<Option<Gate>, (u16, String)> {
    crate::conversation_state::acquire_config_gate(context)
}

pub(crate) fn acquire_gate(context: &RuntimeContext) -> Result<Gate, SocialError> {
    social_state::acquire_gate(context)
}

fn read_config_before(context: &RuntimeContext) -> Result<String, CommitError> {
    context
        .read_optional_text(RuntimeArea::Config, "config.json")
        .map(Option::unwrap_or_default)
        .map_err(|e| {
            CommitError::plain(
                503,
                format!("cannot read config.json before the write: {e}"),
            )
        })
}

fn legacy_read_config_object() -> Result<Value, (u16, String)> {
    let text = std::fs::read_to_string(LEGACY_CONFIG_PATH)
        .map_err(|_| (500, "读不到 config.json".to_string()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| (500, "config.json 不是合法 JSON，已拒绝写入".to_string()))?;
    if !value.is_object() {
        return Err((500, "config.json 不是合法 JSON，已拒绝写入".to_string()));
    }
    Ok(value)
}

fn read_raw_for_edit(context: &RuntimeContext, base: ConfigBase) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return legacy_read_config_object();
    }
    match base {
        ConfigBase::Canonical => raw_config_for_write(context).map_err(SocialError::into_http),
        ConfigBase::RawObject => {
            let text = context
                .read_text(RuntimeArea::Config, "config.json")
                .map_err(|_| (500, "读不到 config.json".to_string()))?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|_| (500, "config.json 不是合法 JSON，已拒绝写入".to_string()))?;
            if !value.is_object() {
                return Err((500, "config.json 不是合法 JSON，已拒绝写入".to_string()));
            }
            Ok(value)
        }
    }
}

/// Sole product publication path for selected `config.json` writes.
pub(crate) fn write_selected_config_text(
    context: &RuntimeContext,
    text: &str,
) -> Result<ConfigDiff, String> {
    let mut doc: Value = serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
        .map_err(|_| "runtime config is invalid".to_string())?;
    let kept = if context.is_isolated() {
        let current = context
            .read_optional_text(RuntimeArea::Config, "config.json")
            .ok()
            .flatten()
            .and_then(|raw| {
                serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')).ok()
            })
            .unwrap_or(Value::Null);
        crate::settings::keep_file_members(&current, &mut doc)
            .map_err(crate::settings::direct_change_error)?
    } else {
        false
    };
    let mirrored = crate::settings::mirror_into(context, &mut doc)?;
    let backup = doc.get("backup").is_some();
    if backup {
        doc["backup"] = crate::config::backup_config(&doc)?;
    }
    let normalized = if backup || mirrored || kept {
        Some(serde_json::to_string_pretty(&doc).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let text = normalized.as_deref().unwrap_or(text);
    let before = context
        .read_optional_text(RuntimeArea::Config, "config.json")
        .map(Option::unwrap_or_default)
        .map_err(|e| format!("cannot read config.json before the write: {e}"))?;
    context.write_atomic(RuntimeArea::Config, "config.json", text.as_bytes())?;
    Ok(ConfigDiff {
        before,
        after: text.to_string(),
    })
}

pub(crate) fn write_config_json(
    context: &RuntimeContext,
    value: &Value,
) -> Result<Option<ConfigDiff>, String> {
    if !context.is_isolated() {
        legacy_write_json_atomic(LEGACY_CONFIG_PATH, value)?;
        return Ok(None);
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("序列化失败：{e}"))?;
    write_selected_config_text(context, &text).map(Some)
}

fn write_config_social_error(
    context: &RuntimeContext,
    value: &Value,
) -> Result<Option<ConfigDiff>, SocialError> {
    write_config_json(context, value).map_err(|e| config_write_error(context, e))
}

pub(crate) fn commit_config_edit<T>(
    context: &RuntimeContext,
    base: ConfigBase,
    edit: impl FnOnce(&mut Value) -> Result<T, (u16, String)>,
) -> Result<CommitOutcome<T>, CommitError> {
    let _gate = acquire_config_gate(context)?;
    let mut cfg = read_raw_for_edit(context, base)?;
    let value = edit(&mut cfg)?;
    let config_diff = write_config_json(context, &cfg).map_err(|error| match base {
        ConfigBase::Canonical => CommitError::from(config_write_error(context, error)),
        ConfigBase::RawObject => CommitError::plain(500, error),
    })?;
    Ok(CommitOutcome { value, config_diff })
}

pub(crate) struct PageConfigWrite<T> {
    pub(crate) file: Value,
    pub(crate) base_for_changes: Value,
    pub(crate) carried: Value,
    pub(crate) confirm: bool,
    pub(crate) check_canonical_before_store: bool,
    pub(crate) keep_stored_always: bool,
    pub(crate) prior_wake: Option<Value>,
    pub(crate) keep_error_fmt: fn(&str) -> (u16, String),
    pub(crate) isolated_write_error_503: bool,
    pub(crate) wake_error_prefix: &'static str,
    pub(crate) finish: Box<dyn FnOnce(&RuntimeContext, &Value) -> Result<T, (u16, String)>>,
}

pub(crate) fn commit_page_config<T>(
    context: &RuntimeContext,
    prepare: impl FnOnce(&RuntimeContext) -> Result<PageConfigWrite<T>, (u16, String)>,
) -> Result<CommitOutcome<T>, CommitError> {
    let _gate = acquire_config_gate(context)?;
    let pre_store_before = if context.is_isolated() {
        Some(read_config_before(context)?)
    } else {
        None
    };
    let mut plan = prepare(context)?;
    let changes =
        crate::settings::console_changes(context, &plan.base_for_changes, &plan.carried)?;
    let changed: Vec<&crate::settings::Setting> =
        changes.iter().map(|(setting, _)| *setting).collect();
    crate::settings::check_confirmation(plan.confirm, &changed)?;
    let has_changes = !changes.is_empty();
    if has_changes && plan.check_canonical_before_store && context.is_isolated() {
        raw_config_for_write(context)?;
    }
    crate::settings::apply_changes(context, changes, "administrator", "console")?;
    if context.is_isolated() && (has_changes || plan.keep_stored_always) {
        let latest = raw_config_object(context)?;
        crate::settings::keep_registry_values(&latest, &mut plan.file)
            .map_err(|key| CommitError::from((plan.keep_error_fmt)(key)))?;
    }
    let mut config_diff = write_config_json(context, &plan.file).map_err(|error| {
        if plan.isolated_write_error_503 {
            CommitError::from(config_write_error(context, error))
        } else {
            CommitError::plain(500, error)
        }
    })?;
    if let (Some(diff), Some(before)) = (&mut config_diff, pre_store_before) {
        diff.before = before;
    }
    if let Some(before_wake) = &plan.prior_wake {
        reconcile_default_wake_configs(context, Some(before_wake)).map_err(|err| {
            let (status, message) = err.into_http();
            CommitError::with_diff(
                status,
                format!("{}{message}", plan.wake_error_prefix),
                config_diff.clone(),
            )
        })?;
    }
    let value = (plan.finish)(context, &plan.file)
        .map_err(|(status, message)| CommitError::with_diff(status, message, config_diff.clone()))?;
    Ok(CommitOutcome { value, config_diff })
}

pub(crate) fn import_engine_choices(
    context: &RuntimeContext,
) -> Result<Option<ConfigDiff>, SocialError> {
    if !crate::settings::has_legacy_engine_choices(context).map_err(SocialError::from_http)? {
        return Ok(None);
    }
    let _gate = acquire_config_gate(context).map_err(SocialError::from_http)?;
    let Some(config) =
        crate::settings::import_engine_choices(context).map_err(SocialError::from_http)?
    else {
        return Ok(None);
    };
    write_selected_config_text(context, &config.to_string())
        .map(Some)
        .map_err(|_| {
            SocialError::Unavailable(
                "conversation engine import could not update config.json".to_string(),
            )
        })
}

pub(crate) fn refresh_default_wake_configs(context: &RuntimeContext) -> Result<(), SocialError> {
    if ConversationState::for_runtime(context)
        .read_document()
        .map_err(SocialError::from_http)?
        .is_empty_conversations()
    {
        return context
            .load_config_requiring_settings()
            .map(drop)
            .map_err(SocialError::Unavailable);
    }
    let _gate = acquire_config_gate(context).map_err(SocialError::from_http)?;
    reconcile_default_wake_configs(context, None)
}

pub(crate) fn finish_startup_write_back(context: &RuntimeContext) {
    if !context.is_isolated() {
        return;
    }
    if crate::settings::has_stored_values(context) {
        let result = acquire_config_gate(context)
            .and_then(|_gate| crate::settings::write_back(context).map_err(|e| (503, e)));
        match result {
            Ok(blocked) if blocked.is_empty() => {}
            Ok(blocked) => crate::safe_println!(
                "[console] ⚠️ config.json cannot hold the stored settings {} \
                 (a non-object is on the way)",
                blocked.join(", ")
            ),
            Err((_, error)) => crate::safe_println!(
                "[console] ⚠️ stored settings were not written back into config.json \
                 (the next settings change retries): {error}"
            ),
        }
    }
    if let Err(error) = refresh_default_wake_configs(context) {
        crate::safe_println!("[console] inherited wake plans could not be refreshed: {error}");
    }
}

pub(crate) fn persist_snowluma_endpoint(
    context: &RuntimeContext,
    access_token: &str,
    ws_url: Option<&str>,
    http_url: Option<&str>,
) -> Result<Vec<&'static str>, String> {
    if !context.is_isolated() {
        return Err("token self-heal writes only a selected runtime's config".to_string());
    }
    let _gate = acquire_config_gate(context).map_err(|(_, message)| message)?;
    let mut doc = raw_config_for_write(context).map_err(|e| e.message().to_string())?;
    let mut changed: Vec<&'static str> = Vec::new();
    let snowluma = doc
        .as_object_mut()
        .expect("raw_config_for_write returns an object")
        .entry("snowluma".to_string())
        .or_insert_with(|| json!({}));
    let Some(snowluma) = snowluma.as_object_mut() else {
        return Err("config.json snowluma is not an object".to_string());
    };
    for (name, value) in [
        ("accessToken", Some(access_token)),
        ("wsUrl", ws_url),
        ("httpUrl", http_url),
    ] {
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            continue;
        };
        if snowluma.get(name).and_then(Value::as_str) != Some(value) {
            snowluma.insert(name.to_string(), json!(value));
            changed.push(name);
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())? + "\n";
    let _diff = write_selected_config_text(context, &text)?;
    Ok(changed)
}

pub(crate) fn rotate_console_token(
    context: &RuntimeContext,
    token: &str,
    configured: bool,
) -> Result<ConfigDiff, CommitError> {
    let _credential = crate::console_auth::credential_mutation_gate(context)
        .map_err(|e| CommitError::plain(503, e))?;
    let _gate = acquire_gate(context)?;
    crate::console_auth::rotate_console_token(context, token, configured)
}

fn config_snapshot(context: &RuntimeContext) -> Value {
    raw_config_object(context).unwrap_or(Value::Null)
}

fn settle_failed_write(
    context: &RuntimeContext,
    unchanged: &Value,
    pending: &Option<String>,
) -> String {
    if pending.is_none() {
        return String::new();
    }
    let published = raw_config_object(context).map_or(true, |current| &current != unchanged);
    if published {
        return concat!(
            "; the configuration may have been written, so its revocation stays pending ",
            "and is finished at the next console start or access change"
        )
        .to_string();
    }
    match forget_access_change(context, pending) {
        Ok(()) => String::new(),
        Err(cleanup) => format!("; the recorded revocation stays pending: {cleanup}"),
    }
}

fn write_config_or_forget(
    context: &RuntimeContext,
    file: &Value,
    unchanged: &Value,
    pending: &Option<String>,
) -> Result<Option<ConfigDiff>, SocialError> {
    match write_config_social_error(context, file) {
        Ok(diff) => Ok(diff),
        Err(error) => {
            let status = error.http_status();
            let msg = format!("{error}{}", settle_failed_write(context, unchanged, pending));
            Err(SocialError::from_http((status, msg)))
        }
    }
}

fn store_access_changes(
    context: &RuntimeContext,
    changes: Vec<(&'static crate::settings::Setting, Value)>,
    previewed: &[(&'static crate::settings::Setting, Option<Value>)],
    pending: &Option<String>,
    source: &str,
) -> Result<Option<String>, SocialError> {
    crate::settings::apply_changes(context, changes, "administrator", source).map_err(
        |(status, error)| {
            SocialError::from_http((
                status,
                format!(
                    "{error}{}",
                    settle_failed_store_write(context, previewed, pending)
                ),
            ))
        },
    )
}

fn settle_failed_store_write(
    context: &RuntimeContext,
    previewed: &[(&'static crate::settings::Setting, Option<Value>)],
    pending: &Option<String>,
) -> String {
    if pending.is_none() {
        return String::new();
    }
    if crate::settings::holds_any(context, previewed).unwrap_or(true) {
        return concat!(
            "; the change may have been stored, so its revocation stays pending ",
            "and is finished at the next console start or access change"
        )
        .to_string();
    }
    match forget_access_change(context, pending) {
        Ok(()) => String::new(),
        Err(cleanup) => format!("; the recorded revocation stays pending: {cleanup}"),
    }
}

fn keep_stored_members(context: &RuntimeContext, file: &mut Value) -> Result<(), SocialError> {
    let current = raw_config_object(context)?;
    crate::settings::keep_registry_values(&current, file)
        .map(|_| ())
        .map_err(|key| {
            SocialError::Storage(format!(
                concat!(
                    "config.json cannot hold {}: a member on its path is not an object; ",
                    "stored settings stand"
                ),
                key
            ))
        })
}

pub(crate) fn commit_setting_write(
    context: &RuntimeContext,
    authority: crate::persona::Authority<'_>,
    commit: impl FnOnce() -> Result<Value, (u16, String)>,
) -> Result<Value, (u16, String)> {
    let _gate = authority()?;
    let mut reply = commit()?;
    if let Some(setting) = reply["setting"]
        .as_str()
        .filter(|_| reply["changed"] != false)
    {
        if crate::settings::wake_timing::refreshes_default_plan(setting)
            && reconcile_default_wake_configs(context, None).is_err()
        {
            reply["wakeRefreshError"] =
                json!("Settings saved. The default wake plans could not be refreshed.");
        }
    }
    Ok(reply)
}

pub(crate) fn commit_access_setting_change(
    context: &RuntimeContext,
    authority: crate::persona::Authority<'_>,
    change: &crate::settings::Change<'_>,
    commit: impl FnOnce() -> Result<Value, (u16, String)>,
) -> Result<Value, (u16, String)> {
    let interaction = crate::closed_agent::lock(context)
        .map_err(SocialError::Unavailable)
        .map_err(SocialError::into_http)?;
    finish_pending_before_change(context, &interaction).map_err(SocialError::into_http)?;
    let (before, after, mut reply, pending) = {
        let _gate = authority()?;
        let effective_before = context
            .load_config()
            .map_err(|e| (503, e.to_string()))?;
        let raw = raw_config_object(context).map_err(SocialError::into_http)?;
        let previewed = crate::settings::preview(context, change)?;
        let effective_after = context
            .effective_config_with(&raw, &previewed)
            .unwrap_or_else(|_| json!({}));
        let pending =
            begin_access_change(context, &effective_before, &effective_after, "access-revoked")
                .map_err(SocialError::into_http)?;
        let reply = commit().map_err(|(status, error)| {
            let msg = format!(
                "{error}{}",
                settle_failed_store_write(context, &previewed, &pending)
            );
            (status, msg)
        })?;
        (effective_before, effective_after, reply, pending)
    };
    let (revoked, failures) = finish_access_change(
        context,
        &interaction,
        &before,
        &after,
        "access-revoked",
        pending,
    );
    drop(interaction);
    if !failures.is_empty() {
        return Err((
            500,
            format!(
                "the setting was saved, but the immediate revocation did not finish: {}",
                failures.join("; ")
            ),
        ));
    }
    reply["revoked"] = json!(revoked);
    Ok(reply)
}

pub(crate) fn commit_whitelist(
    context: &RuntimeContext,
    body: &Value,
) -> Result<CommitOutcome<Value>, CommitError> {
    commit_whitelist_with(context, body, StopTracker::global(), default_stopper())
}

pub(crate) fn commit_whitelist_with(
    context: &RuntimeContext,
    body: &Value,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<CommitOutcome<Value>, CommitError> {
    let confirm = crate::settings::confirm_field(body)?;
    let interaction = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    let mut repair = context.clone();
    repair.console_authority = None;
    finish_pending_before_change_with(&repair, &interaction, tracker, stopper)?;
    let (before, after, file, pending, config_diff) = {
        let _gate = acquire_gate(context)?;
        let pre_store_before = if context.is_isolated() {
            Some(read_config_before(context)?)
        } else {
            None
        };
        let effective_before = context
            .load_config()
            .map_err(|e| SocialError::Unavailable(e.to_string()))?;
        let unchanged = raw_config_object(context)?;
        let mut file = unchanged.clone();
        crate::settings::mirror_into(context, &mut file).map_err(SocialError::Unavailable)?;
        let before = file.clone();
        let list = |root: &str, kind: &str| -> Value {
            js_id_list(body.get(root).and_then(|v| v.get(kind))).unwrap_or_else(|| {
                match before.get(root).and_then(|v| v.get(kind)) {
                    None | Some(Value::Null) => json!([]),
                    Some(value) => value.clone(),
                }
            })
        };
        let root_value = |root: &str| -> Value {
            let named = ["private", "groups"]
                .iter()
                .any(|kind| js_id_list(body.get(root).and_then(|v| v.get(*kind))).is_some());
            match before.get(root) {
                Some(configured) if !named && !configured.is_null() => configured.clone(),
                _ => json!({
                    "private": list(root, "private"),
                    "groups": list(root, "groups"),
                }),
            }
        };
        let allow = root_value("allow");
        let deny = root_value("deny");
        let object = file.as_object_mut().expect("config checked above");
        object.insert("allow".into(), allow);
        object.insert("deny".into(), deny);
        if let Some(value) = body.get("ownerQQ") {
            object.insert("ownerQQ".into(), normalize_owner(Some(value))?);
        }
        let mut changes = crate::settings::console_changes(context, &unchanged, &file)?;
        if context.is_isolated() && body.get("ownerQQ").is_some() && file["ownerQQ"].is_null() {
            let owner = crate::settings::find("ownerQQ").expect("the registry holds the owner");
            if !owner
                .read(&crate::settings::overlay(context, unchanged.clone()))
                .is_null()
            {
                changes.push((owner, Value::Null));
            }
        }
        let changed: Vec<&crate::settings::Setting> =
            changes.iter().map(|(setting, _)| *setting).collect();
        crate::settings::check_confirmation(confirm, &changed)?;
        let previewed: Vec<_> = changes
            .iter()
            .map(|(setting, value)| (*setting, Some(value.clone())))
            .collect();
        let effective_after = context
            .effective_config_with(&file, &previewed)
            .unwrap_or_else(|_| json!({}));
        let stored = !changes.is_empty();
        if stored {
            raw_config_for_write(context)?;
        }
        let pending = begin_access_change(
            context,
            &effective_before,
            &effective_after,
            "access-revoked",
        )?;
        store_access_changes(context, changes, &previewed, &pending, "console")?;
        keep_stored_members(context, &mut file)?;
        let mut diff = write_config_or_forget(
            context,
            &file,
            if stored { &Value::Null } else { &unchanged },
            &pending,
        )?;
        if let (Some(diff), Some(before_text)) = (&mut diff, pre_store_before) {
            diff.before = before_text;
        }
        let shown = context
            .load_config_document()
            .ok()
            .filter(Value::is_object)
            .unwrap_or(file);
        (effective_before, effective_after, shown, pending, diff)
    };
    let (retired, failures) = finish_access_change_with(
        context,
        &interaction,
        &before,
        &after,
        "access-revoked",
        pending,
        tracker,
        stopper,
    );
    drop(interaction);
    if !failures.is_empty() {
        return Err(CommitError::with_diff(
            500,
            format!("白名单已保存，但立即撤销未完成：{}", failures.join("；")),
            config_diff,
        ));
    }
    let ids = |kind: &str| policy_list(&file, "allow", kind).unwrap_or_default().join(",");
    let groups = ids("group");
    let private = ids("private");
    crate::safe_println!(
        "[console] 控制台：白名单已更新（群: {}，私聊: {}，管理员: {}）{}",
        if groups.is_empty() { "无" } else { &groups },
        if private.is_empty() { "无" } else { &private },
        if file["ownerQQ"].is_null() {
            "未设置".to_string()
        } else {
            js_string(&file["ownerQQ"])
        },
        if retired.is_empty() {
            String::new()
        } else {
            format!("；已立即撤销 {}", retired.join(","))
        }
    );
    Ok(CommitOutcome {
        value: json!({
            "ok": true,
            "allow": file["allow"],
            "deny": file["deny"],
            "ownerQQ": file["ownerQQ"],
        }),
        config_diff,
    })
}

pub(crate) fn commit_conversation_scope(
    context: &RuntimeContext,
    body: &Value,
) -> Result<CommitOutcome<Value>, CommitError> {
    commit_conversation_scope_with(context, body, StopTracker::global(), default_stopper())
}

pub(crate) fn commit_conversation_scope_with(
    context: &RuntimeContext,
    body: &Value,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<CommitOutcome<Value>, CommitError> {
    let confirm = crate::settings::confirm_field(body)?;
    let key = canonical_key(&js_string_or(body.get("conversation"), "")).ok_or_else(|| {
        SocialError::BadInput("conversation must be group:<id> or private:<id>".to_string())
    })?;
    let admit = match body.get("action").and_then(Value::as_str) {
        Some("admit") => true,
        Some("remove") => false,
        _ => {
            return Err(SocialError::BadInput("action must be admit or remove".to_string()).into());
        }
    };
    let (kind, id) = key.split_once(':').expect("canonical keys carry a colon");
    let (field, alias) = if kind == "group" {
        ("groups", "group")
    } else {
        ("private", "privates")
    };
    let listed_same = |value: &Value| {
        normalized_id(value)
            .and_then(|listed| canonical_key(&format!("{kind}:{listed}")))
            .is_some_and(|listed| listed == key)
    };
    let served_same = |value: &Value| {
        value
            .as_str()
            .and_then(canonical_key)
            .is_some_and(|served| served == key)
    };
    let interaction = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    finish_pending_before_change_with(context, &interaction, tracker, stopper)?;
    let (before, after, changed, pending, mirror_error, config_diff) = {
        let _gate = acquire_gate(context)?;
        let pre_store_before = if context.is_isolated() {
            Some(read_config_before(context)?)
        } else {
            None
        };
        let effective_before = context
            .load_config()
            .map_err(|e| SocialError::Unavailable(e.to_string()))?;
        let unchanged = raw_config_object(context)?;
        let mut file = unchanged.clone();
        crate::settings::mirror_into(context, &mut file).map_err(SocialError::Unavailable)?;
        #[cfg(test)]
        {
            let hook = social_state::SCOPE_AFTER_READ
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(hook) = hook {
                hook(&key);
            }
        }
        let Some(mut served) = file
            .pointer("/rollout/rustGroups")
            .and_then(Value::as_array)
            .cloned()
        else {
            return Err(SocialError::Conflict(concat!(
                "rollout.rustGroups is not configured: ",
                "the supervisor serves only that explicit list"
            )
            .to_string())
            .into());
        };
        let deny_list = crate::settings::find(&format!("deny.{field}"))
            .expect("the registry holds the deny lists");
        let denied_ids = deny_list.read(&file);
        let wanted = id.parse::<u64>().ok();
        if admit
            && denied_ids.as_array().is_some_and(|ids| {
                ids.iter()
                    .any(|denied| denied.as_u64().is_some_and(|denied| Some(denied) == wanted))
            })
        {
            return Err(SocialError::Conflict(format!(
                "{key} is in deny.{field}; remove it from the deny list first"
            ))
            .into());
        }
        let allow_list = crate::settings::find(&format!("allow.{field}"))
            .expect("the registry holds the allow lists");
        let mut listed = allow_list
            .read(&file)
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut changed: Vec<String> = Vec::new();
        if admit != listed.iter().any(&listed_same) {
            listed.retain(|value| !listed_same(value));
            if admit {
                listed.push(json!(id.parse::<i64>().expect("canonical ids are safe integers")));
            }
            if !file["allow"].is_object() {
                file["allow"] = json!({});
            }
            let allow = file["allow"].as_object_mut().expect("allow is an object");
            allow.remove(alias);
            allow.insert(field.to_string(), Value::Array(listed));
            changed.push(format!("allow.{field}"));
        }
        if admit != served.iter().any(&served_same) {
            served.retain(|value| !served_same(value));
            if admit {
                served.push(json!(key));
            }
            file["rollout"]["rustGroups"] = Value::Array(served);
            changed.push("rollout.rustGroups".to_string());
        }
        if changed.is_empty() {
            (effective_before.clone(), effective_before, changed, None, None, None)
        } else {
            let changes = crate::settings::console_changes(context, &unchanged, &file)?;
            let settings: Vec<&crate::settings::Setting> =
                changes.iter().map(|(setting, _)| *setting).collect();
            crate::settings::check_confirmation(confirm, &settings)?;
            let previewed: Vec<_> = changes
                .iter()
                .map(|(setting, value)| (*setting, Some(value.clone())))
                .collect();
            let effective_after = context
                .effective_config_with(&file, &previewed)
                .unwrap_or_else(|_| json!({}));
            let stored = !changes.is_empty();
            if stored {
                raw_config_for_write(context)?;
            }
            let pending = begin_access_change(
                context,
                &effective_before,
                &effective_after,
                "access-revoked",
            )?;
            let mirror_error =
                store_access_changes(context, changes, &previewed, &pending, "console")?;
            let diff = if mirror_error.is_none() {
                keep_stored_members(context, &mut file)?;
                let mut diff = write_config_or_forget(
                    context,
                    &file,
                    if stored { &Value::Null } else { &unchanged },
                    &pending,
                )?;
                if let (Some(diff), Some(before_text)) = (&mut diff, pre_store_before) {
                    diff.before = before_text;
                }
                diff
            } else {
                None
            };
            let effective_after = context.load_config().unwrap_or(effective_after);
            (
                effective_before,
                effective_after,
                changed,
                pending,
                mirror_error,
                diff,
            )
        }
    };
    let (retired, failures) = if changed.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        finish_access_change_with(
            context,
            &interaction,
            &before,
            &after,
            "access-revoked",
            pending,
            tracker,
            stopper,
        )
    };
    drop(interaction);
    if !failures.is_empty() {
        return Err(CommitError::with_diff(
            500,
            format!(
                "conversation scope saved, but the immediate revocation did not finish: {}",
                failures.join("; ")
            ),
            config_diff,
        ));
    }
    let target: Vec<String> = after
        .pointer("/rollout/rustGroups")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let restart = live_restart(context, &target);
    let gaps = crate::watchdog::rollout_scope_gaps(&after);
    let mut reply = json!({
        "ok": true,
        "conversation": key,
        "action": if admit { "admit" } else { "remove" },
        "changed": changed,
        "revoked": retired,
        "liveRestart": restart,
        "scope": { "notAllowed": gaps.not_allowed, "notServed": gaps.not_served },
    });
    if let Some(error) = mirror_error {
        reply["mirrorError"] = json!(error);
    }
    Ok(CommitOutcome {
        value: reply,
        config_diff,
    })
}

pub(crate) fn change_conversation_scope(
    context: &RuntimeContext,
    conversation: &str,
    action: &str,
) -> Result<Value, SocialError> {
    change_conversation_scope_waiting_with(
        context,
        conversation,
        action,
        Duration::from_secs(10),
        StopTracker::global(),
        default_stopper(),
    )
}

pub(crate) fn change_conversation_scope_waiting_with(
    context: &RuntimeContext,
    conversation: &str,
    action: &str,
    limit: Duration,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<Value, SocialError> {
    let body = json!({
        "conversation": conversation,
        "action": action,
        "confirm": true,
    });
    let _ = import_engine_choices(context)?;
    let result = commit_conversation_scope_with(context, &body, tracker, stopper)
        .map(|outcome| outcome.value)
        .map_err(|err| SocialError::from_http(err.into_http()));
    let stopped = tracker.wait(limit);
    result.map(|mut reply| {
        if !stopped {
            reply["sessionStops"] =
                json!("unfinished: check the retired sessions' tasks in DSH");
        }
        reply
    })
}

#[cfg(test)]
thread_local! {
    static ISOLATED_UNDO_WRITE_FAIL: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
    static LEGACY_UNDO_WRITE_FAIL: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn take_undo_write_error(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<(String, String)>>>,
    file: &str,
) -> Option<String> {
    slot.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if borrow.as_ref().is_some_and(|(target, _)| target == file) {
            borrow.take().map(|(_, error)| error)
        } else {
            None
        }
    })
}

struct PreparedLegacyUndo {
    file: String,
    old: String,
    snapshot: Option<Vec<u8>>,
}

fn write_legacy_undo_file(file: &str, text: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(error) = take_undo_write_error(&LEGACY_UNDO_WRITE_FAIL, file) {
        return Err(error);
    }
    std::fs::write(file, text).map_err(|e| e.to_string())
}

fn write_legacy_undo_targets(
    prepared: Vec<PreparedLegacyUndo>,
) -> Result<Vec<String>, CommitError> {
    let mut restored = Vec::with_capacity(prepared.len());
    let mut written: Vec<PreparedLegacyUndo> = Vec::with_capacity(prepared.len());
    for target in prepared {
        if let Err(error) = write_legacy_undo_file(&target.file, &target.old) {
            let mut message = format!("写回失败 {}: {error}", target.file);
            let mut rollback_errors = Vec::new();
            for prev in written.into_iter().rev() {
                let rb_result = match prev.snapshot.as_deref() {
                    Some(bytes) => std::fs::write(&prev.file, bytes),
                    None => std::fs::remove_file(&prev.file),
                };
                if let Err(rb_err) = rb_result {
                    rollback_errors.push(format!("{} ({rb_err})", prev.file));
                }
            }
            if !rollback_errors.is_empty() {
                message.push_str(&format!(
                    "；回滚已写文件失败：{}",
                    rollback_errors.join("、")
                ));
            }
            return Err(CommitError::plain(500, message));
        }
        restored.push(target.file.clone());
        written.push(target);
    }
    Ok(restored)
}

struct PreparedIsolatedUndo {
    file: String,
    area: RuntimeArea,
    relative: &'static str,
    snapshot: Option<Vec<u8>>,
    text: String,
}

fn write_isolated_undo_target(
    context: &RuntimeContext,
    target: &PreparedIsolatedUndo,
    pre_store_before: &str,
    published_diff: &mut Option<ConfigDiff>,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(error) = take_undo_write_error(&ISOLATED_UNDO_WRITE_FAIL, &target.file) {
        return Err(error);
    }
    if target.file == "config/config.json" {
        write_selected_config_text(context, &target.text).map(|mut diff| {
            diff.before = pre_store_before.to_string();
            *published_diff = Some(diff);
        })
    } else {
        context.write_atomic(target.area, target.relative, target.text.as_bytes())
    }
}

pub(crate) fn commit_audit_undo(
    context: &RuntimeContext,
    body: &str,
) -> Result<CommitOutcome<Value>, CommitError> {
    let req: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
    let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
    if id.is_empty() {
        return Err(CommitError::plain(400, "需要 id"));
    }
    let confirm = crate::settings::confirm_field(&req)?;
    let entry = crate::console::audit_recent(context, 500)
        .map_err(|e| CommitError::plain(503, e))?
        .into_iter()
        .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
        .ok_or_else(|| CommitError::plain(404, format!("找不到审计记录 {id}")))?;
    let files = entry
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if context.is_isolated() {
        return commit_isolated_audit_undo(context, id, &entry, confirm, files);
    }
    // Phase 1: validate all inputs and snapshot pre-undo contents before writing any file.
    let mut prepared = Vec::with_capacity(files.len());
    let mut wake_before: Option<Value> = None;
    for f in &files {
        let file = f.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let backup = f.get("backup").and_then(|v| v.as_str()).unwrap_or("");
        if !crate::console::AUDIT_FILES.contains(&file) {
            return Err(CommitError::plain(
                400,
                format!("拒绝恢复未声明的文件：{file}"),
            ));
        }
        if prepared
            .iter()
            .any(|existing: &PreparedLegacyUndo| existing.file == file)
        {
            return Err(CommitError::plain(
                400,
                format!("审计记录包含重复的目标文件：{file}"),
            ));
        }
        if !crate::console::audit_backup_allowed_for(context, backup) {
            return Err(CommitError::plain(
                400,
                format!("拒绝恢复 audit-blobs 之外的文件：{backup}"),
            ));
        }
        let old = std::fs::read_to_string(backup)
            .map_err(|e| CommitError::plain(500, format!("读备份失败 {backup}: {e}")))?;
        let backup_cfg: Value = serde_json::from_str(&old).map_err(|_| {
            CommitError::plain(500, format!("备份内容不是合法 JSON，拒绝写入：{backup}"))
        })?;
        if file == LEGACY_CONFIG_PATH {
            let before = context
                .load_config_requiring_settings()
                .map_err(|error| CommitError::plain(503, error))?;
            if before["socialV2"]["wake"] != backup_cfg["socialV2"]["wake"] {
                wake_before = Some(before);
            }
        }
        let snapshot = match std::fs::read(file) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(CommitError::plain(
                    503,
                    format!("读取目标文件失败 {file}: {err}"),
                ));
            }
        };
        prepared.push(PreparedLegacyUndo {
            file: file.to_string(),
            old,
            snapshot,
        });
    }
    // Phase 2: write all prepared files with rollback on failure, then reconcile wake plans.
    let restored = write_legacy_undo_targets(prepared)?;
    if let Some(before) = wake_before {
        reconcile_default_wake_configs(context, Some(&before))
            .map_err(CommitError::from_social)?;
    }
    Ok(CommitOutcome {
        value: json!({ "ok": true, "id": id, "restored": restored }),
        config_diff: None,
    })
}

fn commit_isolated_audit_undo(
    context: &RuntimeContext,
    id: &str,
    entry: &Value,
    confirm: bool,
    files: Vec<Value>,
) -> Result<CommitOutcome<Value>, CommitError> {
    if entry.get("path").and_then(Value::as_str) == Some("/api/console/token") {
        return Err(CommitError::plain(
            409,
            "控制台访问令牌的修改不能撤销（会恢复被替换的旧令牌）；如需更换请重新设置令牌",
        ));
    }
    if files.is_empty() {
        return Err(CommitError::plain(
            409,
            format!("审计记录 {id} 没有改动文件，没有可恢复的内容"),
        ));
    }
    let mut pending: Vec<(String, String)> = Vec::with_capacity(files.len());
    for f in &files {
        let file = f.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let backup = f.get("backup").and_then(|v| v.as_str()).unwrap_or("");
        if !["config/config.json", "data/role-map.json"].contains(&file) {
            return Err(CommitError::plain(
                400,
                format!("拒绝恢复未声明的文件：{file}"),
            ));
        }
        if pending.iter().any(|(existing, _)| existing == file) {
            return Err(CommitError::plain(
                400,
                format!("审计记录包含重复的目标文件：{file}"),
            ));
        }
        if !crate::console::audit_backup_allowed_for(context, backup) {
            return Err(CommitError::plain(
                400,
                format!("拒绝恢复 audit-blobs 之外的文件：{backup}"),
            ));
        }
        let old = context
            .read_text(RuntimeArea::Data, backup)
            .map_err(|e| CommitError::plain(500, format!("读备份失败 {backup}: {e}")))?;
        let parsed = serde_json::from_str::<Value>(&old).map_err(|_| {
            CommitError::plain(500, format!("备份内容不是合法 JSON，拒绝写入：{backup}"))
        })?;
        if file == "config/config.json"
            && (!parsed.is_object() || crate::config::canonical_config(&parsed).is_err())
        {
            return Err(CommitError::plain(
                500,
                format!("备份不是可加载的运行配置，拒绝写入：{backup}"),
            ));
        }
        pending.push((file.to_string(), old));
    }
    let interaction = crate::closed_agent::lock(context).map_err(|e| CommitError::plain(503, e))?;
    finish_pending_before_change(context, &interaction)?;
    let mut settings_kept: Vec<&'static str> = Vec::new();
    let mut wake_error = None;
    let mut published_diff: Option<ConfigDiff> = None;
    let (config_before, restored, planned) = {
        let _gate = acquire_gate(context)?;
        let pre_store_before = read_config_before(context)?;
        // Phase 1a: check freshness and snapshot pre-undo raw bytes of all target files
        // before mutating the store, pending revocations, or any file.
        let mut snapshots = Vec::with_capacity(pending.len());
        for (f, (file, _)) in files.iter().zip(&pending) {
            let (area, relative) =
                crate::console::isolated_audit_file(file).expect("declared file checked above");
            let current_opt = context.read_optional_text(area, relative).map_err(|_| {
                CommitError::plain(503, "selected runtime audit target is unavailable")
            })?;
            let snapshot = current_opt.as_ref().map(|text| text.as_bytes().to_vec());
            let current = current_opt.unwrap_or_default();
            let unchanged = match f.get("afterRegistryFreeSha").and_then(Value::as_str) {
                Some(expected) if file == "config/config.json" => {
                    crate::console::config_registry_free_sha(&current).as_deref()
                        == Some(expected)
                }
                _ => {
                    f.get("afterSha").and_then(Value::as_str)
                        == Some(crate::console::sha256_hex(&current).as_str())
                }
            };
            if !unchanged {
                return Err(CommitError::plain(
                    409,
                    format!(
                        "{file} 在这条审计记录之后又被修改过，撤销会覆盖之后的变更；请先撤销更新的记录"
                    ),
                ));
            }
            snapshots.push((area, relative, snapshot));
        }
        let config_before = context
            .load_config()
            .map_err(|e| CommitError::plain(503, format!("读取当前配置失败，未撤销：{e}")))?;

        // Phase 1b: prepare and validate every target file in memory before writing any file.
        let mut prepared = Vec::with_capacity(pending.len());
        let mut planned: Option<(Value, Option<String>, Option<String>)> = None;
        let mut unchanged_config = Value::Null;
        let mut wake_changed = false;
        for ((file, old), (area, relative, snapshot)) in pending.into_iter().zip(snapshots) {
            let text = if file == "config/config.json" {
                let current = raw_config_for_write(context).map_err(|err| {
                    CommitError::plain(
                        503,
                        format!("读取当前配置失败，未撤销：{}", err.message()),
                    )
                })?;
                let mut restored_config: Value = serde_json::from_str(&old)
                    .map_err(|_| CommitError::plain(500, "备份内容不是合法 JSON"))?;
                match (current.get("consoleToken"), restored_config.as_object_mut()) {
                    (Some(token), Some(object)) => {
                        object.insert("consoleToken".into(), token.clone());
                    }
                    (None, Some(object)) => {
                        object.remove("consoleToken");
                    }
                    _ => {}
                }
                let security =
                    crate::settings::security_restore(context, &current, &restored_config)?;
                let restoring: Vec<&crate::settings::Setting> =
                    security.iter().map(|(setting, _)| *setting).collect();
                crate::settings::check_confirmation(confirm, &restoring)?;
                let previewed: Vec<_> = security
                    .iter()
                    .map(|(setting, value)| (*setting, Some(value.clone())))
                    .collect();
                settings_kept =
                    crate::settings::keep_registry_values(&current, &mut restored_config).map_err(
                        |key| {
                            CommitError::plain(
                                409,
                                format!(
                                    "the backup cannot keep the current value of {key}; \
                                     nothing was undone"
                                ),
                            )
                        },
                    )?;
                settings_kept.retain(|key| {
                    !crate::settings::find(key).is_some_and(|setting| setting.confirmed())
                });
                let (after, load_error) =
                    match context.effective_config_with(&restored_config, &previewed) {
                        Ok(after) => (after, None),
                        Err(error) => (json!({}), Some(format!("读取恢复后的配置失败：{error}"))),
                    };
                let stored = !security.is_empty();
                let change =
                    begin_access_change(context, &config_before, &after, "audit-undo")?;
                store_access_changes(context, security, &previewed, &change, "audit-undo")?;
                let pre_write_unchanged = if stored {
                    Value::Null
                } else {
                    config_snapshot(context)
                };
                if let Err(err) = keep_stored_members(context, &mut restored_config) {
                    let mut commit_err = CommitError::from_social(err);
                    commit_err
                        .message
                        .push_str(&settle_failed_write(context, &pre_write_unchanged, &change));
                    return Err(commit_err);
                }
                wake_changed =
                    current["socialV2"]["wake"] != restored_config["socialV2"]["wake"];
                let text = match serde_json::to_string_pretty(&restored_config) {
                    Ok(text) => text,
                    Err(e) => {
                        let mut msg = e.to_string();
                        msg.push_str(&settle_failed_write(
                            context,
                            &pre_write_unchanged,
                            &change,
                        ));
                        return Err(CommitError::plain(500, msg));
                    }
                };
                planned = Some((after, load_error, change));
                unchanged_config = pre_write_unchanged;
                text
            } else {
                old
            };
            prepared.push(PreparedIsolatedUndo {
                file,
                area,
                relative,
                snapshot,
                text,
            });
        }

        // Phase 2: write prepared files; on failure, roll back earlier writes in reverse order.
        let mut restored = Vec::with_capacity(prepared.len());
        let mut written: Vec<PreparedIsolatedUndo> = Vec::with_capacity(prepared.len());
        for target in prepared {
            let write_result = write_isolated_undo_target(
                context,
                &target,
                &pre_store_before,
                &mut published_diff,
            );
            if let Err(error) = write_result {
                let mut message = format!("写回失败 {}: {error}", target.file);
                let mut config_rolled_back = false;
                let mut rollback_errors = Vec::new();
                for prev in written.into_iter().rev() {
                    let rb_result = match prev.snapshot.as_deref() {
                        Some(bytes) => context.write_atomic(prev.area, prev.relative, bytes),
                        None => context.remove_file(prev.area, prev.relative),
                    };
                    match rb_result {
                        Ok(()) => {
                            if prev.file == "config/config.json" {
                                config_rolled_back = true;
                            }
                        }
                        Err(rb_err) => {
                            rollback_errors.push(format!("{} ({rb_err})", prev.file));
                        }
                    }
                }
                if config_rolled_back {
                    published_diff = None;
                }
                if let Some((_, _, change)) = planned.as_ref() {
                    message.push_str(&settle_failed_write(context, &unchanged_config, change));
                }
                if !rollback_errors.is_empty() {
                    message.push_str(&format!(
                        "；回滚已写文件失败：{}",
                        rollback_errors.join("、")
                    ));
                }
                return Err(CommitError::with_diff(500, message, published_diff));
            }
            restored.push(target.file.clone());
            written.push(target);
        }
        if wake_changed {
            wake_error = reconcile_default_wake_configs(context, Some(&config_before)).err();
        }
        (config_before, restored, planned)
    };
    let (after, load_error, change) = match planned {
        Some(planned) => planned,
        None => match context.load_config() {
            Ok(after) => (after, None, None),
            Err(error) => (json!({}), Some(format!("读取恢复后的配置失败：{error}")), None),
        },
    };
    let (revoked, mut failures) = finish_access_change(
        context,
        &interaction,
        &config_before,
        &after,
        "audit-undo",
        change,
    );
    drop(interaction);
    failures.extend(load_error);
    if !failures.is_empty() {
        return Err(CommitError::with_diff(
            500,
            format!(
                "已恢复 {}，但立即撤销未完成：{}",
                restored.join("、"),
                failures.join("；")
            ),
            published_diff,
        ));
    }
    if let Some(error) = wake_error {
        let (status, error) = error.into_http();
        return Err(CommitError::with_diff(
            status,
            format!(
                "configuration restored but inherited wake plans could not be refreshed: {error}"
            ),
            published_diff,
        ));
    }
    Ok(CommitOutcome {
        value: json!({
            "ok": true,
            "id": id,
            "restored": restored,
            "revoked": revoked,
            "settingsKept": settings_kept,
        }),
        config_diff: published_diff,
    })
}

/// 允许通过对话修改的配置项（白名单，前缀/全路径精确匹配）。
pub(crate) const SETTINGS_WRITABLE: [&str; 3] = [
    "socialV2.context.wakeMaxChars",
    "socialV2.context.wakeRecentLimit",
    "role.maxInjectChars",
];

const SETTINGS_READONLY_HINT: &str =
    "白名单 allow.* / ownerQQ / 后端与网关配置 / consoleToken / 任何路径类字段只能本地改";

const SETTINGS_SECRET_MARKER: &str = "(已隐藏)";

pub(crate) const SETTINGS_READ_REFUSED: &str =
    "只能读取可以通过对话修改的设置；不带 settingKey 调用 settings_get 可以看到这些设置";

const SETTINGS_STORE_UNAVAILABLE: &str = "设置存储暂时不可用，这次没有改动";

pub(crate) const SETTINGS_MIRROR_UNFINISHED: &str =
    "配置文件这次没有同步，下次改设置或控制台重启时会补上";

fn settings_applies_note(applies: &str) -> &'static str {
    match applies {
        "live" => "已立即生效",
        "next wake" => "下次唤醒时生效",
        _ => "重启后生效",
    }
}

/// A secret config path: everything under `security.*`, and every path with a segment (lower case,
/// `-` and `_` removed) that ends in token(s), secret(s), password, passwd, apikey(s), key(s) or
/// credential(s).
pub(crate) fn setting_is_secret(path: &str) -> bool {
    if path
        .split('.')
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case("security"))
    {
        return true;
    }
    path.split('.').any(|segment| {
        let segment: String = segment
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .collect::<String>()
            .to_lowercase();
        [
            "token",
            "tokens",
            "secret",
            "secrets",
            "password",
            "passwd",
            "apikey",
            "apikeys",
            "key",
            "keys",
            "credential",
            "credentials",
        ]
        .iter()
        .any(|suffix| segment.ends_with(suffix))
    })
}

pub(crate) fn redact_setting(path: &str, value: Value) -> Value {
    if setting_is_secret(path) {
        return json!(SETTINGS_SECRET_MARKER);
    }
    match value {
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(name, member)| {
                    let redacted = redact_setting(&format!("{path}.{name}"), member);
                    (name, redacted)
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| redact_setting(path, item)).collect())
        }
        other => other,
    }
}

pub(crate) fn is_setting_writable(path: &str) -> bool {
    let p = path.trim();
    if p.is_empty() {
        return false;
    }
    let lower = p.to_lowercase();
    for bad in [
        "allow",
        "token",
        "secret",
        "password",
        "path",
        "ownerqq",
        "dbpath",
        "consoletoken",
    ] {
        if lower.contains(bad) {
            return false;
        }
    }
    if p.starts_with("dsh.") || p.starts_with("snowluma.") {
        return false;
    }
    SETTINGS_WRITABLE.contains(&p)
}

fn read_setting_path(cfg: &Value, path: &str) -> Option<Value> {
    if let Some(value) = path.split('.').try_fold(cfg, |cur, key| cur.get(key)) {
        return Some(value.clone());
    }
    let (group, default) = match path {
        "socialV2.context.wakeMaxChars" => ("context", json!(6000)),
        "socialV2.context.wakeRecentLimit" => ("context", json!(4)),
        "role.maxInjectChars" => return Some(json!(6000)),
        _ => return None,
    };
    if cfg.get("socialV2").and_then(|v| v.get(group)).is_none() {
        Some(default)
    } else {
        None
    }
}

fn write_setting_path(context: &RuntimeContext, path: &str, value: &Value) -> Result<(), String> {
    let mut file = read_raw_for_edit(context, ConfigBase::RawObject).map_err(|(_, e)| {
        if context.is_isolated() {
            "selected runtime config is unavailable".to_string()
        } else {
            e
        }
    })?;
    if context.is_isolated() && !file.is_object() {
        return Err("selected runtime config is invalid".to_string());
    }
    let parts: Vec<&str> = path.split('.').collect();
    let mut node = &mut file;
    for (i, k) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let Some(obj) = node.as_object_mut() {
                obj.insert(k.to_string(), value.clone());
            }
            break;
        }
        let obj = node
            .as_object_mut()
            .ok_or_else(|| format!("{path} 的路径中段不是对象"))?;
        if !obj.get(*k).map(|v| v.is_object()).unwrap_or(false) {
            obj.insert(k.to_string(), json!({}));
        }
        node = obj
            .get_mut(*k)
            .ok_or_else(|| format!("{path} 的路径段 {k} 写入失败"))?;
    }
    let _diff = write_config_json(context, &file)?;
    Ok(())
}

/// `settings-get` / `settings-set` for a caller that already proved owner-private authority.
pub(crate) fn settings_action(
    action: &str,
    b: &Value,
    cfg: &Value,
    context: &RuntimeContext,
) -> Result<Value, (u16, String)> {
    if action == "settings-get" {
        let path = b
            .get("settingKey")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() {
            let mut all = serde_json::Map::new();
            for p in SETTINGS_WRITABLE {
                if let Some(v) = read_setting_path(cfg, p) {
                    all.insert(p.to_string(), redact_setting(p, v));
                }
            }
            let mut writable: Vec<String> =
                SETTINGS_WRITABLE.iter().map(|p| p.to_string()).collect();
            for setting in crate::settings::chat_registry().filter(|_| context.is_isolated()) {
                let name = setting.chat_name();
                all.insert(name.clone(), redact_setting(&name, setting.read(cfg)));
                writable.push(name);
            }
            return Ok(json!({
                "ok": true,
                "settings": all,
                "writable": writable,
                "readonly": SETTINGS_READONLY_HINT,
            }));
        }
        if setting_is_secret(&path) {
            return Err((403, SETTINGS_READ_REFUSED.to_string()));
        }
        if let Some(setting) =
            crate::settings::chat_setting(&path).filter(|_| context.is_isolated())
        {
            let value = redact_setting(&path, setting.read(cfg));
            return Ok(json!({"ok": true, "key": path, "writable": true, "value": value}));
        }
        if !SETTINGS_WRITABLE.contains(&path.as_str()) {
            return Err((403, SETTINGS_READ_REFUSED.to_string()));
        }
        let mut result = json!({
            "ok": true,
            "key": path,
            "writable": is_setting_writable(&path),
        });
        if let Some(value) = read_setting_path(cfg, &path) {
            result["value"] = redact_setting(&path, value);
        }
        return Ok(result);
    }
    let path = b
        .get("settingKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some(setting) = crate::settings::chat_setting(&path)
        .filter(|_| context.is_isolated() && !setting_is_secret(&path))
    {
        let value = b.get("value").cloned().unwrap_or(Value::Null);
        let applied = crate::settings::apply(context, setting.key, &value, "owner", "chat")
            .map_err(|(status, _)| {
                if status == 400 {
                    (400, setting.chat_requirement())
                } else {
                    (503, SETTINGS_STORE_UNAVAILABLE.to_string())
                }
            })?;
        let mut reply = json!({
            "ok": true,
            "key": path,
            "value": applied.value,
            "changed": applied.changed,
            "hot": setting.applies != "restart",
            "note": settings_applies_note(setting.applies),
        });
        if applied.mirror_error.is_some() {
            reply["mirrorError"] = json!(SETTINGS_MIRROR_UNFINISHED);
            reply["note"] = json!(format!(
                "{}；{SETTINGS_MIRROR_UNFINISHED}",
                settings_applies_note(setting.applies)
            ));
        }
        return Ok(reply);
    }
    if !is_setting_writable(&path) {
        return Err((
            403,
            "该项不可通过对话修改（只有 settings_get 列出的设置可以改；白名单/令牌/路径类必须本地改）"
                .to_string(),
        ));
    }
    let value = b.get("value").cloned().unwrap_or(Value::Null);
    write_setting_path(context, &path, &value).map_err(|e| (400, e))?;
    crate::safe_eprintln!("[console] settings 改了 {path} -> {value}");
    Ok(json!({
        "ok": true,
        "key": path,
        "value": value,
        "hot": true,
        "note": "已立即生效",
    }))
}

/// Chat settings transaction invoked from `memory_actions` holding `locks/restore-writers.lock`
/// (shared) and `config_gate` (`locks/social-v2.lock`). Reconciles inherited wake plans under the
/// same `config_gate` when a wake-timing setting changes.
pub(crate) fn commit_chat_settings(
    action: &str,
    b: &Value,
    cfg: &Value,
    context: &RuntimeContext,
    held_gate: Option<&Gate>,
) -> Result<CommitOutcome<Value>, CommitError> {
    let _local_gate = if held_gate.is_none() && context.is_isolated() && action == "settings-set" {
        acquire_config_gate(context)?
    } else {
        None
    };
    let before_text = (context.is_isolated() && action == "settings-set")
        .then(|| read_config_before(context))
        .transpose()?;
    let mut reply = settings_action(action, b, cfg, context)?;
    let config_diff = match before_text {
        Some(before) => {
            let after = read_config_before(context)?;
            (after != before).then_some(ConfigDiff { before, after })
        }
        None => None,
    };
    if action == "settings-set"
        && reply["changed"] == true
        && reply["key"]
            .as_str()
            .and_then(crate::settings::chat_setting)
            .is_some_and(|s| crate::settings::wake_timing::refreshes_default_plan(s.key))
        && reconcile_default_wake_configs(context, None).is_err()
    {
        reply["note"] = json!(format!(
            "{}；设置已保存，这次未能同步默认唤醒计划",
            reply["note"].as_str().unwrap_or("")
        ));
    }
    Ok(CommitOutcome {
        value: reply,
        config_diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let safe: String = label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-config-tx-{}-{}-{safe}",
                std::process::id(),
                crate::parity_clock::now_ms()
            )));
            let _ = std::fs::remove_dir_all(&dir);
            for sub in ["config", "data", "resources", "logs", "user/dsh"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(label: &str, cfg: Value) -> (TestDir, RuntimeContext) {
        let root = TestDir::new(label);
        let mut config = cfg;
        if config.get("consoleToken").is_none() {
            config["consoleToken"] = json!("fixture-config-tx-token-abcdefgh");
        }
        std::fs::write(
            root.0.join("config/config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.0.join("data/social-v2.json"),
            r#"{"paused":false,"conversations":{}}"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("dependencies.json"),
            json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:1",
                "onebot": "http://127.0.0.1:1"
            })
            .to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).unwrap();
        (root, context)
    }

    #[test]
    fn write_selected_config_text_returns_exact_before_and_after_diff() {
        let (root, context) = fixture(
            "diff-exact",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "notes": "before"
            }),
        );
        let before = std::fs::read_to_string(root.0.join("config/config.json")).unwrap();
        let mut next: Value = serde_json::from_str(&before).unwrap();
        next["notes"] = json!("after");
        let after_text = serde_json::to_string_pretty(&next).unwrap();

        let diff = write_selected_config_text(&context, &after_text).unwrap();
        assert_eq!(diff.before, before);
        assert_eq!(diff.after, after_text);
        assert_eq!(
            std::fs::read_to_string(root.0.join("config/config.json")).unwrap(),
            after_text
        );
    }

    #[test]
    fn commit_page_config_captures_pre_store_before_across_settings_mirror() {
        let (root, context) = fixture(
            "page-diff-pre-store",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "socialV2": { "autoReplyCheckMs": 3000 }
            }),
        );
        let initial = std::fs::read_to_string(root.0.join("config/config.json")).unwrap();

        let outcome = commit_page_config(&context, |ctx| {
            let base = raw_config_object(ctx).map_err(SocialError::into_http)?;
            let mut next = base.clone();
            next["socialV2"]["autoReplyCheckMs"] = json!(5000);
            let carried = json!({ "socialV2": { "reply": { "wakeQuietMs": 4500 } } });
            Ok(PageConfigWrite {
                file: next,
                base_for_changes: base,
                carried,
                confirm: false,
                check_canonical_before_store: true,
                keep_stored_always: false,
                prior_wake: None,
                keep_error_fmt: |key| (400, format!("cannot hold {key}")),
                isolated_write_error_503: false,
                wake_error_prefix: "",
                finish: Box::new(|_, file| Ok(file.clone())),
            })
        })
        .unwrap();

        let diff = outcome.config_diff.expect("page commit returns diff");
        assert_eq!(
            diff.before, initial,
            "before snapshot must precede settings.db write-back"
        );
        let after_json: Value = serde_json::from_str(&diff.after).unwrap();
        assert_eq!(after_json["socialV2"]["autoReplyCheckMs"], 5000);
        assert_eq!(after_json["socialV2"]["reply"]["wakeQuietMs"], 4500);
        assert!(
            serde_json::from_str::<Value>(&diff.before)
                .unwrap()
                .pointer("/socialV2/reply/wakeQuietMs")
                .is_none(),
            "pre-store diff.before must not contain the mirrored setting"
        );
    }

    #[test]
    fn concurrent_lock_ordering_holds_config_gate_before_store_gate() {
        let (_root, context) = fixture(
            "lock-order",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42, 43], "private": [7001] },
                "rollout": { "enabled": true, "rustGroups": ["group:42", "group:43"] }
            }),
        );
        let gate = acquire_gate(&context).unwrap();
        let outcome = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                commit_conversation_scope(
                    &context,
                    &json!({ "conversation": "group:43", "action": "remove", "confirm": true }),
                )
            });
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                !worker.is_finished(),
                "config_tx must wait on held config_gate"
            );
            let store_gate =
                context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock");
            assert!(
                store_gate.is_ok(),
                "config_tx must not hold store_gate while waiting on config_gate"
            );
            drop(store_gate);
            drop(gate);
            worker.join().unwrap()
        });
        let outcome = outcome.unwrap();
        assert_eq!(outcome.value["changed"], json!(["allow.groups", "rollout.rustGroups"]));
        assert!(outcome.config_diff.is_some());

        // Held closed_agent lock blocks access commits before touching config_gate or store_gate.
        let interaction = crate::closed_agent::lock(&context).unwrap();
        let blocked = commit_whitelist(
            &context,
            &json!({ "allow": { "groups": [] }, "confirm": true }),
        );
        drop(interaction);
        assert_eq!(blocked.unwrap_err().status, 503);
    }

    #[test]
    fn failed_store_commit_cleans_up_pending_revocation_and_leaves_config_untouched() {
        let (root, context) = fixture(
            "rollback-store-fail",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "rollout": { "enabled": true, "rustGroups": ["group:42"] }
            }),
        );
        let before = std::fs::read(root.0.join("config/config.json")).unwrap();
        // Seed store first so FAIL_BEFORE_AUDIT triggers inside write_store_with.
        let _ = commit_page_config(&context, |ctx| {
            let base = raw_config_object(ctx).map_err(SocialError::into_http)?;
            Ok(PageConfigWrite {
                file: base.clone(),
                base_for_changes: base,
                carried: json!({ "socialV2": { "reply": { "wakeQuietMs": 4500 } } }),
                confirm: false,
                check_canonical_before_store: true,
                keep_stored_always: false,
                prior_wake: None,
                keep_error_fmt: |key| (400, key.to_string()),
                isolated_write_error_503: false,
                wake_error_prefix: "",
                finish: Box::new(|_, _| Ok(json!({"ok": true}))),
            })
        })
        .unwrap();
        let before_with_mirror = std::fs::read(root.0.join("config/config.json")).unwrap();
        assert_ne!(before_with_mirror, before);

        crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
        let failed = commit_conversation_scope(
            &context,
            &json!({ "conversation": "group:42", "action": "remove", "confirm": true }),
        );
        crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(false));

        assert_eq!(failed.unwrap_err().status, 503);
        assert_eq!(
            std::fs::read(root.0.join("config/config.json")).unwrap(),
            before_with_mirror,
            "failed store commit must leave config.json unchanged"
        );
        let pending_file = root.0.join("data").join(social_state::PENDING_REVOCATIONS);
        let pending: Value = if pending_file.exists() {
            serde_json::from_slice(&std::fs::read(&pending_file).unwrap()).unwrap()
        } else {
            json!({ "pending": [] })
        };
        assert_eq!(
            pending["pending"],
            json!([]),
            "uncommitted store failure must clean up its pending revocation"
        );
    }

    #[test]
    fn commit_chat_settings_reconciles_wake_plans_and_legacy_guards_secrets() {
        let (_root, context) = fixture(
            "chat-wake-sync",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [101], "private": [7001] }
            }),
        );
        context
            .write_atomic(
                RuntimeArea::Data,
                "social-v2.json",
                json!({
                    "conversations": {
                        "group:101": {
                            "wakeConfig": {
                                "confirmedBy": "default",
                                "batchWindowMs": 3000
                            }
                        }
                    }
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let gate = acquire_gate(&context).unwrap();
        let cfg = context.load_config().unwrap();
        let outcome = commit_chat_settings(
            "settings-set",
            &json!({ "settingKey": "socialV2.wake.batchWindowMs", "value": 4500 }),
            &cfg,
            &context,
            Some(&gate),
        )
        .unwrap();
        drop(gate);
        assert_eq!(outcome.value["changed"], true);
        let chat_diff = outcome.config_diff.expect("chat settings write returns diff");
        assert_ne!(chat_diff.before, chat_diff.after);
        assert!(chat_diff.after.contains("4500"));
        let state = context.read_social_state().unwrap();
        assert_eq!(
            state["conversations"]["group:101"]["wakeConfig"]["batchWindowMs"],
            4500
        );

        let legacy = RuntimeContext::legacy();
        for secret in ["webSearch.apiKey", "cliModel.apiKey", "security.outboundGuard"] {
            assert!(setting_is_secret(secret), "{secret}");
            assert_eq!(
                settings_action("settings-get", &json!({"settingKey": secret}), &cfg, &legacy)
                    .unwrap_err()
                    .0,
                403
            );
        }

        // Verify background scope/endpoint writes leave zero console audit log side effects.
        let (root_cli, ctx_cli) = fixture(
            "cli-no-audit-side-effect",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "rollout": { "enabled": true, "rustGroups": ["group:42"] }
            }),
        );
        let synced = Arc::new(AtomicBool::new(true));
        struct NoopStopper(Arc<AtomicBool>);
        impl SessionStopper for NoopStopper {
            fn stop_and_archive(
                &self,
                _context: &RuntimeContext,
                _cfg: &Value,
                _key: &str,
                _session_id: &str,
            ) -> Result<(), String> {
                if self.0.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("stopped".into())
                }
            }
            fn sync_mode(&self, _context: &RuntimeContext, _cfg: &Value, _mode: &str) -> bool {
                true
            }
        }
        let tracker = Arc::new(StopTracker::new());
        let stopper: Arc<dyn SessionStopper> = Arc::new(NoopStopper(synced));
        let _ = change_conversation_scope_waiting_with(
            &ctx_cli,
            "group:42",
            "remove",
            Duration::from_secs(2),
            &tracker,
            &stopper,
        )
        .unwrap();
        assert!(!root_cli.0.join("data/audit/audit.jsonl").exists());
    }

    #[test]
    fn commit_audit_undo_rejects_stale_and_restores_matching_config_diff() {
        let (root, context) = fixture(
            "audit-undo-tx",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "notes": "before-undo"
            }),
        );
        let original = std::fs::read_to_string(root.0.join("config/config.json")).unwrap();
        let mut updated: Value = serde_json::from_str(&original).unwrap();
        updated["notes"] = json!("after-edit");
        let diff = write_selected_config_text(
            &context,
            &serde_json::to_string_pretty(&updated).unwrap(),
        )
        .unwrap();

        std::fs::create_dir_all(root.0.join("data/audit/blobs/tx-1")).unwrap();
        std::fs::write(
            root.0.join("data/audit/blobs/tx-1/config.json"),
            &diff.before,
        )
        .unwrap();
        let entry = json!({
            "id": "tx-1",
            "ts": 1000,
            "method": "POST",
            "path": "/api/social",
            "status": 200,
            "files": [{
                "file": "config/config.json",
                "backup": "audit/blobs/tx-1/config.json",
                "afterSha": crate::console::sha256_hex(&diff.after),
                "afterRegistryFreeSha": crate::console::config_registry_free_sha(&diff.after),
            }]
        });
        std::fs::write(
            root.0.join("data/audit/audit.jsonl"),
            format!("{entry}\n"),
        )
        .unwrap();

        // Concurrent modification after the recorded SHA must be refused with 409.
        updated["notes"] = json!("newer-edit");
        write_selected_config_text(
            &context,
            &serde_json::to_string_pretty(&updated).unwrap(),
        )
        .unwrap();
        let stale = commit_audit_undo(&context, &json!({ "id": "tx-1" }).to_string()).unwrap_err();
        assert_eq!(stale.status, 409);

        // Restoring matching `afterRegistryFreeSha` succeeds and returns the undo ConfigDiff.
        let matching: Value = serde_json::from_str(&diff.after).unwrap();
        write_selected_config_text(
            &context,
            &serde_json::to_string_pretty(&matching).unwrap(),
        )
        .unwrap();
        let undo = commit_audit_undo(&context, &json!({ "id": "tx-1" }).to_string()).unwrap();
        assert_eq!(undo.value["ok"], true);
        let undo_diff = undo.config_diff.expect("audit undo returns ConfigDiff");
        assert_eq!(undo_diff.before, diff.after);
        assert_eq!(undo_diff.after, diff.before);
        assert_eq!(
            std::fs::read_to_string(root.0.join("config/config.json")).unwrap(),
            diff.before
        );
    }

    struct UndoWriteFailGuard;

    impl Drop for UndoWriteFailGuard {
        fn drop(&mut self) {
            ISOLATED_UNDO_WRITE_FAIL.with(|slot| *slot.borrow_mut() = None);
            LEGACY_UNDO_WRITE_FAIL.with(|slot| *slot.borrow_mut() = None);
        }
    }

    fn seed_multi_file_audit(
        root: &TestDir,
        id: &str,
        role_map_backup: &str,
        role_map_current: Option<&str>,
        config_backup: &str,
        config_current: &str,
        role_map_first: bool,
    ) {
        let blob_dir = root.0.join("data/audit/blobs").join(id);
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join("role-map.json"), role_map_backup).unwrap();
        std::fs::write(blob_dir.join("config.json"), config_backup).unwrap();
        match role_map_current {
            Some(text) => std::fs::write(root.0.join("data/role-map.json"), text).unwrap(),
            None => {
                let _ = std::fs::remove_file(root.0.join("data/role-map.json"));
            }
        }
        std::fs::write(root.0.join("config/config.json"), config_current).unwrap();
        let role_entry = json!({
            "file": "data/role-map.json",
            "backup": format!("audit/blobs/{id}/role-map.json"),
            "afterSha": crate::console::sha256_hex(role_map_current.unwrap_or("")),
        });
        let cfg_entry = json!({
            "file": "config/config.json",
            "backup": format!("audit/blobs/{id}/config.json"),
            "afterSha": crate::console::sha256_hex(config_current),
            "afterRegistryFreeSha": crate::console::config_registry_free_sha(config_current),
        });
        let files = if role_map_first {
            json!([role_entry, cfg_entry])
        } else {
            json!([cfg_entry, role_entry])
        };
        let entry = json!({
            "id": id,
            "ts": 1000,
            "method": "POST",
            "path": "/api/socialV2/config",
            "status": 200,
            "files": files,
        });
        std::fs::write(root.0.join("data/audit/audit.jsonl"), format!("{entry}\n")).unwrap();
    }

    #[test]
    fn commit_audit_undo_multi_file_409_refusal_leaves_earlier_role_map_untouched() {
        // Case 1: keep_registry_values conflict (409) when role-map is listed before config.
        let (root_keep, ctx_keep) = fixture(
            "audit-undo-409-keep",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "socialV2": { "reply": { "wakeQuietMs": 8000 } }
            }),
        );
        let cfg_current = std::fs::read_to_string(root_keep.0.join("config/config.json")).unwrap();
        let mut cfg_backup_val: Value = serde_json::from_str(&cfg_current).unwrap();
        cfg_backup_val["socialV2"] = json!({ "reply": false });
        let cfg_backup = serde_json::to_string_pretty(&cfg_backup_val).unwrap();
        let role_current = r#"{"default":"current-role"}"#;
        let role_backup = r#"{"default":"restored-role"}"#;
        seed_multi_file_audit(
            &root_keep,
            "tx-keep-409",
            role_backup,
            Some(role_current),
            &cfg_backup,
            &cfg_current,
            true,
        );
        let err = commit_audit_undo(&ctx_keep, &json!({ "id": "tx-keep-409" }).to_string())
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(err.message.contains("reply.wakeQuietMs"), "{}", err.message);
        assert_eq!(
            std::fs::read_to_string(root_keep.0.join("data/role-map.json")).unwrap(),
            role_current,
            "earlier role-map.json must stay untouched when config fails with 409"
        );
        assert_eq!(
            std::fs::read_to_string(root_keep.0.join("config/config.json")).unwrap(),
            cfg_current
        );

        // Case 2: missing security confirmation (409) when role-map is listed before config.
        let (root_sec, ctx_sec) = fixture(
            "audit-undo-409-confirm",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] }
            }),
        );
        let sec_current = std::fs::read_to_string(root_sec.0.join("config/config.json")).unwrap();
        let mut sec_backup_val: Value = serde_json::from_str(&sec_current).unwrap();
        sec_backup_val["allow"]["groups"] = json!([42, 43]);
        let sec_backup = serde_json::to_string_pretty(&sec_backup_val).unwrap();
        seed_multi_file_audit(
            &root_sec,
            "tx-confirm-409",
            role_backup,
            Some(role_current),
            &sec_backup,
            &sec_current,
            true,
        );
        let err = commit_audit_undo(&ctx_sec, &json!({ "id": "tx-confirm-409" }).to_string())
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert!(err.message.contains("allow.groups"), "{}", err.message);
        assert_eq!(
            std::fs::read_to_string(root_sec.0.join("data/role-map.json")).unwrap(),
            role_current,
            "earlier role-map.json must stay untouched when confirmation is missing"
        );
        assert_eq!(
            std::fs::read_to_string(root_sec.0.join("config/config.json")).unwrap(),
            sec_current
        );
    }

    #[test]
    fn commit_audit_undo_multi_file_later_write_failure_rolls_back_first_file() {
        let _guard = UndoWriteFailGuard;

        // Order A: role-map written first (existing), config fails second -> role-map restored.
        let (root_a, ctx_a) = fixture(
            "audit-undo-rb-role",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "notes": "current"
            }),
        );
        let cfg_current = std::fs::read_to_string(root_a.0.join("config/config.json")).unwrap();
        let mut cfg_backup_val: Value = serde_json::from_str(&cfg_current).unwrap();
        cfg_backup_val["notes"] = json!("restored");
        let cfg_backup = serde_json::to_string_pretty(&cfg_backup_val).unwrap();
        let role_current = r#"{"default":"current-role"}"#;
        let role_backup = r#"{"default":"restored-role"}"#;
        seed_multi_file_audit(
            &root_a,
            "tx-rb-role",
            role_backup,
            Some(role_current),
            &cfg_backup,
            &cfg_current,
            true,
        );
        ISOLATED_UNDO_WRITE_FAIL.with(|slot| {
            *slot.borrow_mut() = Some((
                "config/config.json".into(),
                "injected config failure".into(),
            ))
        });
        let err = commit_audit_undo(&ctx_a, &json!({ "id": "tx-rb-role" }).to_string())
            .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("injected config failure"), "{}", err.message);
        assert!(err.config_diff.is_none());
        assert_eq!(
            std::fs::read_to_string(root_a.0.join("data/role-map.json")).unwrap(),
            role_current,
            "first written role-map.json must roll back to its pre-undo bytes"
        );
        assert_eq!(
            std::fs::read_to_string(root_a.0.join("config/config.json")).unwrap(),
            cfg_current
        );

        // Order A2: role-map absent before undo, written first, config fails second -> removed.
        seed_multi_file_audit(
            &root_a,
            "tx-rb-remove",
            role_backup,
            None,
            &cfg_backup,
            &cfg_current,
            true,
        );
        ISOLATED_UNDO_WRITE_FAIL.with(|slot| {
            *slot.borrow_mut() = Some((
                "config/config.json".into(),
                "injected config failure".into(),
            ))
        });
        let err = commit_audit_undo(&ctx_a, &json!({ "id": "tx-rb-remove" }).to_string())
            .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(
            !root_a.0.join("data/role-map.json").exists(),
            "role-map.json that did not exist before undo must be removed on rollback"
        );

        // Order B: config written first (with wake change), role-map fails second -> config rolled
        // back and wake reconciliation skipped.
        let (root_b, ctx_b) = fixture(
            "audit-undo-rb-cfg",
            json!({
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "socialV2": { "wake": { "recommendedPoke": false } },
                "notes": "current"
            }),
        );
        let plan_before = crate::wake::default_wake_plan(&ctx_b.load_config().unwrap());
        assert_eq!(plan_before["triggers"]["poke"], false);
        ctx_b
            .write_atomic(
                RuntimeArea::Data,
                "social-v2.json",
                json!({
                    "paused": false,
                    "conversations": { "group:42": { "wakeConfig": plan_before } }
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let cfg_b_current = std::fs::read_to_string(root_b.0.join("config/config.json")).unwrap();
        let mut cfg_b_backup_val: Value = serde_json::from_str(&cfg_b_current).unwrap();
        cfg_b_backup_val["socialV2"]["wake"]["recommendedPoke"] = json!(true);
        cfg_b_backup_val["notes"] = json!("restored");
        let cfg_b_backup = serde_json::to_string_pretty(&cfg_b_backup_val).unwrap();
        seed_multi_file_audit(
            &root_b,
            "tx-rb-cfg",
            role_backup,
            Some(role_current),
            &cfg_b_backup,
            &cfg_b_current,
            false,
        );
        ISOLATED_UNDO_WRITE_FAIL.with(|slot| {
            *slot.borrow_mut() =
                Some(("data/role-map.json".into(), "injected role-map failure".into()))
        });
        let err = commit_audit_undo(&ctx_b, &json!({ "id": "tx-rb-cfg" }).to_string())
            .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("injected role-map failure"), "{}", err.message);
        assert!(
            err.config_diff.is_none(),
            "rolled-back config.json must not report a published ConfigDiff"
        );
        assert_eq!(
            std::fs::read_to_string(root_b.0.join("config/config.json")).unwrap(),
            cfg_b_current,
            "first written config.json must roll back to its pre-undo bytes"
        );
        assert_eq!(
            std::fs::read_to_string(root_b.0.join("data/role-map.json")).unwrap(),
            role_current
        );
        assert_eq!(
            ctx_b.read_social_state().unwrap()["conversations"]["group:42"]["wakeConfig"]
                ["triggers"]["poke"],
            false,
            "wake reconciliation must not run when a later file write fails"
        );
        assert!(!root_b
            .0
            .join("data")
            .join(social_state::PENDING_REVOCATIONS)
            .exists());
    }

    #[test]
    fn commit_audit_undo_multi_file_happy_path_restores_all_files() {
        let live_token = "fixture-config-tx-token-abcdefgh";
        let (root, context) = fixture(
            "audit-undo-multi-happy",
            json!({
                "consoleToken": live_token,
                "ownerQQ": 7001,
                "allow": { "groups": [42], "private": [7001] },
                "socialV2": { "wake": { "recommendedPoke": false } },
                "notes": "after-edit"
            }),
        );
        let plan_before = crate::wake::default_wake_plan(&context.load_config().unwrap());
        assert_eq!(plan_before["triggers"]["poke"], false);
        context
            .write_atomic(
                RuntimeArea::Data,
                "social-v2.json",
                json!({
                    "paused": false,
                    "conversations": { "group:42": { "wakeConfig": plan_before } }
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let cfg_current = std::fs::read_to_string(root.0.join("config/config.json")).unwrap();
        let mut cfg_backup_val: Value = serde_json::from_str(&cfg_current).unwrap();
        cfg_backup_val["consoleToken"] = json!("[redacted]");
        cfg_backup_val["socialV2"]["wake"]["recommendedPoke"] = json!(true);
        cfg_backup_val["notes"] = json!("before-edit");
        let cfg_backup = serde_json::to_string_pretty(&cfg_backup_val).unwrap();
        let role_current = r#"{"default":"after-role"}"#;
        let role_backup = r#"{"default":"before-role"}"#;
        seed_multi_file_audit(
            &root,
            "tx-multi-happy",
            role_backup,
            Some(role_current),
            &cfg_backup,
            &cfg_current,
            true,
        );

        let outcome = commit_audit_undo(&context, &json!({ "id": "tx-multi-happy" }).to_string())
            .unwrap();
        assert_eq!(outcome.value["ok"], true);
        assert_eq!(
            outcome.value["restored"],
            json!(["data/role-map.json", "config/config.json"])
        );
        assert_eq!(
            std::fs::read_to_string(root.0.join("data/role-map.json")).unwrap(),
            role_backup
        );
        let restored_cfg: Value = serde_json::from_str(
            &std::fs::read_to_string(root.0.join("config/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(restored_cfg["notes"], "before-edit");
        assert_eq!(restored_cfg["consoleToken"], live_token);
        assert_eq!(restored_cfg["socialV2"]["wake"]["recommendedPoke"], true);
        let diff = outcome.config_diff.expect("multi-file undo returns ConfigDiff");
        assert_eq!(diff.before, cfg_current);
        assert_eq!(
            context.read_social_state().unwrap()["conversations"]["group:42"]["wakeConfig"]
                ["triggers"]["poke"],
            true,
            "wake reconciliation runs after all files are written"
        );
    }

    #[test]
    fn write_legacy_undo_targets_rolls_back_earlier_files_on_failure() {
        let _guard = UndoWriteFailGuard;
        let dir = TestDir::new("legacy-undo-rb");
        let first_path = dir.0.join("first.json");
        let second_path = dir.0.join("second.json");
        let first_key = first_path.to_string_lossy().to_string();
        let second_key = second_path.to_string_lossy().to_string();

        std::fs::write(&first_path, r#"{"state":"before"}"#).unwrap();
        LEGACY_UNDO_WRITE_FAIL.with(|slot| {
            *slot.borrow_mut() = Some((second_key.clone(), "disk full".into()));
        });
        let err = write_legacy_undo_targets(vec![
            PreparedLegacyUndo {
                file: first_key.clone(),
                old: r#"{"state":"restored"}"#.into(),
                snapshot: Some(br#"{"state":"before"}"#.to_vec()),
            },
            PreparedLegacyUndo {
                file: second_key.clone(),
                old: r#"{"role":"restored"}"#.into(),
                snapshot: None,
            },
        ])
        .unwrap_err();
        assert_eq!(err.status, 500);
        assert_eq!(
            std::fs::read_to_string(&first_path).unwrap(),
            r#"{"state":"before"}"#,
            "legacy rollback must restore pre-undo bytes of earlier written file"
        );
        assert!(!second_path.exists());

        // When an earlier file did not exist before undo, rollback removes it.
        LEGACY_UNDO_WRITE_FAIL.with(|slot| {
            *slot.borrow_mut() = Some((first_key.clone(), "disk full".into()));
        });
        let err = write_legacy_undo_targets(vec![
            PreparedLegacyUndo {
                file: second_key.clone(),
                old: r#"{"role":"restored"}"#.into(),
                snapshot: None,
            },
            PreparedLegacyUndo {
                file: first_key.clone(),
                old: r#"{"state":"restored"}"#.into(),
                snapshot: Some(br#"{"state":"before"}"#.to_vec()),
            },
        ])
        .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(!second_path.exists());

        // Happy path restores both files.
        let restored = write_legacy_undo_targets(vec![
            PreparedLegacyUndo {
                file: first_key.clone(),
                old: r#"{"state":"restored"}"#.into(),
                snapshot: Some(br#"{"state":"before"}"#.to_vec()),
            },
            PreparedLegacyUndo {
                file: second_key.clone(),
                old: r#"{"role":"restored"}"#.into(),
                snapshot: None,
            },
        ])
        .unwrap();
        assert_eq!(restored, vec![first_key, second_key]);
        assert_eq!(
            std::fs::read_to_string(&first_path).unwrap(),
            r#"{"state":"restored"}"#
        );
        assert_eq!(
            std::fs::read_to_string(&second_path).unwrap(),
            r#"{"role":"restored"}"#
        );
    }
}
