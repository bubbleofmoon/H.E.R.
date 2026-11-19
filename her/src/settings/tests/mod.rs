// The settings store (`settings.rs`, `settings/`) and its console routes (`console_settings.rs`), through the console
// API on a fixture runtime and through the configuration loader, as the next use reads a setting. Included in
// `console::tests` as `settings_tests`; the topic modules below include their own files, and this file holds the
// fixtures they share. Every raw read or write of `data/settings.db` goes through `crate::settings::StoreProbe`.
use super::*;
use crate::settings::{AuditRow, StoreProbe};

mod store { include!("store.rs"); }
mod registry { include!("registry.rs"); }
mod history { include!("history.rs"); }
mod models { include!("models.rs"); }
mod chat { include!("chat.rs"); }
mod prompt_text { include!("prompt_text.rs"); }
mod mirror { include!("mirror.rs"); }
mod console_writers { include!("console_writers.rs"); }
mod security { include!("security.rs"); }
mod conversation { include!("conversation.rs"); }
mod live_controls { include!("live_controls.rs"); }

const QUIET_KEY: &str = "reply.wakeQuietMs";

/// The settings module's product sources, for the source tests: the module root, the store, the kinds and the registry.
const SETTINGS_SOURCES: &[(&str, &str)] = &[
    ("settings.rs", include_str!("../../settings.rs")),
    ("settings/store.rs", include_str!("../store.rs")),
    ("settings/store/conversation.rs", include_str!("../store/conversation.rs")),
    ("settings/kinds.rs", include_str!("../kinds.rs")),
    ("settings/kinds/choice.rs", include_str!("../kinds/choice.rs")),
    ("settings/kinds/id_list.rs", include_str!("../kinds/id_list.rs")),
    ("settings/kinds/integer.rs", include_str!("../kinds/integer.rs")),
    ("settings/kinds/model_id.rs", include_str!("../kinds/model_id.rs")),
    ("settings/kinds/owner_id.rs", include_str!("../kinds/owner_id.rs")),
    ("settings/kinds/route_id.rs", include_str!("../kinds/route_id.rs")),
    ("settings/kinds/switch.rs", include_str!("../kinds/switch.rs")),
    ("settings/kinds/text.rs", include_str!("../kinds/text.rs")),
    ("settings/kinds/voice_number.rs", include_str!("../kinds/voice_number.rs")),
    ("settings/registry.rs", include_str!("../registry.rs")),
    ("settings/registry/access.rs", include_str!("../registry/access.rs")),
    ("settings/registry/engine.rs", include_str!("../registry/engine.rs")),
    ("settings/registry/model.rs", include_str!("../registry/model.rs")),
    ("settings/registry/prompt.rs", include_str!("../registry/prompt.rs")),
    ("settings/registry/reply.rs", include_str!("../registry/reply.rs")),
    ("settings/registry/tools.rs", include_str!("../registry/tools.rs")),
    ("settings/registry/voice.rs", include_str!("../registry/voice.rs")),
];

fn settings_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    fixture_runtime(label, "fixture-settings-token-abcdefghij")
}

fn settings_db(root: &FixtureRoot) -> std::path::PathBuf {
    StoreProbe::at(&root.0).path().to_path_buf()
}

fn set_quiet(context: &RuntimeContext, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": QUIET_KEY, "value": value})))
}

/// The `reply.wakeQuietMs` entry of `GET /api/settings`.
fn quiet_view(context: &RuntimeContext) -> Value {
    let (status, body) = admin_request(context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == QUIET_KEY).unwrap().clone()
}

/// What the next wake reads, through the configuration loader.
fn next_use_quiet(context: &RuntimeContext) -> Option<i64> {
    crate::wake_cfg_of(&context.load_config().unwrap()).reply_wake_quiet_ms
}

/// The audit rows, oldest first.
fn audit_rows(root: &FixtureRoot) -> Vec<Value> {
    StoreProbe::at(&root.0).audit_rows()
}

fn write_config(root: &FixtureRoot, edit: impl FnOnce(&mut Value)) {
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut config);
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

fn pre_settings_copy(root: &FixtureRoot) -> std::path::PathBuf {
    root.0.join("config/config.json.pre-settings")
}

/// `config.json` as stored, without the settings overlay: what a release without the store reads.
fn stored_config(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap()
}

/// The quiet time a release without the store reads: `config.json` alone, through the same reader.
fn file_quiet(root: &FixtureRoot) -> Option<i64> {
    crate::wake_cfg_of(&stored_config(root)).reply_wake_quiet_ms
}

fn post(context: &RuntimeContext, path: &str, body: Value) -> (u16, Value) {
    admin_request(context, "POST", path, Some(&body))
}

/// One page of `GET /api/settings/history<query>`: its entries, newest first, and its `next`.
fn history_page(context: &RuntimeContext, query: &str) -> (Vec<Value>, Value) {
    let (status, body) = admin_request(context, "GET", &format!("/api/settings/history{query}"), None);
    assert_eq!(status, 200, "{body}");
    (body["entries"].as_array().unwrap().clone(), body["next"].clone())
}

/// The `key` entry of `GET /api/settings`.
fn setting_view(context: &RuntimeContext, key: &str) -> Value {
    let (status, body) = admin_request(context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    body["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("{key} is listed"))
        .clone()
}

/// The chat's name for `reply.wakeQuietMs`: its configuration path.
const CHAT_NAME: &str = "socialV2.reply.wakeQuietMs";
/// The fixture's owner-private conversation (`ownerQQ` 7001) and its agent token.
const OWNER_CHAT: &str = "private:7001";
const OWNER_CHAT_TOKEN: &str = "fixture-owner-chat-token-149";
const GROUP_CHAT_TOKEN: &str = "fixture-group-chat-token-149";

/// A settings fixture whose owner-private chat and one group chat hold agent tokens.
fn chat_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    let state = json!({"conversations": {
        OWNER_CHAT: {"agentToken": OWNER_CHAT_TOKEN},
        "group:101": {"agentToken": GROUP_CHAT_TOKEN},
    }});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    (root, context)
}

/// The owner-private chat's arguments with `fields` added.
fn owner_chat_args(fields: Value) -> Value {
    let mut args = json!({"key": OWNER_CHAT, "token": OWNER_CHAT_TOKEN});
    args.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    args
}

/// One settings action from the owner's private chat, as the chat tools run it (`memory_actions::tool_action`).
fn chat(context: &RuntimeContext, action: &str, fields: Value) -> Result<Value, String> {
    memory_tool_action(context, action, &owner_chat_args(fields))
}

/// The settings module's product sources joined, for a source test that reads them as one text.
fn settings_source() -> String {
    SETTINGS_SOURCES.iter().map(|(_, source)| *source).collect::<Vec<_>>().join("\n")
}
