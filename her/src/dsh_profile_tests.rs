// #179: the DSH profile guard over synthetic fixture profiles and presets (no production file is read or written).
use super::test_support::*;
use super::*;
use serde_json::json;

fn read(context: &RuntimeContext, relative: &str) -> String {
    std::fs::read_to_string(home(context).join(relative)).unwrap()
}

fn own_backups(context: &RuntimeContext) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home(context).join(PROFILE_DIR))
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter(|name| name.starts_with(BACKUP_PREFIX))
        .collect();
    names.sort();
    names
}

fn problems(context: &RuntimeContext) -> Vec<String> {
    match check(context) {
        Ok(()) => Vec::new(),
        Err(problems) => problems,
    }
}

#[test]
fn guard179_restore_appends_the_rows_last_and_keeps_every_other_line() {
    let (_root, context) = fixture("append");
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), format!("{PROFILE_HAND}{}", render_block()));
    let backups = own_backups(&context);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(read(&context, &format!("{PROFILE_DIR}/{}", backups[0])), PROFILE_HAND);
    assert_eq!(restore(&context), Ok(false), "an unchanged profile is not rewritten");
    assert_eq!(own_backups(&context).len(), 1);
}

#[test]
fn guard179_restore_creates_a_missing_profile_and_replaces_the_new_profile_template() {
    let (_root, context) = fixture("create");
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), render_block());
    assert!(own_backups(&context).is_empty());
    write(&context, PROFILE_FILE, "# the new profile template\n[]\n");
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), format!("# the new profile template\n{}", render_block()));
}

#[test]
fn guard179_restore_moves_rows_written_after_the_block_above_it() {
    let (_root, context) = fixture("move");
    write_compliant(&context);
    let late = "- id: harness-dev-context\n  disabled: false\n";
    write(&context, PROFILE_FILE, &format!("{PROFILE_HAND}{}{late}", render_block()));
    assert_eq!(problems(&context), vec!["dev-context:profile".to_string()]);
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), format!("{PROFILE_HAND}{late}{}", render_block()));
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_restore_refuses_broken_markers_and_a_too_large_file_and_changes_nothing() {
    let (_root, context) = fixture("refuse");
    let cases = [
        format!("{}{}", render_block(), render_block()),
        format!("{END}\n{BEGIN}\n"),
        format!("{}\n", "#".repeat(1024 * 1024)),
    ];
    for text in cases {
        write(&context, PROFILE_FILE, &text);
        assert!(restore(&context).is_err(), "{} bytes", text.len());
        assert_eq!(read(&context, PROFILE_FILE), text);
    }
    assert!(own_backups(&context).is_empty());
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(restore(&context), Ok(true), "a sound file is written after the refused ones");
}

#[test]
fn guard179_restore_keeps_five_own_backups_and_never_touches_other_backups() {
    let (_root, context) = fixture("prune");
    for minute in 0..6 {
        write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2000-01-01T00-0{minute}-00-000Z"), "old\n");
    }
    write(&context, &format!("{PROFILE_DIR}/cordis.patch.yml.bak-by-hand"), "kept\n");
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(restore(&context), Ok(true));
    let backups = own_backups(&context);
    assert_eq!(backups.len(), 5, "{backups:?}");
    let expected: Vec<String> =
        (2..6).map(|minute| format!("{BACKUP_PREFIX}2000-01-01T00-0{minute}-00-000Z")).collect();
    assert_eq!(backups[..4], expected[..], "{backups:?}");
    assert!(!backups[4].starts_with(&format!("{BACKUP_PREFIX}2000")), "{backups:?}");
    assert_eq!(read(&context, &format!("{PROFILE_DIR}/cordis.patch.yml.bak-by-hand")), "kept\n");
}

#[test]
fn guard179_two_restores_at_once_write_one_block_and_one_backup() {
    let (_root, context) = fixture("concurrent");
    write(&context, PROFILE_FILE, PROFILE_HAND);
    let mut written: Vec<bool> = std::thread::scope(|scope| {
        let first = scope.spawn(|| restore(&context));
        let second = scope.spawn(|| restore(&context));
        vec![first.join().unwrap().unwrap(), second.join().unwrap().unwrap()]
    });
    written.sort();
    assert_eq!(written, vec![false, true]);
    assert_eq!(read(&context, PROFILE_FILE).matches(BEGIN).count(), 1);
    assert_eq!(own_backups(&context).len(), 1);
}

#[test]
fn guard179_a_restore_cut_off_after_its_backup_is_finished_by_the_next_start() {
    let (_root, context) = fixture("resume");
    write_compliant(&context);
    // What a restore leaves when it stops between its backup and its write.
    write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2000-01-01T00-00-00-000Z"), PROFILE_HAND);
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(problems(&context), vec!["profile-file".to_string()]);
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(check(&context), Ok(()));
    assert_eq!(own_backups(&context).len(), 2);
}

