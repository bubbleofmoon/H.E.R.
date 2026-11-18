// The owner's chat settings tools (`settings_get`, `settings_set`) on registry settings (#149).
use super::*;

const CHAT_WRITE_REFUSED: &str = "该项不可通过对话修改（只有 settings_get 列出的设置可以改；白名单/令牌/路径类必须本地改）";
const CHAT_STORE_UNAVAILABLE: &str = "设置存储暂时不可用，这次没有改动";
const CHAT_OWNER_ONLY: &str = "配置读写只允许管理员私聊";

/// What the model reads back from one chat settings tool call, as the MCP server renders it.
fn chat_text(context: &RuntimeContext, tool: &str, fields: Value) -> String {
    let outcome = crate::mcp::call_prepared(tool, &owner_chat_args(fields), context);
    outcome["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn chat_settings_set_validates_and_audits_through_the_registry() {
    let (root, context) = chat_fixture("chat settings set");
    let config_path = root.0.join("config/config.json");
    let stored_file = std::fs::read(&config_path).unwrap();
    let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 4500}));
    assert_eq!(text, format!("已改 {CHAT_NAME} = 4500。下次唤醒时生效"));
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the chat's value");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the console shows the chat's value");
    assert_eq!(file_quiet(&root), Some(4500), "a registry setting is written to the store and back into config.json");
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), stored_file, "config.json from before the first write is kept");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["actor"].clone(), rows[0]["source"].clone()), (json!("owner"), json!("chat")));
    assert_eq!((rows[0]["scope"].clone(), rows[0]["key"].clone()), (json!("global"), json!(QUIET_KEY)));
    assert_eq!((rows[0]["old"].clone(), rows[0]["new"].clone()), (Value::Null, json!("4500")));

    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap();
    assert_eq!((reply["value"].clone(), reply["changed"].clone(), reply["hot"].clone()), (json!(4500), json!(false), json!(true)));
    assert_eq!(audit_rows(&root).len(), 1, "the same value again is not a change");
    let padded = format!(" {CHAT_NAME} ");
    let reply = chat(&context, "settings-set", json!({"settingKey": padded, "value": 1})).unwrap();
    assert_eq!((reply["key"].clone(), reply["value"].clone(), reply["changed"].clone()), (json!(CHAT_NAME), json!(1), json!(true)));
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 30000})).unwrap();
    assert_eq!(reply["value"], 30000);
    assert!(reply.get("mirrorError").is_none(), "a write-back that succeeded adds nothing: {reply}");

    // An invalid value is refused under the chat's name with the registry's bounds, and changes nothing.
    let requirement = format!("{CHAT_NAME} 必须是 1 到 30000 的整数");
    for value in [
        json!(0),
        json!(30001),
        json!(-1),
        json!(4500.5),
        json!(3000.0),
        json!("4500"),
        json!(true),
        Value::Null,
        json!([4500]),
        json!({"ms": 4500}),
        json!(1e30),
        json!(u64::MAX),
        json!("x".repeat(100_000)),
    ] {
        let refused = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": value.clone()}));
        assert_eq!(refused, Err(requirement.clone()), "{}", &value.to_string()[..value.to_string().len().min(40)]);
    }
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME})), Err(requirement.clone()), "no value");
    let refusal = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 0}));
    assert_eq!(refusal, format!("改配置失败：{requirement}"));
    for model_text in [&text, &refusal] {
        assert!(crate::model_text_policy::denied_terms(model_text).is_empty(), "{model_text}");
    }
    // Only the configuration path names it: not the registry key, the pointer, another case, a parent or a child.
    for name in [QUIET_KEY, "/socialV2/reply/wakeQuietMs", "SOCIALV2.REPLY.WAKEQUIETMS", "socialV2.reply", "socialV2", "socialV2.reply.wakeQuietMs.x"] {
        let refused = chat(&context, "settings-set", json!({"settingKey": name, "value": 4500}));
        assert_eq!(refused, Err(CHAT_WRITE_REFUSED.to_string()), "{name}");
    }
    // Only the owner's private chat writes: not a group chat's token, nor that token presented for the owner's chat.
    for (key, token) in [("group:101", GROUP_CHAT_TOKEN), (OWNER_CHAT, GROUP_CHAT_TOKEN), (OWNER_CHAT, "")] {
        let args = json!({"key": key, "token": token, "settingKey": CHAT_NAME, "value": 4500});
        assert_eq!(memory_tool_action(&context, "settings-set", &args), Err(CHAT_OWNER_ONLY.to_string()), "{key}");
    }
    assert_eq!(audit_rows(&root).len(), 3, "a refusal writes no audit row");
    assert_eq!(next_use_quiet(&context), Some(30000), "every refusal keeps the old value");
    assert_eq!(file_quiet(&root), Some(30000), "config.json holds the last stored value");
}

