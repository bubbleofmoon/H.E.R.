//! Conversation authority: a conversation's agent token (its SHA-256 epoch is the generation),
//! revocation and rotation, the runtime mode and the owner's conversation. Social-state storage and
//! the allow-lists still live in `console`.
use serde_json::{json, Value};

use crate::runtime::RuntimeContext;

/// The owner's private conversation (`private:<ownerQQ>`) when `ownerQQ` is an integer.
pub(crate) fn owner_conversation(cfg: &Value) -> Option<String> {
    cfg.get("ownerQQ").and_then(Value::as_i64).map(|qq| format!("private:{qq}"))
}

/// The conversation's social-state record, if any.
pub(crate) fn conversation(context: &RuntimeContext, key: &str) -> Result<Option<Value>, String> {
    let state = crate::console::load_social_state_for(context).map_err(|(_, message)| message)?;
    Ok(state.get("conversations").and_then(|c| c.get(key)).cloned())
}

/// The conversation's current agent token, if it has one.
pub(crate) fn token(context: &RuntimeContext, key: &str) -> Result<Option<String>, String> {
    Ok(conversation(context, key)?
        .as_ref()
        .and_then(|c| c.get("agentToken"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string))
}

/// Whether `presented` is the conversation's live agent token. State is read first, so unreadable
/// state fails even for an empty token.
pub(crate) fn holds_token(context: &RuntimeContext, key: &str, presented: &str) -> Result<bool, String> {
    let live = token(context, key)?;
    Ok(!presented.is_empty() && live.as_deref() == Some(presented))
}

/// The owner's private conversation presenting its own live agent token.
pub(crate) fn owner_holds_token(context: &RuntimeContext, cfg: &Value, key: &str, presented: &str) -> Result<bool, String> {
    let holds = holds_token(context, key, presented)?;
    Ok(holds && owner_conversation(cfg).as_deref() == Some(key))
}

/// The generation an agent token belongs to.
pub(crate) fn generation(token: &str) -> String {
    crate::send_outbox::token_epoch(token)
}

/// Revoke only the admitted token, preserving any replacement turn's authority.
pub(crate) fn revoke(context: &RuntimeContext, key: &str, expected: &str) -> Result<(), String> {
    // A timeout of an old turn must never rotate a replacement turn's authority.
    crate::console::mutate_social_state_for(context, |state| {
        if state["conversations"][key]["agentToken"].as_str() == Some(expected) {
            state["conversations"][key]["agentToken"] = json!(crate::console::fresh_agent_token());
        }
        Ok(())
    }).map_err(|(_, message)| message)
}


/// Make sure the conversation has an agent token (creating the conversation record after a
/// reset removed it), or replace it when `rotate` is set. Rotation is how a cancelled, replaced
/// or unverifiable turn loses the right to run tools or send: its token stops matching, so every
/// token-checked route refuses it. Returns the token now in force.
pub(crate) fn ensure_or_rotate(
    context: &RuntimeContext,
    key: &str,
    rotate: bool,
) -> Result<String, String> {
    crate::console::mutate_social_state_for(context, |state| {
        if !state.is_object() {
            return Err((500, "runtime data is invalid".to_string()));
        }
        let conversations = state
            .as_object_mut()
            .expect("object checked above")
            .entry("conversations".to_string())
            .or_insert_with(|| json!({}));
        let conversations = conversations
            .as_object_mut()
            .ok_or((500, "runtime data is invalid".to_string()))?;
        let conversation = conversations
            .entry(key.to_string())
            .or_insert_with(|| crate::console::new_conversation(context));
        let conversation = conversation
            .as_object_mut()
            .ok_or((500, "runtime data is invalid".to_string()))?;
        let current = conversation
            .get("agentToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        match current {
            Some(token) if !rotate => Ok(token),
            _ => {
                let token = crate::console::fresh_agent_token();
                conversation.insert("agentToken".to_string(), json!(token));
                Ok(token)
            }
        }
    })
    .map_err(|(_, message)| message)
}

/// The mode name a new session policy is computed from (default `reserved2`).
pub(crate) fn mode(context: &RuntimeContext) -> String {
    crate::console::current_mode_for(context).unwrap_or_else(|_| "reserved2".to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::owner_conversation;

    #[test]
    fn owner_conversation_is_present_only_for_an_integer_owner_qq() {
        for (cfg, expected) in [
            (json!({"ownerQQ": 1}), Some("private:1")),
            (json!({}), None),
            (json!({"ownerQQ": "1"}), None),
            (json!({"ownerQQ": 1.5}), None),
        ] {
            assert_eq!(owner_conversation(&cfg).as_deref(), expected, "{cfg}");
        }
    }
}
