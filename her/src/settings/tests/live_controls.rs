use super::*;

#[test]
fn settings_batch_tools_seed_and_mirror_without_changing_the_effective_switch() {
    let (root, context) = settings_fixture("settings tool upgrade");
    write_config(&root, |cfg| cfg["socialV2"] = json!({"tools":{"sendVoice":null}}));
    assert!(tool_enabled(&context.load_config().unwrap(), "sendVoice"));
    let (status, answer) = post(&context, "/api/settings/set", json!({"setting":"socialV2.tools.getImages","value":false}));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(stored_config(&root)["socialV2"]["tools"]["sendVoice"], true, "the legacy null must seed as an enabled switch");
    let rows = audit_rows(&root);
    assert!(rows.iter().any(|row| row["key"] == "socialV2.tools.sendVoice" && row["source"] == "migration"), "{rows:?}");
    let (status, reply) = post(&context, "/api/socialV2/config", json!({"tools":{"sendVoice":false}}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(stored_config(&root)["socialV2"]["tools"]["sendVoice"], false);
    assert_eq!(reply["config"]["tools"]["getImages"], false, "an omitted stored switch keeps its value");
    assert!(!tool_enabled(&context.load_config().unwrap(), "sendVoice"));
}

#[test]
fn settings_batch_media_uses_the_live_limits_at_both_edges() {
    let (root, context) = settings_fixture("settings media");
    write_config(&root, |cfg| cfg["socialV2"]["media"] = json!({"maxBytes":0,"maxPixels":99_999_999,"maxCount":"2.4"}));
    let loaded = context.load_config().unwrap();
    assert_eq!(loaded["socialV2"]["media"], json!({"maxBytes":8_388_608,"maxPixels":64_000_000,"maxCount":2}));
    for (key, min, max) in [
        ("maxBytes", 262_144, 20_971_520),
        ("maxPixels", 1_000_000, 64_000_000),
        ("maxCount", 1, 8),
    ] {
        for (value, status, effective) in [(min, 200, min), (min - 1, 400, min), (max, 200, max), (max + 1, 400, max)] {
            let (actual, answer) = post(&context, "/api/settings/set", json!({
                "setting": format!("socialV2.media.{key}"), "value": value
            }));
            assert_eq!(actual, status, "{key}={value}: {answer}");
            let limits = crate::media::Limits::from_config(&context.load_config().unwrap());
            let read = match key {
                "maxBytes" => limits.bytes as i64,
                "maxPixels" => limits.pixels as i64,
                "maxCount" => limits.count as i64,
                _ => unreachable!(),
            };
            assert_eq!(read, effective, "media reader for {key}={value}");
        }
    }
}

#[test]
fn settings_batch_sticker_chat_spelling_writes_the_live_sticker_switch() {
    let (root, context) = chat_fixture("settings sticker chat switch");
    write_config(&root, |cfg| cfg["socialV2"]["sticker"]["stickerEnabled"] = json!(false));
    assert!(crate::sticker::enabled(&context.load_config().unwrap()), "old unused file member has no effect");
    let name = "socialV2.sticker.stickerEnabled";
    let reply = chat(&context, "settings-set", json!({"settingKey":name,"value":false})).expect("legacy sticker chat path");
    assert_eq!(reply["value"], false);
    assert!(!crate::sticker::enabled(&context.load_config().unwrap()), "the chat change disables stickers");
    assert_eq!(stored_config(&root)["socialV2"]["sticker"]["enabled"], false, "rollback reads the canonical file member");
    let key = "socialV2.sticker.collect.maxBytes";
    let answer = chat(&context, "settings-set", json!({"settingKey":key,"value":1024})).unwrap();
    assert_eq!(answer["value"], 1024);
    assert_eq!(setting_view(&context, key)["stored"], 1024);
    assert!(chat(&context, "settings-set", json!({"settingKey":key,"value":20_971_521})).is_err());
    assert_eq!(setting_view(&context, key)["stored"], 1024, "invalid chat edit leaves the cap in place");
}