#[test]
fn chat_settings_get_reads_registry_settings_as_the_next_use_and_hides_everything_else() {
    let (root, context) = chat_fixture("chat settings get");
    // No stored and no file value: the registry default, in the list and alone.
    let all = chat(&context, "settings-get", json!({})).unwrap();
    assert_eq!(all["settings"][CHAT_NAME], 3000, "{all}");
    assert!(all["writable"].as_array().unwrap().contains(&json!(CHAT_NAME)), "{all}");
    let one = chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap();
    assert_eq!(one, json!({"ok": true, "key": CHAT_NAME, "writable": true, "value": 3000}));
    assert_eq!(chat_text(&context, "settings_get", json!({"settingKey": CHAT_NAME})), format!("{CHAT_NAME} = 3000"));
    let listed = chat_text(&context, "settings_get", json!({}));
    assert!(listed.lines().any(|line| line == format!("{CHAT_NAME} = 3000")), "{listed}");

    // The file's value, a stored value over it, then a stored value the registry refuses: always what the next use reads.
    let read = |context: &RuntimeContext| chat(context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap()["value"].clone();
    let listed = |context: &RuntimeContext| chat(context, "settings-get", json!({})).unwrap()["settings"][CHAT_NAME].clone();
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000, "apiKey": "fixture-reply-value-149"}}));
    assert_eq!((read(&context), listed(&context)), (json!(9000), json!(9000)));
    assert_eq!(next_use_quiet(&context), Some(9000));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!((read(&context), listed(&context)), (json!(4500), json!(4500)), "a stored value wins over the file's");
    assert_eq!(next_use_quiet(&context), Some(4500));
    StoreProbe::at(&root.0).set_value("reply.wakeQuietMs", "50000");
    assert_eq!(
        (read(&context), listed(&context)),
        (json!(4500), json!(4500)),
        "a stored value the registry refuses is not applied; the file's written-back value is read"
    );
    assert_eq!(next_use_quiet(&context), Some(4500));
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 90000, "apiKey": "fixture-reply-value-149"}}));
    assert_eq!((read(&context), listed(&context)), (json!(3000), json!(3000)), "an out-of-bounds file value reads as the default");
    assert_eq!(next_use_quiet(&context), Some(3000));

    // Nothing else reads: not the registry key, the pointer, a parent, a sibling, a secret beside it or a long name.
    let long = "x".repeat(10_000);
    for name in [
        QUIET_KEY,
        "/socialV2/reply/wakeQuietMs",
        "SOCIALV2.REPLY.WAKEQUIETMS",
        "socialV2.reply",
        "socialV2",
        "socialV2.reply.apiKey",
        "socialV2.reply.quietMinMs",
        long.as_str(),
    ] {
        let refused = chat(&context, "settings-get", json!({"settingKey": name}));
        assert_eq!(refused, Err(SETTINGS_READ_REFUSED.to_string()), "{}", &name[..name.len().min(40)]);
    }
    let listed = chat_text(&context, "settings_get", json!({}));
    assert!(!listed.contains("fixture-") && !listed.contains("apiKey"), "{listed}");
    let all = chat(&context, "settings-get", json!({})).unwrap();
    assert!(!all.to_string().contains("fixture-"), "{all}");
    for name in all["writable"].as_array().unwrap() {
        assert!(!crate::console::setting_is_secret(name.as_str().unwrap()), "{name}");
    }

    // A legacy (no-root) context has no settings store: no registry setting is offered, read or written there.
    let legacy = RuntimeContext::legacy();
    let cfg = json!({"socialV2": {"reply": {"wakeQuietMs": 9000}}});
    let all = crate::console::settings_action("settings-get", &json!({}), &cfg, &legacy).unwrap();
    assert!(all["settings"].get(CHAT_NAME).is_none(), "{all}");
    assert!(!all["writable"].as_array().unwrap().contains(&json!(CHAT_NAME)), "{all}");
    let one = crate::console::settings_action("settings-get", &json!({"settingKey": CHAT_NAME}), &cfg, &legacy);
    assert_eq!(one, Err((403, SETTINGS_READ_REFUSED.to_string())));
    let set = crate::console::settings_action("settings-set", &json!({"settingKey": CHAT_NAME, "value": 4500}), &cfg, &legacy);
    assert_eq!(set, Err((403, CHAT_WRITE_REFUSED.to_string())));
}

