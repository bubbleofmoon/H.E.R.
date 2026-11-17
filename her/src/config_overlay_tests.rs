// The configuration overlay seam: every reader of configuration values sees what the loader's overlay applies over
// the stored `config.json`; writers and the backup archive keep the stored file.
use super::*;

const OVERLAY_MODEL: &str = "overlay-model";

/// A selected runtime whose stored config has `dsh.model` "fixture-model", `ownerQQ` 7001 and
/// `role.maxInjectChars` 7000.
fn overlay_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = fixture_runtime(label, "fixture-config-overlay-token-abcdefghij");
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["role"] = json!({"maxInjectChars": 7000});
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    (root, context)
}

fn with_overlay_model<T>(run: impl FnOnce() -> T) -> T {
    crate::runtime::with_config_overlay("/dsh/model", json!(OVERLAY_MODEL), run)
}

#[test]
fn config_overlay_reaches_the_loader_and_never_the_stored_file() {
    let (root, context) = overlay_fixture("config overlay loader");
    let path = root.0.join("config/config.json");
    let stored = std::fs::read(&path).unwrap();
    with_overlay_model(|| {
        assert_eq!(context.load_config().unwrap()["dsh"]["model"], OVERLAY_MODEL, "load_config");
        assert_eq!(context.load_config_document().unwrap()["dsh"]["model"], OVERLAY_MODEL, "load_config_document");
        assert_eq!(
            raw_config_for_write(&context).unwrap()["dsh"]["model"],
            "fixture-model",
            "writers modify the stored file, never the overlaid view"
        );
    });
    assert_eq!(context.load_config().unwrap()["dsh"]["model"], "fixture-model", "the overlay ends with its scope");
    assert_eq!(std::fs::read(&path).unwrap(), stored, "reading never writes the stored file");
}

#[test]
fn config_loader_reports_unavailable_invalid_and_bom_prefixed_files() {
    let (root, context) = overlay_fixture("config overlay loader errors");
    let path = root.0.join("config/config.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("\u{feff}{text}")).unwrap();
    assert_eq!(context.load_config_document().unwrap()["dsh"]["model"], "fixture-model", "a leading BOM is accepted");
    std::fs::write(&path, "[1]").unwrap();
    assert_eq!(context.load_config_document(), Ok(json!([1])), "the document is not normalized");
    assert_eq!(context.load_config(), Err("runtime config is invalid"), "load_config still refuses a non-object");
    std::fs::write(&path, "{ malformed").unwrap();
    assert_eq!(context.load_config_document(), Err(crate::runtime::ConfigReadError::Invalid));
    assert_eq!(context.load_config(), Err("runtime config is invalid"));
    std::fs::remove_file(&path).unwrap();
    assert!(
        matches!(context.load_config_document(), Err(crate::runtime::ConfigReadError::Unavailable(_))),
        "a missing file is unavailable"
    );
    assert_eq!(context.load_config(), Err("runtime path is unavailable"), "load_config keeps the read's own message");
}

#[test]
fn config_stamp_changes_with_the_stored_file() {
    let (root, context) = overlay_fixture("config overlay stamp");
    let path = root.0.join("config/config.json");
    let first = context.config_stamp().unwrap();
    assert_eq!(context.config_stamp().unwrap(), first, "an unchanged file keeps its stamp");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push('\n');
    std::fs::write(&path, &text).unwrap();
    let second = context.config_stamp().unwrap();
    assert_ne!(second, first, "a changed length changes the stamp");
    assert_eq!(second.len, text.len() as u64);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(context.config_stamp(), Err("runtime path is unavailable"), "a missing file has no stamp");
}

#[test]
fn config_overlay_reaches_the_qq_tool_server() {
    let (_root, context) = overlay_fixture("config overlay qq tools");
    with_overlay_model(|| {
        assert_eq!(crate::qq_tools::config(&context).unwrap()["dsh"]["model"], OVERLAY_MODEL);
    });
}

#[test]
fn config_overlay_reaches_the_auxiliary_tool_server() {
    let (_root, context) = overlay_fixture("config overlay aux tools");
    with_overlay_model(|| {
        assert_eq!(crate::aux_mcp::read_config(&context)["dsh"]["model"], OVERLAY_MODEL);
    });
}

#[test]
fn config_overlay_reaches_the_memory_tool_server() {
    let (_root, context) = overlay_fixture("config overlay memory tools");
    with_overlay_model(|| {
        assert_eq!(crate::mcp::served_config(&context)["dsh"]["model"], OVERLAY_MODEL);
    });
}

#[test]
fn config_overlay_reaches_the_prompt_config() {
    let (_root, context) = overlay_fixture("config overlay prompt");
    with_overlay_model(|| {
        assert_eq!(crate::prompt::config_for_runtime(&context)["dsh"]["model"], OVERLAY_MODEL);
    });
}

#[test]
fn config_overlay_reaches_the_role_inject_limit() {
    let (_root, context) = overlay_fixture("config overlay role limit");
    assert_eq!(role_inject_max(&context), 7000, "the stored limit");
    crate::runtime::with_config_overlay("/role/maxInjectChars", json!(9000), || {
        assert_eq!(role_inject_max(&context), 9000);
    });
}

#[test]
fn role_inject_limit_reads_a_bom_prefixed_config() {
    let (root, context) = overlay_fixture("config overlay role limit bom");
    let path = root.0.join("config/config.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("\u{feff}{text}")).unwrap();
    assert_eq!(role_inject_max(&context), 7000, "a leading BOM is accepted, as by load_config");
}

#[test]
fn config_overlay_reaches_the_session_models_view() {
    let (_root, context) = overlay_fixture("config overlay session models");
    with_overlay_model(|| {
        let (status, body) = admin_request(&context, "GET", "/api/session-models", None);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["global"]["model"], OVERLAY_MODEL);
    });
}

#[test]
fn config_overlay_reaches_the_reloaded_whitelist_view() {
    let (root, context) = overlay_fixture("config overlay whitelist");
    access::note_config_start(&context);
    // A save after start: from then on the view shows the file's own members (JS `maybeReloadConfig`).
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["note"] = json!("changed after start");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let cfg = context.load_config().unwrap();
    crate::runtime::with_config_overlay("/ownerQQ", json!(7002), || {
        assert_eq!(access::whitelist_get(&context, &cfg)["ownerQQ"], 7002);
    });
}

#[test]
fn config_overlay_reaches_backup_settings_and_the_archive_keeps_the_stored_file() {
    let (root, context) = overlay_fixture("config overlay backup");
    let stored = std::fs::read(root.0.join("config/config.json")).unwrap();
    with_overlay_model(|| {
        let (bytes, config) = crate::backup::snapshot_config(&context).unwrap();
        assert_eq!(bytes, stored, "the archive keeps config.json as stored");
        assert_eq!(config["dsh"]["model"], OVERLAY_MODEL, "the snapshot settings come through the overlay");
    });
}
