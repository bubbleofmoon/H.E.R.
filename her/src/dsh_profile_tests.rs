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
        .filter(|name| is_own_backup(name))
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

fn codes(list: &[&str]) -> Vec<String> {
    list.iter().map(|code| code.to_string()).collect()
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
        write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2000-01-01T00-0{minute}-00-000Z-00"), "old\n");
    }
    let others = [
        "cordis.patch.yml.bak-by-hand",
        "cordis.patch.yml.bak-her-0-manual",
        "cordis.patch.yml.bak-her-2000-01-01T00-00-00-000Z",
    ];
    for other in others {
        write(&context, &format!("{PROFILE_DIR}/{other}"), "kept\n");
    }
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(restore(&context), Ok(true));
    for other in others {
        assert_eq!(read(&context, &format!("{PROFILE_DIR}/{other}")), "kept\n", "{other}");
    }
    let backups = own_backups(&context);
    assert_eq!(backups.len(), 5, "{backups:?}");
    let expected: Vec<String> =
        (2..6).map(|minute| format!("{BACKUP_PREFIX}2000-01-01T00-0{minute}-00-000Z-00")).collect();
    assert_eq!(backups[..4], expected[..], "{backups:?}");
    assert!(!backups[4].starts_with(&format!("{BACKUP_PREFIX}2000")), "{backups:?}");
}

#[test]
fn guard179_backup_names_are_unique_and_only_own_names_are_pruned() {
    let stamp = "2026-01-02T03-04-05-006Z";
    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| format!("{BACKUP_PREFIX}{name}")).collect()
    }
    assert_eq!(backup_name(stamp, &[]), Some(format!("{BACKUP_PREFIX}{stamp}-00")));
    assert_eq!(
        backup_name(stamp, &names(&[format!("{stamp}-00").as_str(), format!("{stamp}-01").as_str()])),
        Some(format!("{BACKUP_PREFIX}{stamp}-02")),
        "a third backup in the same millisecond"
    );
    // The seventh backup in one millisecond, after pruning removed -00 and -01: never -00 again.
    let pruned: Vec<String> = (2..7).map(|n| format!("{BACKUP_PREFIX}{stamp}-{n:02}")).collect();
    assert_eq!(backup_name(stamp, &pruned), Some(format!("{BACKUP_PREFIX}{stamp}-07")));
    // A clock that went back: the name still sorts after the newest backup.
    assert_eq!(
        backup_name(stamp, &names(&["2026-01-02T03-04-05-007Z-04"])),
        Some(format!("{BACKUP_PREFIX}2026-01-02T03-04-05-007Z-05"))
    );
    assert_eq!(backup_name(stamp, &names(&[format!("{stamp}-99").as_str()])), None, "no name is left after -99");
    assert_eq!(
        backup_name(stamp, &names(&["0-manual", "9999"])),
        Some(format!("{BACKUP_PREFIX}{stamp}-00")),
        "names of another form do not count"
    );
    assert!(is_own_backup(&format!("{BACKUP_PREFIX}{stamp}-07")));
    let others = [
        format!("{BACKUP_PREFIX}{stamp}"),
        format!("{BACKUP_PREFIX}0-manual"),
        format!("{BACKUP_PREFIX}{stamp}-7x"),
        format!("{BACKUP_PREFIX}{stamp}-07-old"),
        "cordis.patch.yml.bak-by-hand".to_string(),
    ];
    for other in others {
        assert!(!is_own_backup(&other), "{other}");
    }
}

