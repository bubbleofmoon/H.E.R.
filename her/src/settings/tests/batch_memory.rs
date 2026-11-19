use super::*;

#[test]
fn recall_setting_survives_a_failed_mirror_and_the_next_write_repairs_it() {
    let (root, context) = chat_fixture("memory settings mirror");
    write_config(&root, |cfg| cfg["memory"] = json!({"inject": {"relatedTopK": " 7 "}}));
    let previous = context.load_config().unwrap();
    assert_eq!(previous.pointer("/memory/inject/relatedTopK"), Some(&json!(7)));
    assert_eq!(crate::briefing::Budgets::from_cfg(&previous).material_items, 5);
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let args = json!({"settingKey": "memory.inject.relatedTopK", "value": 14});
    let reply = chat(&context, "settings-set", args.clone()).unwrap();
    assert_eq!(reply["mirrorError"], SETTINGS_MIRROR_UNFINISHED);
    assert_eq!(stored_config(&root)["memory"]["inject"]["relatedTopK"], " 7 ");
    let next = context.load_config().unwrap();
    assert_eq!(crate::prompt::recall_top_k(&next), 14, "next wake must read the committed recall depth");
    assert_eq!(crate::briefing::Budgets::from_cfg(&next).material_items, 14);
    assert_eq!(chat(&context, "settings-set", args).unwrap()["changed"], false);
    assert_eq!(stored_config(&root)["memory"]["inject"]["relatedTopK"], 14);
}

#[test]
fn effective_memory_snapshot_resolves_legacy_ranges_and_missing_members() {
    let (root, context) = settings_fixture("memory settings ranges");
    write_config(&root, |cfg| cfg["memory"] = json!({
        "autoFlush":{"enabled":null,"contextPct":"101","maxSessionsPerDay":"2.8","minIntervalMinutes":-1},
        "digest":{"maxInputChars":999}, "layers":{"identityTokens":0,"materialItems":21}
    }));
    let cfg = context.load_config().unwrap();
    let memory = &cfg["memory"];
    assert_eq!(memory["digest"]["maxInputChars"], 1000, "normalized snapshot must clamp input size");
    assert_eq!(memory["autoFlush"], json!({"enabled":true,"contextPct":100,"maxSessionsPerDay":2,"minIntervalMinutes":0}));
    assert_eq!(memory["layers"], json!({
        "identityTokens":1500,"relationshipTokens":1500,"conversationTokens":800,"materialTokens":1000,"materialItems":5
    }));
    assert_eq!(stored_config(&root)["memory"]["digest"]["maxInputChars"], 999);
    assert!(!settings_db(&root).exists());
}
