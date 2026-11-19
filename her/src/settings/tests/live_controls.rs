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
