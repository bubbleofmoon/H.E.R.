//! Admission-scoped capabilities over HER's existing handlers, without ambient MCP sessions.
//! The turn's mode profile (`profile.rs`) decides which tools it may call.
use super::profile::Profile;
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
    /// The participation mode the pipeline admitted the turn under.
    mode: String,
    profile: Profile,
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
    /// Capture the tools of a turn the pipeline admitted under `mode`. Refused: a mode the harness
    /// does not run, a conversation no mode profile serves, and a current mode, owner or
    /// conversation authority other than the admitted one.
    pub fn capture(
        context: &RuntimeContext,
        cfg: &Value,
        mode: &str,
        conversation: &str,
        generation: &str,
    ) -> Result<Self, String> {
        if !crate::turn_engine::Adapter::Harness.supports(mode) {
            return Err(crate::turn_engine::UNSUPPORTED_MODE.into());
        }
        let profile = Profile::of_conversation(cfg, mode, conversation)
            .ok_or("harness has no tool menu for this conversation in this mode")?;
        let token = crate::conversation_authority::token(context, conversation)?
            .ok_or("harness tool authority is unavailable")?;
        let authority = Authority {
            context: context.clone(),
            conversation: conversation.into(),
            generation: generation.into(),
            token,
            mode: mode.into(),
            profile,
        };
        authority.check().map_err(String::from)?;
        Ok(Self {
            definitions: definitions(cfg, profile),
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
        self.authority.as_ref().map(|a| session_instruction(&a.conversation)).unwrap_or_default()
    }

    /// The tools offered to the next model request: the captured menu, narrowed by the current
    /// authority (mode, allow-list, owner, token) and tool switches. The harness asks before every
    /// model request, so a switch turned off mid-turn leaves the menu before the next request; a
    /// switch turned back on restores only a tool the turn captured.
    pub(super) fn model_tools(&self) -> Result<Vec<Tool>, CheckError> {
        let current = match &self.authority {
            Some(authority) => {
                authority.check()?;
                let cfg = authority
                    .context
                    .load_config()
                    .map_err(|reason| CheckError::Unavailable(reason.into()))?;
                Some(definitions(&cfg, authority.profile))
            }
            None => None,
        };
        Ok(self
            .definitions
            .iter()
            .filter(|d| {
                current
                    .as_ref()
                    .is_none_or(|current| current.iter().any(|c| c["name"] == d["name"]))
            })
            .map(|d| {
                Tool::new(d["name"].as_str().unwrap_or_default())
                    .with_description(d["description"].as_str().unwrap_or_default())
                    .with_schema(d["inputSchema"].clone())
            })
            .collect())
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
            return Err(error("tool authority is unavailable"));
        };
        let current = match authority
            .check()
            .map_err(String::from)
            .and_then(|_| authority.context.load_config().map_err(str::to_string))
        {
            Ok(cfg) => cfg,
            Err(_) => return Err(error("tool authority changed")),
        };
        if !definitions(&current, authority.profile)
            .iter()
            .any(|d| d["name"] == call.fn_name)
        {
            return Err(error("tool is no longer enabled"));
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
            return error("tool authority is unavailable");
        };
        if authority.check().is_err() {
            return error("tool authority changed");
        }
        let Ok(cfg) = authority.context.load_config() else {
            return error("tool authority changed");
        };
        if !definitions(&cfg, authority.profile)
            .iter()
            .any(|d| d["name"] == call.fn_name)
        {
            return error("tool is no longer enabled");
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
    /// The current mode, allow-list, owner (through the profile) and token must all still be the
    /// admitted ones.
    fn check(&self) -> Result<(), CheckError> {
        let cfg = self
            .context
            .load_config()
            .map_err(|reason| CheckError::Unavailable(reason.into()))?;
        let mode = crate::conversation_authority::mode(&self.context);
        if mode != self.mode
            || !crate::chat::allowed_in_mode(&cfg, &mode, &self.conversation)
            || Profile::of_conversation(&cfg, &mode, &self.conversation) != Some(self.profile)
        {
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

/// The definitions of `profile`'s menu that `cfg` registers, in registration order: a tool the
/// configuration switches off (#153) drops out of every menu, the direct-send switches included.
fn definitions(cfg: &Value, profile: Profile) -> Vec<Value> {
    let menu = profile.menu();
    let mut tools = crate::mcp::tool_list(cfg)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    tools.extend(crate::aux_mcp::tool_list());
    tools
        .into_iter()
        .filter(|d| {
            d["name"].as_str().is_some_and(|name| {
                menu.contains(&name)
                    && direct_send_switch(name).is_none_or(|flag| {
                        cfg.pointer(&format!("/socialV2/tools/{flag}"))
                            .and_then(Value::as_bool)
                            != Some(false)
                    })
            })
        })
        .collect()
}

/// The `socialV2.tools` switch the console checks for a token-bound direct send
/// (`console_agent::direct_inner`). The shared MCP registration (`qq_tools::registered`) does not
/// gate these three tools, and the harness always binds the turn's token into them, so the menu
/// applies the switch. As in the console, only an explicit `false` switches a send off.
fn direct_send_switch(name: &str) -> Option<&'static str> {
    Some(match name {
        "qq_send_private_message" => "sendPrivate",
        "qq_send_group_message" => "sendGroup",
        "qq_reply" => "reply",
        _ => return None,
    })
}

pub(super) fn error(reason: &str) -> Value {
    json!({"content":[{"type":"text","text":reason}],"isError":true})
}

/// The system line of an admitted harness turn; the model reads it.
pub(crate) fn session_instruction(conversation: &str) -> String {
    format!("Tool session: {conversation}. For tools with key/token fields, use this key and an empty token string; the bridge binds it to this turn. A successful QQ send tool already delivers the reply.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_KEY: &str = "private:8808";
    const GROUP_KEY: &str = "group:420042";

    /// A selected runtime: owner 8808, group 420042 and the owner's private chat allowed.
    struct Fixture {
        root: std::path::PathBuf,
        context: RuntimeContext,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new(label: &str, mode: &str) -> Self {
            let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-160-{label}-{}",
                crate::turn_ledger::mint_id("")
            )));
            for dir in ["config", "data", "resources"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            let cfg = json!({
                "ownerQQ": 8808, "consoleToken": "synthetic-console-token-160",
                "allow": {"groups": [420042], "private": [8808]}
            });
            std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(
                root.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:43161","model":"http://127.0.0.1:43162","onebot":"http://127.0.0.1:43163"}"#,
            )
            .unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            let fixture = Self { root, context };
            fixture.set_mode(mode);
            fixture
        }

        fn set_mode(&self, mode: &str) {
            std::fs::write(
                self.root.join("data/mode.json"),
                json!({ "mode": mode }).to_string(),
            )
            .unwrap();
        }

        fn edit_config(&self, edit: impl FnOnce(&mut Value)) {
            let path = self.root.join("config/config.json");
            let mut cfg: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            edit(&mut cfg);
            std::fs::write(path, cfg.to_string()).unwrap();
        }

        /// The conversation's generation, minting its agent token when it has none.
        fn generation(&self, conversation: &str) -> String {
            let token =
                crate::conversation_authority::ensure_or_rotate(&self.context, conversation, false)
                    .unwrap();
            crate::conversation_authority::generation(&token)
        }

        fn capture(&self, mode: &str, conversation: &str) -> Result<Tools, String> {
            let generation = self.generation(conversation);
            let cfg = self.context.load_config().map_err(str::to_string)?;
            Tools::capture(&self.context, &cfg, mode, conversation, &generation)
        }
    }

    fn names(definitions: &[Value]) -> Vec<&str> {
        definitions
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect()
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            call_id: format!("call-{name}"),
            fn_name: name.into(),
            fn_arguments: arguments,
            thought_signatures: None,
        }
    }

    fn refusal_text(outcome: &Value) -> String {
        assert_eq!(outcome["isError"], true, "{outcome}");
        outcome["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// The table row, and what the default configuration (every tool registered) offers from it.
    fn assert_menu(profile: Profile, expected: &[&str]) {
        assert_eq!(profile.menu(), expected, "{profile:?} table");
        assert_eq!(
            names(&definitions(&json!({}), profile)),
            expected,
            "{profile:?} offered"
        );
    }

    #[test]
    fn menu_chat_lists_the_exact_tools_of_groups_and_other_private_chats() {
        assert_menu(
            Profile::Chat,
            &[
                "qq_status",
                "memory_write",
                "memory_search",
                "memory_index",
                "memory_forget",
                "web_search",
                "web_fetch",
                "snowluma_status",
            ],
        );
    }

    #[test]
    fn menu_owner_private_lists_the_exact_tools_of_the_owner_chat_in_chat_mode() {
        assert_menu(
            Profile::OwnerPrivate,
            &[
                "qq_status",
                "memory_write",
                "memory_search",
                "memory_index",
                "memory_forget",
                "memory_acl",
                "settings_get",
                "settings_set",
                "web_search",
                "web_fetch",
                "snowluma_status",
            ],
        );
    }

    #[test]
    fn menu_closed_agent_lists_the_exact_tools_of_the_admin_agent() {
        assert_menu(
            Profile::ClosedAgent,
            &[
                "qq_status",
                "qq_list_groups",
                "qq_get_group_members",
                "qq_get_group_history",
                "qq_send_private_message",
                "qq_get_message_images",
                "memory_write",
                "memory_search",
                "memory_index",
                "memory_forget",
                "memory_acl",
                "settings_get",
                "settings_set",
                "web_search",
                "web_fetch",
                "snowluma_status",
            ],
        );
    }

    #[test]
    fn menu_reserved2_lists_the_exact_tools_of_groups_and_other_private_chats() {
        assert_menu(
            Profile::Reserved2,
            &[
                "qq_status",
                "qq_send_group_message",
                "qq_reply",
                "qq_send_private_message",
                "qq_get_prompt",
                "qq_get_unread_messages",
                "qq_get_recent_messages",
                "qq_social_state",
                "qq_mark_read",
                "qq_set_wake_config",
                "qq_send_burst",
                "qq_send_message",
                "qq_send_poke",
                "qq_wait_for_messages",
                "qq_report_feedback",
                "qq_get_my_recent_messages",
                "qq_get_message_detail",
                "qq_get_active_members",
                "qq_memory_append",
                "qq_memory_query",
                "qq_memory_remove",
                "qq_memory_clear",
                "qq_slang_query",
                "qq_slang_submit",
                "qq_get_message_images",
                "qq_list_stickers",
                "qq_get_sticker_image",
                "qq_send_sticker",
                "qq_collect_sticker",
                "qq_send_voice",
                "qq_list_voices",
                "qq_get_self_image",
                "qq_sticker_note",
                "qq_set_sticker_remark",
                "qq_get_forward_msg",
                "memory_write",
                "memory_search",
                "memory_index",
                "memory_forget",
                "web_search",
                "web_fetch",
                "snowluma_status",
            ],
        );
    }

    #[test]
    fn menu_reserved2_owner_lists_the_exact_tools_of_the_owner_chat_in_reserved2() {
        assert_menu(
            Profile::Reserved2Owner,
            &[
                "qq_status",
                "qq_send_group_message",
                "qq_reply",
                "qq_send_private_message",
                "qq_get_prompt",
                "qq_get_unread_messages",
                "qq_get_recent_messages",
                "qq_social_state",
                "qq_mark_read",
                "qq_set_wake_config",
                "qq_send_burst",
                "qq_send_message",
                "qq_send_poke",
                "qq_wait_for_messages",
                "qq_report_feedback",
                "qq_get_my_recent_messages",
                "qq_get_message_detail",
                "qq_get_active_members",
                "qq_memory_append",
                "qq_memory_query",
                "qq_memory_remove",
                "qq_memory_clear",
                "qq_slang_query",
                "qq_slang_submit",
                "qq_get_message_images",
                "qq_list_stickers",
                "qq_get_sticker_image",
                "qq_send_sticker",
                "qq_collect_sticker",
                "qq_send_voice",
                "qq_list_voices",
                "qq_get_self_image",
                "qq_sticker_note",
                "qq_set_sticker_remark",
                "qq_get_forward_msg",
                "memory_write",
                "memory_search",
                "memory_index",
                "memory_forget",
                "memory_acl",
                "settings_get",
                "settings_set",
                "web_search",
                "web_fetch",
                "snowluma_status",
            ],
        );
    }

    #[test]
    fn menu_digest_offers_no_tool() {
        assert_menu(Profile::Digest, &[]);
    }

    #[test]
    fn menu_slang_learner_offers_only_the_read_only_web_tools() {
        assert_menu(Profile::SlangLearner, &["web_search", "web_fetch"]);
    }

    #[test]
    fn menu_no_profile_offers_a_dsh_local_tool_and_only_owner_profiles_offer_acl_or_settings() {
        let all = [
            Profile::Chat,
            Profile::OwnerPrivate,
            Profile::ClosedAgent,
            Profile::Reserved2,
            Profile::Reserved2Owner,
            Profile::Digest,
            Profile::SlangLearner,
        ];
        for profile in all {
            for name in profile.menu() {
                assert!(
                    name.starts_with("qq_")
                        || name.starts_with("memory_")
                        || name.starts_with("settings_")
                        || ["web_search", "web_fetch", "snowluma_status"].contains(name),
                    "{profile:?} offers {name}"
                );
            }
            for local in [
                "bash",
                "pwsh",
                "read",
                "read_image",
                "write",
                "edit",
                "str_replace_editor",
                "glob",
                "grep",
                "load_workspace_dependencies",
                "subagent",
                "subagent_codex",
                "plugin_manager",
                "read_mcp_resource",
                "schedule_create",
                "ask_user_question",
                "todo_write",
            ] {
                assert!(!profile.menu().contains(&local), "{profile:?} offers {local}");
            }
            let owner = matches!(
                profile,
                Profile::OwnerPrivate | Profile::ClosedAgent | Profile::Reserved2Owner
            );
            for owner_only in ["memory_acl", "settings_get", "settings_set"] {
                assert_eq!(
                    profile.menu().contains(&owner_only),
                    owner,
                    "{profile:?} and {owner_only}"
                );
            }
        }
    }

    #[test]
    fn menu_profile_follows_the_admitted_mode_and_the_configured_owner() {
        let cfg = json!({"ownerQQ": 8808});
        for (mode, conversation, expected) in [
            ("chat", GROUP_KEY, Some(Profile::Chat)),
            ("chat", "private:7707", Some(Profile::Chat)),
            ("chat", OWNER_KEY, Some(Profile::OwnerPrivate)),
            ("closed-agent", OWNER_KEY, Some(Profile::ClosedAgent)),
            ("closed-agent", GROUP_KEY, None),
            ("closed-agent", "private:7707", None),
            ("reserved2", GROUP_KEY, Some(Profile::Reserved2)),
            ("reserved2", OWNER_KEY, Some(Profile::Reserved2Owner)),
            ("reserved", GROUP_KEY, None),
            ("simulation", GROUP_KEY, None),
            ("", GROUP_KEY, None),
            ("Chat", GROUP_KEY, None),
        ] {
            assert_eq!(
                Profile::of_conversation(&cfg, mode, conversation),
                expected,
                "{mode} {conversation}"
            );
        }
        assert_eq!(
            Profile::of_conversation(&json!({}), "chat", OWNER_KEY),
            Some(Profile::Chat),
            "without a configured owner nobody gets an owner menu"
        );
    }

    #[test]
    fn menu_capture_refuses_another_mode_an_unserved_mode_or_conversation_and_an_unreadable_mode() {
        let fixture = Fixture::new("capture", "closed-agent");
        let refused = |mode: &str, conversation: &str| {
            fixture
                .capture(mode, conversation)
                .err()
                .unwrap_or_else(|| panic!("{mode} {conversation} was captured"))
        };
        assert_eq!(
            refused("chat", OWNER_KEY),
            "harness conversation authority changed",
            "admitted as chat while the runtime is in closed-agent"
        );
        assert_eq!(
            refused("closed-agent", GROUP_KEY),
            "harness has no tool menu for this conversation in this mode"
        );
        fixture.set_mode("reserved2");
        assert_eq!(
            refused("reserved2", GROUP_KEY),
            crate::turn_engine::UNSUPPORTED_MODE
        );
        fixture.set_mode("reserved");
        assert_eq!(
            refused("reserved", GROUP_KEY),
            crate::turn_engine::UNSUPPORTED_MODE
        );
        std::fs::write(fixture.root.join("data/mode.json"), "not json").unwrap();
        assert_eq!(
            refused("chat", GROUP_KEY),
            "harness conversation authority changed",
            "an unreadable mode never widens the menu"
        );
        std::fs::remove_file(fixture.root.join("data/mode.json")).unwrap();
        assert_eq!(
            refused("chat", GROUP_KEY),
            "harness conversation authority changed",
            "a missing mode file reads as reserved2"
        );
        fixture.set_mode("chat");
        let chat = fixture.capture("chat", GROUP_KEY).unwrap();
        assert_eq!(names(chat.definitions()), Profile::Chat.menu());
    }

    #[test]
    fn menu_off_menu_and_dsh_tool_names_are_refused_before_any_effect() {
        let fixture = Fixture::new("off-menu", "chat");
        let tools = fixture.capture("chat", GROUP_KEY).unwrap();
        for name in [
            "qq_send_group_message",
            "qq_reply",
            "qq_send_private_message",
            "qq_get_group_history",
            "qq_send_message",
            "qq_get_message_images",
            "memory_acl",
            "settings_set",
            "bash",
            "read",
            "plugin_manager",
            "ask_user_question",
            "todo_write",
            "mcp__snowluma__qq_status",
            "QQ_STATUS",
            "qq_status ",
            "",
        ] {
            let refusal = tools.prepare(&call(name, json!({}))).unwrap_err();
            assert_eq!(
                refusal_text(&refusal),
                format!("MCP error -32602: Tool {name} not found")
            );
            // Dispatch rechecks the menu itself: a call that skipped `prepare` has no effect either.
            assert_eq!(
                refusal_text(&tools.dispatch(&call(name, json!({})), &json!({}))),
                "tool is no longer enabled"
            );
        }
        assert!(
            tools.prepare(&call("qq_status", json!({}))).is_ok(),
            "the menu's own tools still pass"
        );
    }

    #[test]
    fn menu_a_mode_transition_revokes_the_admitted_tools_and_the_next_turn_gets_the_new_menu() {
        let fixture = Fixture::new("transition", "chat");
        let chat = fixture.capture("chat", OWNER_KEY).unwrap();
        assert_eq!(names(chat.definitions()), Profile::OwnerPrivate.menu());
        // Written directly, without the console's token rotation: the mode check alone revokes.
        fixture.set_mode("closed-agent");
        assert!(matches!(chat.check(), Err(CheckError::Revoked(_))));
        let status = call("qq_status", json!({}));
        assert_eq!(
            refusal_text(&chat.prepare(&status).unwrap_err()),
            "tool authority changed"
        );
        assert_eq!(
            refusal_text(&chat.dispatch(&status, &json!({}))),
            "tool authority changed"
        );
        let admin = fixture.capture("closed-agent", OWNER_KEY).unwrap();
        assert_eq!(names(admin.definitions()), Profile::ClosedAgent.menu());
        fixture.set_mode("chat");
        let send = call(
            "qq_send_private_message",
            json!({"userId": 8808, "message": "must not send"}),
        );
        assert_eq!(
            refusal_text(&admin.prepare(&send).unwrap_err()),
            "tool authority changed"
        );
        let next = fixture.capture("chat", OWNER_KEY).unwrap();
        assert_eq!(names(next.definitions()), Profile::OwnerPrivate.menu());
    }

    #[test]
    fn menu_an_owner_change_mid_turn_revokes_the_owner_menu() {
        let fixture = Fixture::new("owner-change", "chat");
        let owner = fixture.capture("chat", OWNER_KEY).unwrap();
        let settings = call("settings_get", json!({"key": OWNER_KEY, "token": ""}));
        assert!(owner.prepare(&settings).is_ok());
        fixture.edit_config(|cfg| cfg["ownerQQ"] = json!(7707));
        assert!(matches!(owner.check(), Err(CheckError::Revoked(_))));
        assert_eq!(
            refusal_text(&owner.prepare(&settings).unwrap_err()),
            "tool authority changed"
        );
        let demoted = fixture.capture("chat", OWNER_KEY).unwrap();
        assert_eq!(names(demoted.definitions()), Profile::Chat.menu());
    }

    #[test]
    fn menu_a_switched_off_tool_leaves_the_menu_for_the_next_call_and_the_next_turn() {
        let fixture = Fixture::new("switch", "closed-agent");
        let admin = fixture.capture("closed-agent", OWNER_KEY).unwrap();
        assert_eq!(names(admin.definitions()), Profile::ClosedAgent.menu());
        fixture.edit_config(|cfg| cfg["socialV2"] = json!({"tools": {"getImages": false}}));
        let images = call(
            "qq_get_message_images",
            json!({"key": OWNER_KEY, "messageId": "1"}),
        );
        assert_eq!(
            refusal_text(&admin.prepare(&images).unwrap_err()),
            "tool is no longer enabled"
        );
        let next = fixture.capture("closed-agent", OWNER_KEY).unwrap();
        let offered = names(next.definitions());
        assert!(!offered.contains(&"qq_get_message_images"));
        assert_eq!(offered.len(), Profile::ClosedAgent.menu().len() - 1);
    }

    #[test]
    fn menu_concurrent_conversations_hold_their_own_menus() {
        let fixture = Fixture::new("concurrent", "chat");
        let cfg = fixture.context.load_config().unwrap();
        let generations =
            [GROUP_KEY, OWNER_KEY].map(|conversation| (conversation, fixture.generation(conversation)));
        let captures = generations.map(|(conversation, generation)| {
            let (context, cfg) = (fixture.context.clone(), cfg.clone());
            std::thread::spawn(move || Tools::capture(&context, &cfg, "chat", conversation, &generation))
        });
        let [group, owner] = captures.map(|capture| capture.join().unwrap().unwrap());
        assert_eq!(names(group.definitions()), Profile::Chat.menu());
        assert_eq!(names(owner.definitions()), Profile::OwnerPrivate.menu());
        let settings = |key: &str| call("settings_get", json!({"key": key, "token": ""}));
        assert!(refusal_text(&group.prepare(&settings(GROUP_KEY)).unwrap_err()).contains("not found"));
        assert!(owner.prepare(&settings(OWNER_KEY)).is_ok());
    }

    /// The names the next model request would offer; panics when authority refuses the menu.
    fn offered(tools: &Tools) -> Vec<String> {
        match tools.model_tools() {
            Ok(offered) => offered.iter().map(|t| t.name.as_str().to_string()).collect(),
            Err(_) => panic!("authority refused the menu"),
        }
    }

    #[test]
    fn menu_model_tools_narrow_before_each_request_and_refuse_after_a_mode_change() {
        let fixture = Fixture::new("model-tools", "closed-agent");
        let admin = fixture.capture("closed-agent", OWNER_KEY).unwrap();
        assert_eq!(offered(&admin), Profile::ClosedAgent.menu());
        fixture.edit_config(|cfg| cfg["socialV2"] = json!({"tools": {"getImages": false}}));
        let narrowed = offered(&admin);
        assert!(
            !narrowed.iter().any(|name| name == "qq_get_message_images"),
            "a switch turned off mid-turn stayed on the menu: {narrowed:?}"
        );
        assert_eq!(narrowed.len(), Profile::ClosedAgent.menu().len() - 1);
        assert_eq!(
            names(admin.definitions()),
            Profile::ClosedAgent.menu(),
            "the admission record keeps the captured menu"
        );
        fixture.edit_config(|cfg| cfg["socialV2"] = json!({"tools": {"getImages": true}}));
        assert_eq!(
            offered(&admin),
            Profile::ClosedAgent.menu(),
            "a switch turned back on restores a captured tool"
        );
        fixture.set_mode("chat");
        assert!(matches!(admin.model_tools(), Err(CheckError::Revoked(_))));
    }

    #[test]
    fn menu_direct_send_switches_narrow_every_menu_like_the_console() {
        let fixture = Fixture::new("send-switches", "closed-agent");
        fixture.edit_config(|cfg| cfg["socialV2"] = json!({"tools": {"sendPrivate": false}}));
        let admin = fixture.capture("closed-agent", OWNER_KEY).unwrap();
        assert!(!names(admin.definitions()).contains(&"qq_send_private_message"));
        let send = call(
            "qq_send_private_message",
            json!({"userId": 8808, "message": "hello"}),
        );
        assert!(refusal_text(&admin.prepare(&send).unwrap_err()).contains("not found"));
        for (flag, tool) in [
            ("sendPrivate", "qq_send_private_message"),
            ("sendGroup", "qq_send_group_message"),
            ("reply", "qq_reply"),
        ] {
            let listed = |value: Value| {
                let cfg = json!({"socialV2": {"tools": {flag: value}}});
                names(&definitions(&cfg, Profile::Reserved2)).contains(&tool)
            };
            assert!(!listed(json!(false)), "{flag} false");
            assert!(listed(json!(true)), "{flag} true");
            assert!(
                listed(json!("false")),
                "only an explicit false switches {flag} off, as the console reads it"
            );
        }
    }

    #[test]
    fn mode_prompt_comes_from_the_store_per_profile_and_opens_the_system_message() {
        let fixture = Fixture::new("mode-prompt", "chat");
        let instruction = |mode: &str, conversation: &str| {
            fixture.capture(mode, conversation).unwrap().instruction()
        };
        let save = |key: &str, text: &str| {
            crate::settings::apply(&fixture.context, key, &json!(text), "administrator", "console")
        };
        assert_eq!(
            instruction("chat", GROUP_KEY),
            session_instruction(GROUP_KEY),
            "no saved mode prompt: the session line alone, as before"
        );
        save("prompt.mode.chat", "群聊模式说明 140").unwrap();
        save("prompt.mode.ownerPrivate", "这是你和管理员的私聊。140").unwrap();
        save("prompt.mode.closedAgent", "封闭模式说明 140").unwrap();
        assert_eq!(
            instruction("chat", GROUP_KEY),
            format!("群聊模式说明 140\n\n{}", session_instruction(GROUP_KEY))
        );
        assert_eq!(
            instruction("chat", OWNER_KEY),
            format!("这是你和管理员的私聊。140\n\n{}", session_instruction(OWNER_KEY))
        );
        fixture.set_mode("closed-agent");
        assert_eq!(
            instruction("closed-agent", OWNER_KEY),
            format!("封闭模式说明 140\n\n{}", session_instruction(OWNER_KEY))
        );
        // Guarded at save: a denied term is refused and the saved prompt stays in effect.
        assert_eq!(
            save("prompt.mode.closedAgent", "你是一个助手").err(),
            Some((400, "prompt.mode.closedAgent names a denied term: 助手".to_string()))
        );
        assert_eq!(
            save("prompt.mode.closedAgent", &"鲸".repeat(8001)).err(),
            Some((
                400,
                "prompt.mode.closedAgent must be text of at most 8000 characters, with no control character but line feed and tab"
                    .to_string()
            ))
        );
        assert!(instruction("closed-agent", OWNER_KEY).starts_with("封闭模式说明 140\n\n"));
        // The longest accepted prompt is sent whole.
        save("prompt.mode.closedAgent", &"鲸".repeat(8000)).unwrap();
        assert_eq!(
            instruction("closed-agent", OWNER_KEY),
            format!("{}\n\n{}", "鲸".repeat(8000), session_instruction(OWNER_KEY))
        );
        // The model-view strip applies to the mode prompt.
        save("prompt.mode.closedAgent", "封闭模式说明 140").unwrap();
        fixture.edit_config(|cfg| {
            cfg["socialV2"] = json!({"context": {"stripFromModelText": ["封闭"]}})
        });
        assert_eq!(
            instruction("closed-agent", OWNER_KEY),
            format!("模式说明 140\n\n{}", session_instruction(OWNER_KEY))
        );
        // An emptied prompt sends the session line alone again.
        save("prompt.mode.closedAgent", "").unwrap();
        assert_eq!(instruction("closed-agent", OWNER_KEY), session_instruction(OWNER_KEY));
    }
}