#[test]
fn guard179_a_new_backup_sorts_after_the_newest_and_none_is_written_when_no_name_is_left() {
    let (_root, context) = fixture("backup-order");
    // Five backups stamped later than now (a clock that went back): the new one must still be the newest kept.
    for counter in 0..5 {
        write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2999-12-31T23-59-59-999Z-{counter:02}"), "old\n");
    }
    write(&context, PROFILE_FILE, PROFILE_HAND);
    assert_eq!(restore(&context), Ok(true));
    let backups = own_backups(&context);
    assert_eq!(backups.len(), 5, "{backups:?}");
    assert_eq!(backups[4], format!("{BACKUP_PREFIX}2999-12-31T23-59-59-999Z-05"), "{backups:?}");
    assert_eq!(read(&context, &format!("{PROFILE_DIR}/{}", backups[4])), PROFILE_HAND);
    // No name sorts after -99: the restore refuses and writes nothing.
    write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2999-12-31T23-59-59-999Z-99"), "old\n");
    write(&context, PROFILE_FILE, PROFILE_HAND);
    let before = own_backups(&context);
    assert!(restore(&context).is_err());
    assert_eq!(read(&context, PROFILE_FILE), PROFILE_HAND);
    assert_eq!(own_backups(&context), before);
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
    write(&context, &format!("{PROFILE_DIR}/{BACKUP_PREFIX}2000-01-01T00-00-00-000Z-00"), PROFILE_HAND);
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
        (format!("{good}- id: system-prompt\n  config:\n    includeHarnessIdentity: true\n"), codes(&["system-prompt:profile"])),
        (format!("{good}- id: harness-dev-context\n  disabled: false\n"), codes(&["dev-context:profile"])),
        (format!("{good}- id: tool-pwsh\n  disabled: false\n"), codes(&["local-tools:profile"])),
        (format!("{good}- id: agent-instructions\n  config:\n    maxBytes: 65536\n"), codes(&["agent-instructions:profile"])),
        (
            format!("- insert:\n    - id: second-context\n      name: '@deepseek-ai/dsh-harness-dev-context'\n{good}"),
            codes(&["dev-context:profile"]),
        ),
        (
            format!("- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: tool-bash\n        name: '@deepseek-ai/dsh-tool-bash'\n{good}"),
            codes(&["local-tools:profile", "missing-rows:profile"]),
        ),
        (
            format!("- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: persona\n        name: '@deepseek-ai/dsh-persona'\n        config:\n          prefix: 像 AI 一样回答\n{good}"),
            codes(&["missing-rows:profile", "persona-text:profile"]),
        ),
        (format!("[]\n{good}"), codes(&["profile-file"])),
        (format!("{good}{}", render_block()), codes(&["profile-file"])),
    ];
    for (text, expected) in cases {
        write(&context, PROFILE_FILE, &text);
        assert_eq!(problems(&context), expected, "{text}");
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
fn guard179_rows_above_the_block_are_checked_unless_the_block_overrides_them() {
    let (_root, context) = fixture("above");
    write_compliant(&context);
    let good = read(&context, PROFILE_FILE);
    let above = [
        "- id: shell-two\n  name: '@deepseek-ai/dsh-tool-pwsh'\n",
        "- id: persistent-pwsh\n  disabled: false\n",
        "- id: tool-terminal\n  disabled: false\n",
    ];
    for row in above {
        write(&context, PROFILE_FILE, &format!("{row}{good}"));
        assert_eq!(problems(&context), vec!["local-tools:profile".to_string()], "{row}");
    }
    write(&context, PROFILE_FILE, &format!("- id: harness-dev-context\n  disabled: false\n{good}"));
    assert_eq!(check(&context), Ok(()), "HER's block disables harness-dev-context after this row");
}

#[test]
fn guard179_each_guarded_preset_row_is_required() {
    let (_root, context) = fixture("preset-rows");
    write_compliant(&context);
    let cases = [
        ("              maxBytes: 0\n", "              maxBytes: 65536\n", "agent-instructions:preset1"),
        (PWSH_ON.0, PWSH_ON.1, "local-tools:preset1"),
        (
            "            name: '@deepseek-ai/dsh-tool-fs'\n            disabled: true\n",
            "            name: '@deepseek-ai/dsh-tool-fs'\n",
            "local-tools:preset1",
        ),
        (
            "            name: '@deepseek-ai/dsh-tool-fs-search'\n            disabled: true\n",
            "            name: '@deepseek-ai/dsh-tool-fs-search'\n",
            "local-tools:preset1",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: tool-bash\n            name: '@deepseek-ai/dsh-tool-bash'\n          - id: qq-tool-restrict\n",
            "local-tools:preset1",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: shell-two\n            name: '@deepseek-ai/dsh-tool-pwsh-persistent'\n          - id: qq-tool-restrict\n",
            "local-tools:preset1",
        ),
        (
            "          - id: qq-tool-restrict\n",
            "          - id: harness-dev-context\n            name: '@deepseek-ai/dsh-harness-dev-context'\n          - id: qq-tool-restrict\n",
            "dev-context:preset1",
        ),
        ("                说话简短。\n", "                说话简短，像 AI 一样。\n", "persona-text:preset1"),
        (
            "          - id: qq-tool-restrict\n",
            "          - {id: shell-three, name: '@deepseek-ai/dsh-tool-pwsh'}\n          - id: qq-tool-restrict\n",
            "yaml:preset1",
        ),
        (
            "            name: qq-agent-presets/qq-tool-restrict.mjs\n",
            "            name: qq-agent-presets/qq-tool-restrict.mjs # '@deepseek-ai/dsh-tool-pwsh'\n",
            "local-tools:preset1",
        ),
        ("        plugins:\n", "        plugins: &rows\n", "yaml:preset1"),
        (
            "        name: Fixture chat\n",
            "        name: \"Fixture\n          - id: hidden\n            name: '@deepseek-ai/dsh-tool-pwsh'\n          chat\"\n",
            "local-tools:preset1",
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
fn guard179_a_preset_missing_a_guarded_row_refuses() {
    let (_root, context) = fixture("missing-rows");
    write_compliant(&context);
    let agent_row = "          - id: agent-instructions\n            name: '@deepseek-ai/dsh-agent-instructions'\n            config:\n              maxBytes: 0\n";
    edit_preset(&context, |text| text.replacen(agent_row, "", 1));
    assert_eq!(problems(&context), vec!["missing-rows:preset1".to_string()], "missing agent-instructions");
    write(&context, PRESET_FILE, PRESET);
    // Omitting the local-tool rows (like the first-gen and second-gen chat presets) passes because presets mount only listed
    // plugins and HER's profile block disables them globally; any local-tool row that is present still must disable.
    let tool_rows = [
        "          - id: tool-bash\n            name: '@deepseek-ai/dsh-tool-bash'\n            disabled: true\n",
        "          - id: tool-pwsh\n            name: '@deepseek-ai/dsh-tool-pwsh'\n            disabled: true\n",
        "          - id: tool-fs\n            name: '@deepseek-ai/dsh-tool-fs'\n            disabled: true\n",
        "          - id: tool-fs-search\n            name: '@deepseek-ai/dsh-tool-fs-search'\n            disabled: true\n            config:\n              sampleOverCapGlobResults: false\n",
    ];
    edit_preset(&context, |text| tool_rows.iter().fold(text.to_string(), |acc, row| acc.replacen(row, "", 1)));
    assert_eq!(check(&context), Ok(()), "omitted local-tool rows pass when agent-instructions is present");
    edit_preset(&context, |text| {
        text.replacen(
            "          - id: qq-tool-restrict\n",
            "          - id: tool-pwsh\n            name: '@deepseek-ai/dsh-tool-pwsh'\n          - id: qq-tool-restrict\n",
            1,
        )
    });
    assert_eq!(problems(&context), vec!["local-tools:preset1".to_string()], "present local-tool row without disabled");
    write(&context, PRESET_FILE, "# a preset file without a preset\n");
    assert_eq!(problems(&context), vec!["missing-rows:preset1".to_string()], "an emptied preset file");
    let non_preset_with_plugins = "- id: other-plugin\n  name: '@deepseek-ai/other-plugin'\n  config:\n    plugins:\n      - id: agent-instructions\n        disabled: true\n";
    write(&context, PRESET_FILE, non_preset_with_plugins);
    assert_eq!(
        problems(&context),
        vec!["missing-rows:preset1".to_string()],
        "a bundle file with config.plugins but no preset row"
    );
    write(&context, PRESET_FILE, PRESET);
    let good = read(&context, PROFILE_FILE);
    let without_agent = "- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: tool-web\n        disabled: true\n";
    write(&context, PROFILE_FILE, &format!("{without_agent}{good}"));
    assert_eq!(problems(&context), vec!["missing-rows:profile".to_string()], "a profile patch replacing a preset's plugins");
    let with_agent = "- id: preset-fixture-chat\n  config:\n    plugins:\n      - id: agent-instructions\n        disabled: true\n";
    write(&context, PROFILE_FILE, &format!("{with_agent}{good}"));
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_only_the_presets_own_plugin_rows_count() {
    let (_root, context) = fixture("plugin-level");
    write_compliant(&context);
    let agent_row = "          - id: agent-instructions\n            name: '@deepseek-ai/dsh-agent-instructions'\n            config:\n              maxBytes: 0\n";
    let cases = [
        // An `agent-instructions` item nested in another plugin's config list.
        (
            "              fetch: false\n",
            "              fetch: false\n              examples:\n                - id: agent-instructions\n                  disabled: true\n",
        ),
        // An `agent-instructions` item in a list beside `plugins` in the preset's config, at the plugin items' column.
        (
            "            config: {}\n",
            "            config: {}\n        examples:\n          - id: agent-instructions\n            disabled: true\n",
        ),
    ];
    for (find, replace) in cases {
        edit_preset(&context, |text| text.replacen(agent_row, "", 1).replacen(find, replace, 1));
        assert_eq!(problems(&context), vec!["missing-rows:preset1".to_string()], "{replace}");
        write(&context, PRESET_FILE, PRESET);
    }
    assert_eq!(check(&context), Ok(()));
}

#[test]
fn guard179_managed_block_tolerates_dsh_blank_lines_and_refuses_non_space_lines() {
    let (_root, context) = fixture("blank-lines");
    write_compliant(&context);
    let block = render_block();
    // DSH's YAML serializer inserts a blank line right before the trailing `# === her profile guard END ===` comment.
    let with_trailing_blank = block.replacen(&format!("{END}\n"), &format!("\n{END}\n"), 1);
    assert_ne!(with_trailing_blank, block);
    let with_inner_blanks = with_trailing_blank
        .replacen("- id: harness-dev-context\n", "\n- id: harness-dev-context\n", 1)
        .replacen("- id: tool-bash\n", "   \n- id: tool-bash\n", 1);
    for variant in [&with_trailing_blank, &with_inner_blanks] {
        write(&context, PROFILE_FILE, &format!("{PROFILE_HAND}{variant}"));
        assert_eq!(check(&context), Ok(()), "{variant}");
    }
    // `restore` canonicalizes a profile containing DSH's inserted blank line back to `render_block()`.
    write(&context, PROFILE_FILE, &format!("{PROFILE_HAND}{with_trailing_blank}"));
    assert_eq!(restore(&context), Ok(true));
    assert_eq!(read(&context, PROFILE_FILE), format!("{PROFILE_HAND}{block}"));
    assert_eq!(check(&context), Ok(()));

    for bad_line in ["\t\n", "\u{a0}\n", "\u{3000}\n", "# extra comment\n"] {
        let bad_block = block.replacen(&format!("{END}\n"), &format!("{bad_line}{END}\n"), 1);
        write(&context, PROFILE_FILE, &format!("{PROFILE_HAND}{bad_block}"));
        assert_eq!(problems(&context), vec!["profile-file".to_string()], "{bad_line:?}");
    }
}

#[test]
fn guard179_only_plain_keys_and_readable_values_are_read() {
    let (_root, context) = fixture("allowlist");
    write_compliant(&context);
    let persona = "              prefix: |-\n                你是群里的普通成员。\n                - 不用列表回答。\n                说话简短。\n";
    let cases = [
        ("              \"prefix\" : 像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix: !!str \"\\u0041I\"\n", "yaml:preset1"),
        ("              prefix: !!str 像 AI 一样回答\n", "yaml:preset1"),
        ("              \"prefix\": 像 AI 一样回答\n", "yaml:preset1"),
        ("              'prefix': 像 AI 一样回答\n", "yaml:preset1"),
        ("              \"pre\\u0066ix\": 像 AI 一样回答\n", "yaml:preset1"),
        ("              ? prefix\n              : 像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix : 像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix: &text 像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix: *text\n", "yaml:preset1"),
        ("              prefix: 规则: 像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix: 你好\n                像 AI 一样回答\n", "yaml:preset1"),
        ("              prefix: 像 AI 一样回答\n", "persona-text:preset1"),
        ("              prefix: '像 AI 一样回答'\n", "persona-text:preset1"),
        ("              prefix: \"像 AI 一样回答\"\n", "persona-text:preset1"),
    ];
    for (replace, expected) in cases {
        edit_preset(&context, |text| text.replacen(persona, replace, 1));
        assert_eq!(problems(&context), vec![expected.to_string()], "{replace}");
        write(&context, PRESET_FILE, PRESET);
    }
}

#[test]
fn guard179_quoted_scalars_are_read_fully_or_refused() {
    let (_root, context) = fixture("quoting");
    write_compliant(&context);
    let persona = "              prefix: |-\n                你是群里的普通成员。\n                - 不用列表回答。\n                说话简短。\n";
    let cases = [
        ("              prefix: '你好 '' AI'\n", "persona-text:preset1"),
        ("              prefix: '你好 ''\n                AI 回答'\n", "persona-text:preset1"),
        ("              prefix: \"\\u0041I\"\n", "yaml:preset1"),
        ("              prefix: '你好' 回答\n", "yaml:preset1"),
    ];
    for (replace, expected) in cases {
        edit_preset(&context, |text| text.replacen(persona, replace, 1));
        assert_eq!(problems(&context), vec![expected.to_string()], "{replace}");
        write(&context, PRESET_FILE, PRESET);
    }
    edit_preset(&context, |text| text.replacen(persona, "              prefix: '说话简短，别用''列表'''\n", 1));
    assert_eq!(check(&context), Ok(()), "escaped quotes alone are read");
}

#[test]
fn denied_term_policy_persona_text_refuses_full_width_zero_width_and_lower_case_forms() {
    let (_root, context) = fixture("persona-forms");
    write_compliant(&context);
    let persona = "              prefix: |-\n                你是群里的普通成员。\n                - 不用列表回答。\n                说话简短。\n";
    for replace in [
        "              prefix: 像 ＡＩ 一样回答\n",
        "              prefix: 像 A\u{200b}I 一样回答\n",
        "              prefix: 像 ai 一样回答\n",
        "              prefix: 别提 ｄｓｈ\n",
    ] {
        edit_preset(&context, |text| text.replacen(persona, replace, 1));
        assert_eq!(problems(&context), codes(&["persona-text:preset1"]), "{replace:?}");
        write(&context, PRESET_FILE, PRESET);
    }
    assert_eq!(check(&context), Ok(()), "the restored preset passes");
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
        (json!({"dsh": {"bundle": {"patch": ["./presets/missing.patch.yml"]}}}).to_string(), "preset-bundle:preset1"),
        (json!({"dsh": {"bundle": {"patch": ["../escape.patch.yml"]}}}).to_string(), "preset-bundle:preset1"),
    ];
    for (text, expected) in cases {
        write(&context, &manifest, &text);
        assert_eq!(problems(&context), vec![expected.to_string()], "{text}");
    }
    write(&context, &manifest, MANIFEST);
    write(&context, PRESET_FILE, &format!("{}\n", "#".repeat(1024 * 1024)));
    assert_eq!(problems(&context), vec!["preset-bundle:preset1".to_string()], "a preset file over 1 MiB");
    write(&context, PRESET_FILE, PRESET);
    std::fs::remove_file(home(&context).join(&manifest)).unwrap();
    assert_eq!(problems(&context), vec!["preset-bundle".to_string()]);
    write(&context, &manifest, &json!({"dsh": {"bundle": {"patch": "./presets/fixture-chat.patch.yml"}}}).to_string());
    assert_eq!(check(&context), Ok(()), "one preset file named as a string");
}

#[test]
fn guard179_codes_name_preset_files_by_position_only() {
    let (_root, context) = fixture("labels");
    write_compliant(&context);
    write(
        &context,
        "bundles/qq-agent-presets/presets/openai-chat.patch.yml",
        &PRESET.replacen(PWSH_ON.0, PWSH_ON.1, 1),
    );
    let list = ["./presets/fixture-chat.patch.yml", "./presets/openai-chat.patch.yml", "./presets/deepseek.patch.yml"];
    write(&context, &format!("{PRESET_BUNDLE}/package.json"), &json!({"dsh": {"bundle": {"patch": list}}}).to_string());
    let found = problems(&context);
    assert_eq!(found, codes(&["local-tools:preset2", "preset-bundle:preset3"]));
    for code in &found {
        assert!(crate::model_text_policy::denied_terms(code).is_empty(), "{code}");
    }
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
    assert_eq!(error, format!("{REFUSAL_PREFIX}local-tools:preset1"));
    assert_eq!(alerts(&context).len(), 1);
    assert_eq!(std::fs::read_to_string(&alert_file).unwrap(), "local-tools:preset1");
    assert!(admit(&context).is_err());
    assert_eq!(alerts(&context).len(), 1, "the same gap alerts once; the codes are kept for a restart");
    edit_preset(&context, |text| text.replacen("maxBytes: 0", "maxBytes: 65536", 1));
    assert_eq!(
        admit(&context).unwrap_err(),
        format!("{REFUSAL_PREFIX}agent-instructions:preset1, local-tools:preset1")
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
        assert!(crate::model_text_policy::denied_terms(text).is_empty(), "{text}");
    }
}

#[test]
fn guard179_a_refused_owner_notice_is_offered_again() {
    let (_root, context) = fixture("notice");
    force(&context);
    write_compliant(&context);
    let alert_file = context.runtime_root().unwrap().join("data").join(ALERT_FILE);
    refuse_notices(&context, true);
    edit_preset(&context, |text| text.replacen(PWSH_ON.0, PWSH_ON.1, 1));
    assert!(admit(&context).is_err());
    assert!(!alert_file.exists(), "a refused notice is not kept as sent");
    refuse_notices(&context, false);
    assert!(admit(&context).is_err());
    assert_eq!(alerts(&context).len(), 1, "the refused notice is offered again at the next prompt");
    assert_eq!(std::fs::read_to_string(&alert_file).unwrap(), "local-tools:preset1");
    assert!(admit(&context).is_err());
    assert_eq!(alerts(&context).len(), 1);
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
    assert!(raised[0].contains("local-tools:preset1"), "{raised:?}");
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

/// Read one HTTP request (head and body) from `stream`.
fn read_request(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "incomplete request");
        bytes.extend_from_slice(&chunk[..n]);
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
        let len = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
        if bytes.len() >= end + 4 + len {
            return;
        }
    }
}

/// A fake DSH that answers the first request with 401 after the preset lost its `pwsh` guard, then counts the
/// requests that still arrive within two seconds (each answered 401 too).
fn fake_401_breaking_the_preset(
    listener: std::net::TcpListener,
    preset: std::path::PathBuf,
) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        use std::io::Write;
        let answer = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        let text = std::fs::read_to_string(&preset).unwrap();
        std::fs::write(&preset, text.replacen(PWSH_ON.0, PWSH_ON.1, 1)).unwrap();
        stream.write_all(answer).unwrap();
        drop(stream);
        listener.set_nonblocking(true).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut repeats = 0;
        while std::time::Instant::now() < end {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    read_request(&mut stream);
                    stream.write_all(answer).unwrap();
                    repeats += 1;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        repeats
    })
}

#[test]
fn guard179_the_rpc_asks_the_guard_again_before_a_401_repeat() {
    for bounded in [false, true] {
        let (_root, context) = fixture("retry");
        force(&context);
        write_compliant(&context);
        write(&context, ".credentials.yaml", &format!("secret: \"{}\"\n", "A".repeat(43)));
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = fake_401_breaking_the_preset(listener, home(&context).join(PRESET_FILE));
        let rpc = crate::dsh::Rpc::connect_with_runtime(&context, &base).unwrap();
        let prompt = json!({"sessionId": "s", "mode": "queue", "content": []});
        let result = if bounded {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            rpc.call_with_cancellation("session/prompt", prompt, deadline, &|| false)
        } else {
            rpc.call("session/prompt", prompt)
        };
        let repeats = server.join().unwrap();
        assert_eq!(repeats, 0, "bounded={bounded}: the repeat after a 401 reached DSH ({result:?})");
        let error = result.unwrap_err();
        assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("local-tools:preset1"), "{error}");
    }
}

