use super::*;

#[test]
fn settings_batch_gate_warns_once_per_excessive_voice_duration() {
    const WORKER: &str = "HER_VOICE_WARNING_TEST_WORKER";
    if std::env::var_os(WORKER).is_some() {
        let (root, context) = settings_fixture("voice warning");
        for seconds in [300, 300, 301, 60, 300] {
            write_config(&root, |cfg| cfg["socialV2"]["voice"]["maxSeconds"] = json!(seconds));
            assert_eq!(context.load_config().unwrap()["socialV2"]["voice"]["maxSeconds"], json!(60));
        }
        return;
    }

    let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "console::tests::settings_tests::live_controls::settings_batch_gate_warns_once_per_excessive_voice_duration",
            "--nocapture",
        ])
        .env(WORKER, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    let warnings: Vec<_> = stdout.lines()
        .filter(|line| line.starts_with("[voice] socialV2.voice.maxSeconds="))
        .collect();
    assert_eq!(warnings, [
        "[voice] socialV2.voice.maxSeconds=300 exceeds QQ limit; clamped to 60s",
        "[voice] socialV2.voice.maxSeconds=301 exceeds QQ limit; clamped to 60s",
    ]);
}

#[test]
fn settings_batch_gate_reports_the_minimum_voice_duration() {
    let (_root, context) = settings_fixture("voice duration minimum");
    let setting = "socialV2.voice.maxSeconds";
    let minimum = setting_view(&context, setting)["kind"]["min"].as_f64().unwrap();
    assert_eq!(minimum, f64::from_bits(1), "the duration minimum must be the smallest positive value");
    for (value, expected) in [(0.0, 400), (minimum, 200)] {
        let (status, reply) = post(&context, "/api/settings/set", json!({"setting":setting,"value":value}));
        assert_eq!(status, expected, "{value}: {reply}");
    }
    assert_eq!(setting_view(&context, "socialV2.voice.maxPerMinute")["kind"]["min"], 0);
}

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

#[test]
fn settings_batch_alert_uses_live_limits_without_clearing_spent_budget() {
    use crate::admin_alert::{AlertLimiter, AlertVerdict};
    let (_root, context) = settings_fixture("settings live alert limits");
    let fallback = context.load_config().unwrap();
    let mut limiter = AlertLimiter::new();
    assert_eq!(limiter.check_current(&context, &fallback, "first", 1000), AlertVerdict::Send);
    for (setting, value) in [("alerts.keyWindowSecs", 2), ("alerts.hourlyCap", 1)] {
        assert_eq!(post(&context, "/api/settings/set", json!({"setting":setting,"value":value})).0, 200);
    }
    assert_eq!(limiter.check_current(&context, &fallback, "first", 2999), AlertVerdict::SkipKeyWindow);
    assert_eq!(limiter.check_current(&context, &fallback, "first", 3000), AlertVerdict::SkipHourlyCap, "the live window must end at exactly two seconds");
    assert_eq!(limiter.check_current(&context, &fallback, "second", 3000), AlertVerdict::SkipHourlyCap, "lowering the cap keeps the existing charge");
    assert_eq!(limiter.check_current(&context, &fallback, "second", 3_600_999), AlertVerdict::SkipHourlyCap);
    assert_eq!(limiter.check_current(&context, &fallback, "second", 3_601_000), AlertVerdict::Send);
}

#[test]
fn settings_batch_gateway_silence_uses_the_live_threshold() {
    let (root, context) = settings_fixture("settings live silence threshold");
    write_config(&root, |cfg| cfg["snowluma"]["silenceWarnMs"] = json!(5_000));
    let mut health = crate::gateway_health::GatewayHealth::new();
    health.note_packet("message", 1000);
    health.configure(&context.load_config().unwrap());
    assert_eq!(health.snapshot(61_000)["silent"], false);
    assert_eq!(health.snapshot(61_001)["silent"], true, "legacy values clamp to the safety minimum");
    let setting = "snowluma.silenceWarnMs";
    assert_eq!(post(&context, "/api/settings/set", json!({"setting":setting,"value":59_999})).0, 400);
    assert_eq!(post(&context, "/api/settings/set", json!({"setting":setting,"value":120_000})).0, 200);
    health.configure(&context.load_config().unwrap());
    assert_eq!(health.snapshot(61_001)["silent"], false, "the next poll must use the new threshold");
    assert_eq!(health.snapshot(121_000)["silent"], false);
    assert_eq!(health.snapshot(121_001)["silent"], true);
}