#[test]
fn chat_settings_answer_store_trouble_without_its_details() {
    // A store this release cannot use: not a database, a newer schema, a leftover journal.
    for trouble in ["garbage", "newer", "journal"] {
        let (root, context) = chat_fixture(&format!("chat settings trouble {trouble}"));
        match trouble {
            "garbage" => std::fs::write(settings_db(&root), vec![b'x'; 4096]).unwrap(),
            "newer" => StoreProbe::at(&root.0).build("CREATE TABLE later (x INTEGER); PRAGMA user_version=2;"),
            _ => {
                StoreProbe::at(&root.0).build("CREATE TABLE scratch (x INTEGER); DROP TABLE scratch;");
                std::fs::write(root.0.join("data/settings.db-journal"), vec![0xff_u8; 512]).unwrap();
            }
        }
        let before = std::fs::read(settings_db(&root)).unwrap();
        let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 4500}));
        assert_eq!(text, format!("改配置失败：{CHAT_STORE_UNAVAILABLE}"), "{trouble}");
        assert!(crate::model_text_policy::denied_terms(&text).is_empty(), "{trouble}: {text}");
        assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "{trouble}: the store keeps every byte");
        let read = chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap();
        assert_eq!(read["value"], 3000, "{trouble}: a read shows what the next use reads");
        assert_eq!(next_use_quiet(&context), Some(3000), "{trouble}");
    }
    // A change cut off before its audit row (a crash midway) leaves no value and no row; the next change continues.
    let (root, context) = chat_fixture("chat settings cut off");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap()["value"], 4500);
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let cut = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000}));
    assert_eq!(cut, Err(CHAT_STORE_UNAVAILABLE.to_string()));
    assert_eq!(next_use_quiet(&context), Some(4500), "the cut-off change left no value");
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000})).unwrap()["value"], 5000);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[1]["old"].clone(), rows[1]["new"].clone()), (json!("4500"), json!("5000")));
}

#[test]
fn chat_settings_and_the_console_write_one_setting_at_once() {
    let (root, context) = chat_fixture("chat settings concurrent");
    let (from_chat, from_console) = std::thread::scope(|scope| {
        let from_chat = scope.spawn(|| chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4000})));
        let from_console =
            scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(5000), "administrator", "console"));
        (from_chat.join().unwrap(), from_console.join().unwrap())
    });
    assert_eq!(from_chat.unwrap()["changed"], true);
    assert!(from_console.unwrap().changed);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let mut sources: Vec<String> = rows.iter().map(|row| row["source"].as_str().unwrap().to_string()).collect();
    sources.sort();
    assert_eq!(sources, ["chat", "console"]);
    assert_eq!(rows[1]["old"], rows[0]["new"], "the later writer saw the earlier commit");
    let last: i64 = rows[1]["new"].as_str().unwrap().parse().unwrap();
    assert_eq!(next_use_quiet(&context), Some(last));
    assert_eq!(chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap()["value"], last);
}

