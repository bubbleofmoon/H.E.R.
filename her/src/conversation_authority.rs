//! Conversation authority, runtime mode and the owner's conversation.
use serde_json::Value;

use crate::runtime::RuntimeContext;

pub(crate) use crate::conversation_state::{
    ensure_or_rotate_agent_token as ensure_or_rotate, read_agent_token as token,
    revoke_agent_token_if_current as revoke_if_current, token_epoch as generation,
};

/// The owner's private conversation (`private:<ownerQQ>`) when `ownerQQ` is an integer.
pub(crate) fn owner_conversation(cfg: &Value) -> Option<String> {
    cfg.get("ownerQQ").and_then(Value::as_i64).map(|qq| format!("private:{qq}"))
}

/// Whether `presented` is the conversation's live agent token. State is read first, so unreadable
/// state fails even for an empty token.
pub(crate) fn holds_token(context: &RuntimeContext, key: &str, presented: &str) -> Result<bool, String> {
    let live = token(context, key)?;
    Ok(crate::console_auth::credential_eq(live.as_deref().unwrap_or(""), presented))
}

/// The owner's private conversation presenting its own live agent token.
pub(crate) fn owner_holds_token(context: &RuntimeContext, cfg: &Value, key: &str, presented: &str) -> Result<bool, String> {
    let holds = holds_token(context, key, presented)?;
    Ok(holds && owner_conversation(cfg).as_deref() == Some(key))
}

/// Revoke only the admitted token and return the new generation if it rotated.
pub(crate) fn revoke(context: &RuntimeContext, key: &str, expected: &str) -> Result<Option<String>, String> {
    revoke_if_current(context, key, expected).map(|token| token.as_deref().map(generation))
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
