//! Admission-scoped capabilities over HER's existing handlers, without ambient MCP sessions.
use crate::runtime::RuntimeContext;
use genai::chat::{Tool, ToolCall};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Tools {
    definitions: Vec<Value>,
    authority: Option<Authority>,
}

#[derive(Clone)]
struct Authority {
    context: RuntimeContext,
    conversation: String,
    generation: String,
    token: String,
    mode: String,
}

// Only confirmed revocation cancels a turn; unreadable authority state stays a failure.
pub(super) enum CheckError {
    Revoked(&'static str),
    Unavailable(String),
}

impl From<CheckError> for String {
    fn from(error: CheckError) -> Self {
        match error {
            CheckError::Revoked(reason) => reason.into(),
            CheckError::Unavailable(reason) => reason,
        }
    }
}

impl Tools {
    pub fn capture(
        context: &RuntimeContext,
        cfg: &Value,
        conversation: &str,
        generation: &str,
    ) -> Result<Self, String> {
        let token = crate::conversation_authority::token(context, conversation)?
            .ok_or("harness tool authority is unavailable")?;
        let authority = Authority {
            context: context.clone(),
            conversation: conversation.into(),
            generation: generation.into(),
            token,
            mode: crate::conversation_authority::mode(context),
        };
        authority.check().map_err(String::from)?;
        Ok(Self {
            definitions: definitions(cfg, conversation),
            authority: Some(authority),
        })
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            definitions: Vec::new(),
            authority: None,
        }
    }

    pub(super) fn definitions(&self) -> &[Value] {
        &self.definitions
    }

    pub(super) fn check(&self) -> Result<(), CheckError> {
        self.authority
            .as_ref()
            .map(Authority::check)
            .unwrap_or(Ok(()))
    }

    pub(super) fn revoke(&self) -> Result<(), String> {
        if let Some(a) = &self.authority {
            crate::conversation_authority::revoke(&a.context, &a.conversation, &a.token)?;
        }
        Ok(())
    }

    pub(super) fn pause(&self, turn: &str, reason: &str, detail: &str) -> Result<(), String> {
        let revoked = self.revoke();
        if let Some(a) = &self.authority {
            crate::turn_ledger::Recorder::attach(&a.context, turn, &a.conversation)
                .unknown(reason, detail)?;
        }
        revoked
    }

    pub(super) fn matches(&self, conversation: &str, generation: &str) -> bool {
        self.authority
            .as_ref()
            .is_none_or(|a| a.conversation == conversation && a.generation == generation)
    }

    pub(super) fn instruction(&self) -> String {
        self.authority.as_ref().map(|a| format!(
            "HER tool session: {}. For tools with key/token fields, use this key and an empty token string; HER binds it to this admitted turn. A successful QQ send tool already delivers the reply.", a.conversation
        )).unwrap_or_default()
    }

    pub(super) fn model_tools(&self) -> Vec<Tool> {
        self.definitions
            .iter()
            .map(|d| {
                Tool::new(d["name"].as_str().unwrap_or_default())
                    .with_description(d["description"].as_str().unwrap_or_default())
                    .with_schema(d["inputSchema"].clone())
            })
            .collect()
    }

    /// Validate the captured schema and authority before recording the start intent.
    pub(super) fn prepare(&self, call: &ToolCall) -> Result<Value, Value> {
        let mut args = match crate::mcp::prepare_call(
            &call.fn_name,
            Some(&call.fn_arguments),
            &self.definitions,
        ) {
            Ok(args) => args,
            Err(outcome) => return Err(outcome),
        };
        let Some(authority) = &self.authority else {
            return Err(error("harness tool authority is unavailable"));
        };
        let current = match authority
            .check()
            .map_err(String::from)
            .and_then(|_| authority.context.load_config().map_err(str::to_string))
        {
            Ok(cfg) => cfg,
            Err(_) => return Err(error("harness tool authority changed")),
        };
        if !definitions(&current, &authority.conversation)
            .iter()
            .any(|d| d["name"] == call.fn_name)
        {
            return Err(error("harness tool is no longer enabled"));
        }
        // Validate before applying defaults, as the MCP server does. Never consult QQ_SESSION_*.
        let definition = self
            .definitions
            .iter()
            .find(|d| d["name"] == call.fn_name)
            .unwrap();
        for (field, bound) in [
            ("key", &authority.conversation),
            ("token", &authority.token),
        ] {
            if definition["inputSchema"]["properties"].get(field).is_some() {
                if field == "token"
                    && call.fn_name == "qq_get_message_images"
                    && !args
                        .get(field)
                        .is_some_and(|v| v.as_str().is_some_and(|v| !v.is_empty()))
                {
                    // Like the MCP route, omitted image tokens retain closed-agent access.
                    // The admitted generation is still checked before dispatch.
                    continue;
                }
                if args[field]
                    .as_str()
                    .is_some_and(|v| !v.is_empty() && v != bound)
                {
                    return Err(error(
                        "tool session does not match the admitted conversation",
                    ));
                }
                args[field] = json!(bound);
            }
        }
        Ok(args)
    }

    /// Recheck after scheduling the blocking worker; QQ effects also reauthorize in the console.
    pub(super) fn dispatch(&self, call: &ToolCall, args: &Value) -> Value {
        let Some(authority) = &self.authority else {
            return error("harness tool authority is unavailable");
        };
        if authority.check().is_err() {
            return error("harness tool authority changed");
        }
        let Ok(cfg) = authority.context.load_config() else {
            return error("harness tool authority changed");
        };
        if !definitions(&cfg, &authority.conversation)
            .iter()
            .any(|d| d["name"] == call.fn_name)
        {
            return error("harness tool is no longer enabled");
        }
        match call.fn_name.as_str() {
            "web_search" | "web_fetch" | "snowluma_status" => {
                crate::aux_mcp::call_tool(&call.fn_name, args, &authority.context)
            }
            _ => crate::mcp::call_prepared_recorded(&call.fn_name, args, &authority.context),
        }
    }
}

impl Authority {
    fn check(&self) -> Result<(), CheckError> {
        let cfg = self
            .context
            .load_config()
            .map_err(|reason| CheckError::Unavailable(reason.into()))?;
        let mode = crate::conversation_authority::mode(&self.context);
        if mode != self.mode || !crate::chat::allowed_in_mode(&cfg, &mode, &self.conversation) {
            return Err(CheckError::Revoked(
                "harness conversation authority changed",
            ));
        }
        let token = crate::conversation_authority::token(&self.context, &self.conversation)
            .map_err(CheckError::Unavailable)?;
        if token.as_deref() != Some(&self.token)
            || crate::conversation_authority::generation(&self.token) != self.generation
        {
            return Err(CheckError::Revoked(
                "harness conversation generation changed",
            ));
        }
        Ok(())
    }
}

fn definitions(cfg: &Value, conversation: &str) -> Vec<Value> {
    let mut tools = crate::mcp::tool_list(cfg)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    tools.extend(crate::aux_mcp::tool_list());
    tools
        .into_iter()
        .filter(|d| {
            crate::chat::is_owner_key(cfg, conversation)
                || !matches!(
                    d["name"].as_str(),
                    Some("memory_acl" | "settings_get" | "settings_set")
                )
        })
        .collect()
}

pub(super) fn error(reason: &str) -> Value {
    json!({"content":[{"type":"text","text":reason}],"isError":true})
}