#[test]
fn chat_settings_legacy_set_replies_without_the_value() {
    let (_root, context) = chat_fixture("chat settings legacy reply");
    // A config.json setting the owner writes: the model's reply names the key, never the value the owner sent.
    let text = chat_text(&context, "settings_set", json!({"settingKey": "sendDelayMs", "value": "OpenAI"}));
    assert_eq!(text, "已改 sendDelayMs。已立即生效", "{text}");
    assert!(!text.contains("OpenAI"), "{text}");
    assert!(crate::model_text_policy::denied_terms(&text).is_empty(), "{text}");
}

#[test]
fn chat_settings_come_from_the_registry_and_are_never_secret() {
    let (_root, context) = chat_fixture("chat settings registry");
    let all = chat(&context, "settings-get", json!({})).unwrap();
    let offered: Vec<String> =
        all["writable"].as_array().unwrap().iter().map(|name| name.as_str().unwrap().to_string()).collect();
    let registry: Vec<_> = crate::settings::registry().collect();
    assert!(registry.iter().any(|setting| setting.chat), "the registry has a chat setting");
    for setting in registry.iter().filter(|setting| setting.chat) {
        let name = setting.chat_name();
        assert!(offered.contains(&name), "{name} is marked chat but settings_get does not offer it: {offered:?}");
        assert_eq!(crate::settings::chat_setting(&name).map(|found| found.key), Some(setting.key), "{name}");
        assert!(
            setting.pointer.split('/').skip(1).all(|part| !part.contains('.')),
            "{}: a dot in a member name makes the chat name ambiguous",
            setting.pointer
        );
        assert!(!crate::console::setting_is_secret(&name) && !crate::console::setting_is_secret(setting.key), "{name} is a secret");
        let lower = name.to_lowercase();
        for refused in ["allow", "token", "secret", "password", "path", "ownerqq", "consoletoken"] {
            assert!(!lower.contains(refused), "{name} names {refused}");
        }
        assert!(!name.starts_with("dsh.") && !name.starts_with("snowluma."), "{name}");
    }
    for setting in registry.iter().filter(|setting| !setting.chat) {
        let name = setting.chat_name();
        assert!(!offered.contains(&name) && crate::settings::chat_setting(&name).is_none(), "{} is not for the chat", setting.key);
    }
    // A path in both lists would be written to config.json under the store's value, and lost.
    for path in SETTINGS_WRITABLE {
        let pointer = format!("/{}", path.replace('.', "/"));
        assert!(
            registry.iter().all(|setting| setting.pointer != pointer),
            "{path} is registered: mark its registry setting chat and delete it from SETTINGS_WRITABLE"
        );
    }
}

#[test]
fn chat_settings_set_reply_carries_a_failed_write_back_as_a_fixed_text() {
    let (root, context) = chat_fixture("chat settings mirror error");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap()["value"], 4500);
    // The write-back's own error names files, so the chat gets one fixed text; the change stands.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000})).unwrap();
    assert_eq!((reply["value"].clone(), reply["changed"].clone()), (json!(5000), json!(true)));
    assert_eq!(reply["mirrorError"], SETTINGS_MIRROR_UNFINISHED, "{reply}");
    assert_eq!(reply["note"], format!("下次唤醒时生效；{SETTINGS_MIRROR_UNFINISHED}"), "{reply}");
    assert_eq!(next_use_quiet(&context), Some(5000));
    assert_eq!(file_quiet(&root), Some(4500), "config.json still holds the earlier value");
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 6000}));
    assert_eq!(text, format!("已改 {CHAT_NAME} = 6000。下次唤醒时生效；{SETTINGS_MIRROR_UNFINISHED}"));
    assert!(!text.contains("config.json") && !text.contains("runtime config"), "{text}");
    assert!(crate::model_text_policy::denied_terms(&text).is_empty(), "{text}");
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 6000})).unwrap();
    assert_eq!(reply["changed"], false);
    assert!(reply.get("mirrorError").is_none(), "{reply}");
    assert_eq!(file_quiet(&root), Some(6000), "the next settings change finished the write-back");
}