#[test]
fn guard179_the_rpc_guards_session_create_like_a_prompt() {
    // Every caller (the slang learner included) creates its DSH session through the RPC before any prompt.
    let create = json!({"workspaceId": "w", "agentPreset": "fixture-chat"});
    for bounded in [false, true] {
        let (_root, context) = fixture("session-create");
        force(&context);
        write_compliant(&context);
        write(&context, ".credentials.yaml", &format!("secret: \"{}\"\n", "A".repeat(43)));
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = fake_401_breaking_the_preset(listener, home(&context).join(PRESET_FILE));
        let rpc = crate::dsh::Rpc::connect_with_runtime(&context, &base).unwrap();
        let result = if bounded {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            rpc.call_with_cancellation("session/create", create.clone(), deadline, &|| false)
        } else {
            rpc.call("session/create", create.clone())
        };
        let repeats = server.join().unwrap();
        assert_eq!(repeats, 0, "bounded={bounded}: the repeated session/create after a 401 reached DSH ({result:?})");
        let error = result.unwrap_err();
        assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("local-tools:preset1"), "{error}");
    }
    let (_root, context) = fixture("session-create-missing");
    force(&context);
    write(&context, ".credentials.yaml", &format!("secret: \"{}\"\n", "A".repeat(43)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let rpc = crate::dsh::Rpc::connect_with_runtime(&context, &base).unwrap();
    let error = rpc.call("session/create", create.clone()).unwrap_err();
    assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("preset-bundle"), "{error}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let error = rpc.call_with_deadline("session/create", create, deadline).unwrap_err();
    assert!(error.starts_with(REFUSAL_PREFIX) && error.contains("preset-bundle"), "{error}");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no session/create may reach DSH"
    );
}

