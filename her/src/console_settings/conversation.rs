//! #218: the conversation scope of settings (`GET /api/settings/conversations`,
//! `POST /api/settings/conversation/set`, `POST /api/settings/conversation/reset`). Undo and history are the settings
//! routes' own (`POST /api/settings/undo`, `GET /api/settings/history?conversation=`). Bodies name the conversation as
//! `conversation` and the setting as `setting`. The settings store keeps the desired engine; the switch also drains
//! and moves the active generation.

use super::{applied_reply, body_fields, setting_field};
use crate::console_front::AdminCaller;
use crate::runtime::RuntimeContext;
use serde_json::{json, Map, Value};

/// The refusal of a write to a conversation the bridge does not answer.
const NOT_ADMITTED_ERROR: &str = "conversation is not admitted; admit it first";

const SWITCH_NOTE: &str = "Switch waits for the current turn and moves this conversation. Engine reset and undo apply before its next turn. The model route applies from the next harness turn.";

/// The `conversation` member: a conversation key, canonicalized (`group:0042` is `group:42`), else 400.
fn conversation_field(fields: &Map<String, Value>) -> Result<String, (u16, String)> {
    fields
        .get("conversation")
        .and_then(Value::as_str)
        .and_then(crate::settings::canonical_conversation)
        .ok_or((400, crate::settings::CONVERSATION_ERROR.to_string()))
}

/// Every admitted conversation under `cfg`: the admitted ids of both allow lists and the owner's private chat, as
/// canonical keys, sorted. The model route list (`console_settings/models.rs`) lists the same conversations.
pub(super) fn admitted_conversations(cfg: &Value) -> Vec<String> {
    let mut keys: Vec<String> = ["group", "private"]
        .into_iter()
        .flat_map(|kind| {
            crate::social_state::admitted_ids(cfg, kind)
                .into_iter()
                .filter_map(move |id| crate::settings::canonical_conversation(&format!("{kind}:{id}")))
        })
        .collect();
    let owner = format!("private:{}", crate::chat::owner_qq(cfg));
    if crate::settings::conversation_admitted(cfg, &owner) && crate::settings::canonical_conversation(&owner).as_deref() == Some(owner.as_str()) {
        keys.push(owner);
    }
    keys.sort();
    keys.dedup();
    keys
}

fn loaded_config(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    context.load_config().map_err(|_| (503, "runtime config is unavailable".to_string()))
}

/// The reply of a conversation set or reset: the global set route's reply with the conversation.
fn conversation_reply(conversation: &str, setting: &str, applied: crate::settings::Applied) -> Value {
    let mut reply = applied_reply(setting, applied);
    reply["conversation"] = json!(conversation);
    reply
}

/// `GET /api/settings/conversations` (`settings::conversation_view`), with `note` and, per conversation, `engineToday`:
/// the desired engine for its next admission (`Engine::read_choice`), or `refused` if the choice is unreadable.
pub(super) fn list(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    let cfg = loaded_config(context)?;
    let mode = crate::social_state::mode(context);
    let mut reply = crate::settings::conversation_view(context, &admitted_conversations(&cfg))?;
    if let Some(rows) = reply["conversations"].as_array_mut() {
        for row in rows {
            let conversation = row["conversation"].as_str().unwrap_or_default().to_string();
            let today = crate::turn_engine::Engine::read_choice(context, &cfg, &conversation, &mode).map_or("refused", crate::turn_engine::Engine::id);
            row["engineToday"] = json!(today);
        }
    }
    reply["note"] = json!(SWITCH_NOTE);
    Ok(reply)
}

/// `{"conversation": <key>, "setting": <conversation setting key>, "value": <value>}`, and
/// `{"conversation", "setting"}` for a reset. Order of refusals: the body, the conversation key and an unknown setting
/// (400); then, under the configuration gate, a conversation that is not admitted (409) and a refused value (400).
fn write(caller: AdminCaller<'_>, body: &str, reset: bool) -> Result<Value, (u16, String)> {
    let context = caller.context();
    let allowed: &[&str] = if reset { &["conversation", "setting"] } else { &["conversation", "setting", "value"] };
    let fields = body_fields(body, allowed)?;
    let conversation = conversation_field(&fields)?;
    let setting = setting_field(&fields)?;
    crate::settings::find_conversation(setting).ok_or((400, "unknown setting".to_string()))?;
    let value = if reset { None } else { Some(fields.get("value").ok_or((400, "value is required".to_string()))?) };
    let _gate = caller.gate()?;
    if !crate::settings::conversation_admitted(&loaded_config(context)?, &conversation) {
        return Err((409, NOT_ADMITTED_ERROR.to_string()));
    }
    let applied = match value {
        Some(value) => crate::settings::apply_conversation(context, &conversation, setting, value, "administrator", "console")?,
        None => crate::settings::reset_conversation(context, &conversation, setting, "administrator")?,
    };
    Ok(conversation_reply(&conversation, setting, applied))
}

/// `POST /api/settings/conversation/set`.
pub(super) fn set(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    write(caller, body, false)
}

/// `POST /api/settings/conversation/reset`: stores the global setting's default for that conversation.
pub(super) fn reset(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    write(caller, body, true)
}

/// Wait outside the configuration gate so the admitted turn can finish its sends.
pub(super) fn switch(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    use crate::turn_engine::Engine;
    let context = caller.context();
    let fields = body_fields(body, &["conversation", "engine"])?;
    let key = conversation_field(&fields)?;
    let value = fields.get("engine").ok_or((400, "engine is required".to_string()))?;
    let requested = Engine::parse(value).map_err(|error| (400, error))?;
    let _gates = crate::engine_switch::drain(context, &key)?;
    let setting = &crate::settings::CONVERSATION_ENGINE;
    let (mode, applied, rollback) = {
        let _config = caller.gate()?;
        let cfg = loaded_config(context)?;
        let mode = crate::social_state::mode(context);
        if !crate::settings::conversation_admitted(&cfg, &key)
            || !crate::social_state::access_allowed(&cfg, &mode, &key)
        {
            return Err((409, NOT_ADMITTED_ERROR.into()));
        }
        if mode == "reserved2" && !crate::social_state::inbound_admitted(&cfg, &key) {
            return Err((409, NOT_ADMITTED_ERROR.into()));
        }
        if !Engine::Harness.supports(&mode) {
            return Err((409, crate::turn_engine::UNSUPPORTED_MODE.into()));
        }
        let (applied, rollback) = crate::settings::apply_switch_choice(context, &key, value)?;
        (mode, applied, rollback)
    };
    let result = crate::engine_switch::prepare(context, &key, &mode, requested);
    if result.is_err() {
        if let Some(rollback) = rollback {
            let _config = caller.gate()?;
            crate::settings::restore_switch_choice(context, &key, rollback)?;
        }
    }
    result?;
    Ok(conversation_reply(&key, setting.key, applied))
}