#[test]
fn guard179_a_compliant_profile_and_preset_pass() {
    let (_root, context) = fixture("compliant");
    write_compliant(&context);
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_each_guarded_profile_row_is_required() {
    let (_root, context) = fixture("profile-rows");
    write_compliant(&context);
    let good = read(&context, PROFILE_FILE);
    let rows = [
        "    includeHarnessIdentity: false\n",
        "    includeRuntimeContext: false\n",
        "    personaPrefix: ''\n",
        "    personaSuffix: ''\n",
        "- id: harness-dev-context\n  disabled: true\n",
        "- id: agent-instructions\n  disabled: true\n",
        "- id: tool-bash\n  disabled: true\n",
        "- id: tool-pwsh\n  disabled: true\n",
        "- id: tool-fs\n  disabled: true\n",
        "- id: tool-fs-search\n  disabled: true\n",
    ];
    for row in rows {
        let text = good.replacen(row, "", 1);
        assert_ne!(text, good, "{row}");
        write(&context, PROFILE_FILE, &text);
        assert_eq!(problems(&context), vec!["profile-file".to_string()], "{row}");
    }
    let cases = [
        (format!("{good}- id: system-prompt\n  config:\n    includeHarnessIdentity: true\n"), "system-prompt:profile"),
        (format!("{good}- id: harness-dev-context\n  disabled: false\n"), "dev-context:profile"),
        (format!("{good}- id: tool-pwsh\n  disabled: false\n"), "local-tools:profile"),
        (format!("{good}- id: agent-instructions\n  config:\n    maxBytes: 65536\n"), "agent-instructions:profile"),
        (
            format!("- insert:\n    - id: second-context\n      name: '@deepseek-ai/dsh-harness-dev-context'\n{good}"),
            "dev-context:profile",
        ),
        (
            format!("- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: tool-bash\n        name: '@deepseek-ai/dsh-tool-bash'\n{good}"),
            "local-tools:profile",
        ),
        (
            format!("- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: persona\n        name: '@deepseek-ai/dsh-persona'\n        config:\n          prefix: 像 AI 一样回答\n{good}"),
            "persona-text:profile",
        ),
        (format!("[]\n{good}"), "profile-file"),
        (format!("{good}{}", render_block()), "profile-file"),
    ];
    for (text, expected) in cases {
        write(&context, PROFILE_FILE, &text);
        assert_eq!(problems(&context), vec![expected.to_string()], "{text}");
    }
    write(&context, PROFILE_FILE, &format!("- id: system-prompt\n  config:\n    personaPrefix: anything\n{good}"));
    assert_eq!(check(&context), Ok(()), "a row above HER's block is overridden by it");
    let mcp_after = "# === qq-bridge MCP BEGIN ===\n- insert:\n    - id: mcp-snowluma\n      name: '@deepseek-ai/dsh-mcp-client'\n# === qq-bridge MCP END ===\n";
    write(&context, PROFILE_FILE, &format!("{good}{mcp_after}"));
    assert_eq!(check(&context), Ok(()), "an MCP block an older writer appended after HER's block passes");
    std::fs::remove_file(home(&context).join(PROFILE_FILE)).unwrap();
    assert_eq!(problems(&context), vec!["profile-file".to_string()]);
}

#[test]
fn guard179_each_guarded_preset_row_is_required() {
    let (_root, context) = fixture("preset-rows");
    write_compliant(&context);
    let cases = [
        ("              maxBytes: 0\n", "              maxBytes: 65536\n", "agent-instructions:fixture-chat"),
        (PWSH_ON.0, PWSH_ON.1, "local-tools:fixture-chat"),
        (
            "            name: '@deepseek-ai/dsh-tool-fs'\n            disabled: true\n",
            "            name: '@deepseek-ai/dsh-tool-fs'\n",
            "local-tools:fixture-chat",
        ),
        (
            "            name: '@deepseek-ai/dsh-tool-fs-search'\n            disabled: true\n",
            "            name: '@deepseek-ai/dsh-tool-fs-search'\n",
            "local-tools:fixture-chat",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: tool-bash\n            name: '@deepseek-ai/dsh-tool-bash'\n          - id: qq-tool-restrict\n",
            "local-tools:fixture-chat",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: shell-two\n            name: '@deepseek-ai/dsh-tool-pwsh-persistent'\n          - id: qq-tool-restrict\n",
            "local-tools:fixture-chat",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: harness-dev-context\n            name: '@deepseek-ai/dsh-harness-dev-context'\n          - id: qq-tool-restrict\n",
            "dev-context:fixture-chat",
        ),
        ("                说话简短。\n", "                说话简短，像 AI 一样。\n", "persona-text:fixture-chat"),
        (
            "          - id: qq-tool-restrict\n",
            "          - {id: shell-three, name: '@deepseek-ai/dsh-tool-pwsh'}\n          - id: qq-tool-restrict\n",
            "yaml:fixture-chat",
        ),
        (
            "            name: qq-agent-presets/qq-tool-restrict.mjs\n",
            "            name: qq-agent-presets/qq-tool-restrict.mjs # '@deepseek-ai/dsh-tool-pwsh'\n",
            "local-tools:fixture-chat",
        ),
        ("        plugins:\n", "        plugins: &rows\n", "yaml:fixture-chat"),
        (
            "        name: Fixture chat\n",
            "        name: \"Fixture\n          - id: hidden\n            name: '@deepseek-ai/dsh-tool-pwsh'\n          chat\"\n",
            "local-tools:fixture-chat",
        ),
    ];
    for (find, replace, expected) in cases {
        edit_preset(&context, |text| text.replacen(find, replace, 1));
        assert_eq!(problems(&context), vec![expected.to_string()], "{replace}");
        write(&context, PRESET_FILE, PRESET);
    }
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_the_preset_bundle_is_required() {
    let (_root, context) = fixture("bundle");
    write_compliant(&context);
    let manifest = format!("{PRESET_BUNDLE}/package.json");
    let many: Vec<String> = (0..17).map(|n| format!("./presets/p{n}.patch.yml")).collect();
    let cases = [
        ("{".to_string(), "preset-bundle"),
        (json!({"dsh": {"bundle": {"patch": []}}}).to_string(), "preset-bundle"),
        (json!({"dsh": {"bundle": {"patch": many}}}).to_string(), "preset-bundle"),
        (json!({"dsh": {"bundle": {"patch": [7]}}}).to_string(), "preset-bundle"),
        (json!({"dsh": {"bundle": {"patch": ["./presets/missing.patch.yml"]}}}).to_string(), "preset-bundle:missing"),
        (json!({"dsh": {"bundle": {"patch": ["../escape.patch.yml"]}}}).to_string(), "preset-bundle:escape"),
    ];
    for (text, expected) in cases {
        write(&context, &manifest, &text);
        assert_eq!(problems(&context), vec![expected.to_string()], "{text}");
    }
    write(&context, &manifest, MANIFEST);
    write(&context, PRESET_FILE, &format!("{}\n", "#".repeat(1024 * 1024)));
    assert_eq!(problems(&context), vec!["preset-bundle:fixture-chat".to_string()], "a preset file over 1 MiB");
    write(&context, PRESET_FILE, PRESET);
    std::fs::remove_file(home(&context).join(&manifest)).unwrap();
    assert_eq!(problems(&context), vec!["preset-bundle".to_string()]);
    write(&context, &manifest, &json!({"dsh": {"bundle": {"patch": "./presets/fixture-chat.patch.yml"}}}).to_string());
    assert_eq!(check(&context), Ok(()), "one preset file named as a string");
}

#[test]
fn guard179_admit_refuses_and_alerts_once_per_gap_and_clears_on_fix() {
    let (_root, context) = fixture("admit");
    force(&context);
    write_compliant(&context);
    assert_eq!(admit(&context), Ok(()));
    assert!(alerts(&context).is_empty());
    let alert_file = context.runtime_root().unwrap().join("data").join(ALERT_FILE);
    edit_preset(&context, |text| text.replacen(PWSH_ON.0, PWSH_ON.1, 1));
    let error = admit(&context).unwrap_err();
    assert_eq!(error, format!("{REFUSAL_PREFIX}local-tools:fixture-chat"));
    assert_eq!(alerts(&context).len(), 1);
    assert_eq!(std::fs::read_to_string(&alert_file).unwrap(), "local-tools:fixture-chat");
    assert!(admit(&context).is_err());
    assert_eq!(alerts(&context).len(), 1, "the same gap alerts once; the codes are kept for a restart");
    edit_preset(&context, |text| text.replacen("maxBytes: 0", "maxBytes: 65536", 1));
    assert_eq!(
        admit(&context).unwrap_err(),
        format!("{REFUSAL_PREFIX}agent-instructions:fixture-chat, local-tools:fixture-chat")
    );
    assert_eq!(alerts(&context).len(), 2, "a different gap alerts again");
    write(&context, PRESET_FILE, PRESET);
    assert_eq!(admit(&context), Ok(()));
    assert!(!alert_file.exists());
    edit_preset(&context, |text| text.replacen(PWSH_ON.0, PWSH_ON.1, 1));
    assert!(admit(&context).is_err());
    let raised = alerts(&context);
    assert_eq!(raised.len(), 3, "a gap after a fix alerts again");
    for text in raised.iter().chain([&error]) {
        assert!(crate::upstream_capture::denied_terms(text).is_empty(), "{text}");
    }
}

#[test]
fn guard179_development_roots_outside_the_install_root_are_not_guarded() {
    let (_root, context) = fixture("development");
    assert_eq!(admit(&context), Ok(()));
    startup(&context);
    assert!(!home(&context).join(PROFILE_FILE).exists(), "a development root is never written");
    assert!(alerts(&context).is_empty());
}

#[test]
fn guard179_startup_restores_the_rows_and_alerts_about_what_it_cannot_write() {
    let (_root, context) = fixture("startup");
    force(&context);
    write(&context, &format!("{PRESET_BUNDLE}/package.json"), MANIFEST);
    write(&context, PRESET_FILE, PRESET);
    write(&context, PROFILE_FILE, PROFILE_HAND);
    startup(&context);
    assert_eq!(read(&context, PROFILE_FILE), format!("{PROFILE_HAND}{}", render_block()));
    assert!(alerts(&context).is_empty());
    edit_preset(&context, |text| text.replacen(PWSH_ON.0, PWSH_ON.1, 1));
    startup(&context);
    let raised = alerts(&context);
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(raised[0].contains("local-tools:fixture-chat"), "{raised:?}");
}

#[test]
fn guard179_the_rpc_refuses_a_prompt_while_a_row_is_missing() {
    let (_root, context) = fixture("rpc");
    force(&context);
    write(&context, ".credentials.yaml", &format!("secret: \"{}\"\n", "A".repeat(43)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let rpc = crate::dsh::Rpc::connect_with_runtime(&context, &base).unwrap();
    let prompt = json!({"sessionId": "s", "mode": "queue", "content": []});
    let error = rpc.call("session/prompt", prompt.clone()).unwrap_err();
    assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("preset-bundle"), "{error}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let error = rpc.call_with_deadline("session/prompt", prompt, deadline).unwrap_err();
    assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("preset-bundle"), "{error}");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no request may reach DSH"
    );
}

#[test]
fn guard179_restore_keeps_an_old_crlf_profile_and_drops_its_bom() {
    let (_root, context) = fixture("crlf");
    write_compliant(&context);
    let old = "# an old profile\r\n- id: tool-web\r\n  disabled: true\r\n";
    write(&context, PROFILE_FILE, &format!("\u{feff}{old}"));
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), format!("{old}{}", render_block()));
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_a_failed_restore_is_recorded_and_prompts_stay_refused() {
    let (_root, context) = fixture("unwritable");
    force(&context);
    write(&context, &format!("{PRESET_BUNDLE}/package.json"), MANIFEST);
    write(&context, PRESET_FILE, PRESET);
    // A file where the profile folder belongs: nothing can be written there (as on a full or read-only disk).
    write(&context, PROFILE_DIR, "not a folder\n");
    startup(&context);
    assert_eq!(read(&context, PROFILE_DIR), "not a folder\n");
    assert_eq!(admit(&context), Err(format!("{REFUSAL_PREFIX}profile-file")));
    assert_eq!(alerts(&context).len(), 1);
}

#[cfg(unix)]
#[test]
fn guard179_linked_profile_and_preset_files_are_refused() {
    let (_root, context) = fixture("linked");
    write_compliant(&context);
    for (index, relative) in [PROFILE_FILE, PRESET_FILE].into_iter().enumerate() {
        let outside = context.runtime_root().unwrap().join(format!("outside-{index}.yml"));
        let path = home(&context).join(relative);
        std::fs::copy(&path, &outside).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
    }
    assert_eq!(problems(&context), vec!["preset-bundle:fixture-chat".to_string(), "profile-file".to_string()]);
    assert!(restore(&context).is_err(), "a linked profile is never written through");
    assert!(std::fs::symlink_metadata(home(&context).join(PROFILE_FILE)).unwrap().file_type().is_symlink());
}

#[test]
fn guard179_the_check_command_reports_ok_or_the_problems() {
    let (_root, context) = fixture("cli");
    let root = context.runtime_root().unwrap().to_string_lossy().into_owned();
    let args = |command: &str| vec![command.to_string(), "--runtime-root".to_string(), root.clone()];
    assert_eq!(run_cli(&args("check")), 1, "nothing is in place yet");
    write_compliant(&context);
    assert_eq!(run_cli(&args("check")), 0);
    assert_eq!(run_cli(&args("write")), 2);
    assert_eq!(run_cli(&["check".to_string()]), 2);
}