#[test]
fn guard179_a_backup_never_replaces_an_existing_file() {
    let (_root, context) = fixture("create-new");
    let name = format!("{BACKUP_PREFIX}2026-01-02T03-04-05-006Z-00");
    write(&context, &format!("{PROFILE_DIR}/{name}"), "foreign\n");
    assert!(write_backup(&context, &name, "old\n").is_err(), "an existing file is never replaced");
    assert_eq!(read(&context, &format!("{PROFILE_DIR}/{name}")), "foreign\n");
    // Another spelling of HER's prefix names HER's file on a case-insensitive file system (Windows) only.
    let stamp = "2999-12-31T23-59-59-999Z";
    let upper = format!("cordis.patch.yml.bak-HER-{stamp}-05");
    assert_eq!(is_own_backup(&upper), cfg!(windows));
    let next = backup_name("2026-01-02T03-04-05-006Z", &[format!("{BACKUP_PREFIX}{stamp}-04"), upper]);
    let counter = if cfg!(windows) { "06" } else { "05" };
    assert_eq!(next, Some(format!("{BACKUP_PREFIX}{stamp}-{counter}")));
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
    let raised = alerts(&context);
    assert_eq!(raised.len(), 2, "{raised:?}");
    assert!(raised[0].contains("profile-restore"), "the failed restore alerts the owner: {raised:?}");
    assert!(raised[1].contains("profile-file"), "{raised:?}");
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
    assert_eq!(problems(&context), vec!["preset-bundle:preset1".to_string(), "profile-file".to_string()]);
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

#[test]
fn guard179_bare_line_breaks_are_refused() {
    let (_root, context) = fixture("bare-breaks");
    write_compliant(&context);
    let persona = "              prefix: |-\n                你是群里的普通成员。\n                - 不用列表回答。\n                说话简短。\n";
    // A bare carriage return is a line break for DSH's YAML loader: `prefix: AI` is a row here, not a comment.
    edit_preset(&context, |text| text.replacen(persona, "              # comment\r              prefix: AI\n", 1));
    assert_eq!(problems(&context), codes(&["yaml:preset1"]), "a bare CR in a preset");
    write(&context, PRESET_FILE, PRESET);
    // A bare carriage return after the managed block hides an enabled tool row from the check.
    let profile = format!("{}# comment\r- id: tool-pwsh\r  disabled: false\n", read(&context, PROFILE_FILE));
    write(&context, PROFILE_FILE, &profile);
    assert_eq!(problems(&context), codes(&["yaml:profile"]), "a bare CR in the profile");
    write_compliant(&context);
    // U+2028 is a line break for the YAML loader too.
    edit_preset(&context, |text| text.replacen(persona, "              # note\u{2028}              prefix: AI\n", 1));
    assert_eq!(problems(&context), codes(&["yaml:preset1"]), "U+2028 in a preset");
}

#[test]
fn guard179_only_yaml_separation_whitespace_is_trimmed() {
    let (_root, context) = fixture("separation-whitespace");
    write_compliant(&context);
    let persona = "              prefix: |-\n                你是群里的普通成员。\n                - 不用列表回答。\n                说话简短。\n";
    // DSH keeps U+00A0 and U+3000 as scalar content: the `#` after them is part of the scalar, and `AI` is persona text.
    let cases = [
        "              prefix: \u{a0}# AI\n",
        "              prefix: \u{3000}# AI\n",
    ];
    for replace in cases {
        edit_preset(&context, |text| text.replacen(persona, replace, 1));
        assert_eq!(problems(&context), codes(&["persona-text:preset1"]), "{replace:?}");
        write(&context, PRESET_FILE, PRESET);
    }
}
