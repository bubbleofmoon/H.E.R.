// #140: prompt text as live data.
use super::*;

/// A settings fixture whose memory database exists, so a prompt render can recall.
fn prompt_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    drop(initialize_fixture_memory_database(&context));
    (root, context)
}

fn set_text(context: &RuntimeContext, key: &str, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": key, "value": value})))
}

/// What the next compatibility render of a group conversation sends; it carries the behaviour and sticker rules.
fn next_render(context: &RuntimeContext) -> String {
    crate::prompt::build_with_runtime(context, "group:101", "atMention", "小鲸鱼", "9001")
}

#[test]
fn prompt_text_edit_is_live_on_the_next_wake_as_a_section_replacement() {
    let (_root, context) = prompt_fixture("prompt text next wake");
    let key = "group:101";
    context.write_atomic(RuntimeArea::Data, "social-v2.json", json!({"conversations":{
        key:{"agentToken":"epoch-140","bootstrapSent":true,"lastUnreadSeq":1,"lastReadThroughSeq":0,"lastIncomingAt":1_000,
            "unread":[{"seq":1,"sender":"甲","userId":"1001","text":"今天天气不错","time":1_000,"isSelf":false}],
            "recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}}}})
        .to_string().as_bytes()).unwrap();
    crate::session_map::bind(&context, key, "sid-140", None).unwrap();
    let mut allowed = context.load_config().unwrap();
    allowed["allow"]["groups"] = json!([101]);
    context.write_atomic(RuntimeArea::Config, "config.json", allowed.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let open: crate::persona::Authority<'static> = &crate::persona::open_authority;
    let id = crate::persona::create(&context, &json!({"name": "小鲸鱼", "core": "你是一只住在群里的小鲸鱼，爱聊天。"}), open)
        .unwrap()["id"].as_str().unwrap().to_string();
    crate::persona::approve(&context, &json!({"id": id, "version": 1}), open).unwrap();
    crate::persona::assign(&context, &json!({"default": id}), open).unwrap();
    // Each wake's message packet comes from the console route, served here twice.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    let server = std::thread::spawn(move || {
        let startup = std::sync::Arc::new(std::sync::Mutex::new(true));
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, served_cfg.clone(), served.clone(), startup.clone());
        }
    });
    let (briefing, _) = crate::prompt::prepare_dsh_wake(&context, &context.load_config().unwrap(), key, "sid-140", "epoch-140",
        "atMention", "rpc-140-1", "").unwrap();
    assert!(briefing.contains("【简报节 behaviour】\n【在群里怎么说话】"), "{briefing}");
    crate::session_context::observe(&context, key, "sid-140", "epoch-140", &json!({
        "type": "user/message", "seq": 1, "data": {"source": {"rpcId": "rpc-140-1"}},
    })).unwrap();
    let edited = "【别像客服】改过的规则 140";
    crate::settings::apply(&context, "prompt.notAClerk", &json!(edited), "administrator", "console").unwrap();
    let (delta, _) = crate::prompt::prepare_dsh_wake(&context, &context.load_config().unwrap(), key, "sid-140", "epoch-140",
        "atMention", "rpc-140-2", "").unwrap();
    server.join().unwrap();
    assert!(delta.contains("【唤醒增量】"), "{delta}");
    assert!(delta.contains(&format!("【替换简报节 behaviour】完整替换旧节：\n{edited}\n\n【在群里怎么说话】")), "{delta}");
    assert!(!delta.contains("【积极性】"), "participation defaults to empty: {delta}");
    assert!(!delta.contains("【替换简报节 stickers】"), "only the edited section is replaced: {delta}");
}

#[test]
fn prompt_text_save_is_checked_and_a_refusal_keeps_the_last_good_version() {
    let (root, context) = prompt_fixture("prompt text refused");
    assert!(!next_render(&context).contains("【积极性】"), "participation defaults to empty");
    let (status, body) = set_text(&context, "prompt.proactive", json!("【积极性】看心情接话 140"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": "prompt.proactive", "value": "【积极性】看心情接话 140", "revision": 1, "changed": true}));
    assert!(next_render(&context).contains("【积极性】看心情接话 140\n\n【在群里怎么说话】"), "the next render reads it");
    let shape = "prompt.proactive must be text of at most 2000 characters, with no control character but line feed and tab";
    for (value, error) in [
        (json!("你是一个AI，要积极"), "prompt.proactive names a denied term: AI"),
        (json!("我是ai"), "prompt.proactive names a denied term: AI"),
        (json!("别提 DeepSeek 和 机器人"), "prompt.proactive names a denied term: deepseek, 机器人"),
        (json!("ＤＳＨ 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{200b}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{61c}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{180e}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("DSH\u{61c}Ready 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("x".repeat(2001)), shape),
        (json!("一行\r\n两行"), shape),
        (json!("响铃\u{7}"), shape),
        (json!(5), shape),
        (Value::Null, shape),
        (json!(["【积极性】"]), shape),
    ] {
        let (status, body) = set_text(&context, "prompt.proactive", value.clone());
        assert_eq!(status, 400, "{value}: {body}");
        assert_eq!(body["error"], error, "{value}");
    }
    assert!(next_render(&context).contains("【积极性】看心情接话 140\n\n"), "every refusal keeps the last good version");
    assert_eq!(audit_rows(&root).len(), 1, "a refusal writes no audit row");
    // The boundaries: 2000 characters (a Han character counts once) render whole, since a rule section is never cut;
    // line feed and tab pass.
    let longest = "鲸".repeat(2000);
    let (status, body) = set_text(&context, "prompt.proactive", json!(longest));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains(&format!("{longest}\n\n【在群里怎么说话】")), "the longest text renders whole");
    let (status, body) = set_text(&context, "prompt.proactive", json!("第一行\n\t第二行"));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains("第一行\n\t第二行\n\n【在群里怎么说话】"));
    // An emptied text renders nothing; the other rules stay.
    let (status, body) = set_text(&context, "prompt.proactive", json!(""));
    assert_eq!(status, 200, "{body}");
    let text = next_render(&context);
    assert!(!text.contains("【积极性】") && text.contains("【在群里怎么说话】"), "{text}");
}

#[test]
fn settings_followups_global_prompt_fallback_ignores_conversation_audit() {
    let (root, context) = settings_fixture("global prompt fallback");
    let key = "prompt.notAClerk";
    let global = "别像客服，平常说话。";
    let (status, body) = set_text(&context, key, json!(global));
    assert_eq!(status, 200, "{body}");
    let pointer = crate::settings::find(key).unwrap().pointer;
    write_config(&root, |config| {
        *config.pointer_mut(pointer).unwrap() = json!("文件里的规则。");
    });

    let store = StoreProbe::at(&root.0);
    store.set_value(key, &json!("我是 AI").to_string());
    store.insert_audit(&[AuditRow {
        scope: "group:101",
        ..AuditRow::test(key, None, Some(json!("另一段规则。").to_string()), "conversation-prompt")
    }]);

    let document = context.load_config_document().unwrap();
    assert_eq!(
        document.pointer(pointer).and_then(Value::as_str),
        Some(global),
        "global prompt fallback must use global audit rows"
    );
}

#[test]
fn prompt_text_refused_at_load_falls_back_to_its_last_good_version() {
    let (root, context) = prompt_fixture("prompt text load guard");
    for text in ["【别像客服】第一版 140", "【别像客服】第二版 140"] {
        let (status, body) = set_text(&context, "prompt.notAClerk", json!(text));
        assert_eq!(status, 200, "{body}");
    }
    let store = StoreProbe::at(&root.0);
    // A row changed outside `apply`, or one holding a term a later release denies, is refused at load.
    store.set_value("prompt.notAClerk", &json!("【别像客服】我是 AI 140").to_string());
    // Newer audit rows holding refused text too (old data: many of them) are passed over.
    let refused: Vec<AuditRow> = (0..500)
        .map(|n| {
            AuditRow::test(
                "prompt.notAClerk",
                None,
                Some(json!(format!("【别像客服】AI {n}")).to_string()),
                &format!("refused-{n}"),
            )
        })
        .collect();
    store.insert_audit(&refused);
    let text = next_render(&context);
    assert!(text.contains("【别像客服】第二版 140\n\n") && !text.contains("我是 AI"), "the newest accepted audited value: {text}");
    let (status, view) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{view}");
    let entry = view["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == "prompt.notAClerk").unwrap().clone();
    assert_eq!(entry["stored"], "【别像客服】第二版 140", "{entry}");
    assert_eq!(entry["value"], "【别像客服】第二版 140", "{entry}");
    // An audit row without a new value (an undo that removed the stored value writes one) ends the search: the
    // default applies.
    store.insert_audit(&[AuditRow::test("prompt.notAClerk", None, None, "removed")]);
    // The search ends, and the file's value applies when the registry accepts it: the file holds the last text a write
    // stored (#176 writes the stored values back), so the second version still renders.
    assert!(next_render(&context).contains("【别像客服】第二版 140\n\n"), "the file's value after a removal");
    // A file with no prompt text leaves the default.
    write_config(&root, |config| config["prompts"] = json!({}));
    assert!(!next_render(&context).contains("【别像客服】"), "the default after a removal, with nothing in the file");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】第三版 140"));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains("【别像客服】第三版 140\n\n"), "a later save is in effect");

    // Without an accepted stored or audited value, the file's value applies when accepted, else the default.
    let (other_root, other) = prompt_fixture("prompt text file guard");
    write_config(&other_root, |config| config["prompts"] = json!({"behaviour": {"notAClerk": "【别像客服】文件里的 AI 140"}}));
    assert!(!next_render(&other).contains("【别像客服】"), "a refused file value renders the default");
    write_config(&other_root, |config| config["prompts"] = json!({"behaviour": {"notAClerk": "【别像客服】文件里的版本 140"}}));
    assert!(next_render(&other).contains("【别像客服】文件里的版本 140\n\n"), "an accepted file value renders");
    // The store's first write seeds every registry value in the file (#176), so the file holds no prompt text while
    // it is created; the text comes back after, with no stored or audited value for it.
    write_config(&other_root, |config| config["prompts"] = json!({}));
    let (status, body) = set_quiet(&other, json!(4500));
    assert_eq!(status, 200, "{body}");
    write_config(&other_root, |config| config["prompts"] = json!({"behaviour": {"notAClerk": "【别像客服】文件里的版本 140"}}));
    StoreProbe::at(&other_root.0).insert_value("prompt.notAClerk", &json!("【别像客服】库里的 AI 140").to_string());
    assert!(next_render(&other).contains("【别像客服】文件里的版本 140\n\n"), "a refused row with no audited value leaves the file's");
}

#[test]
fn prompt_text_save_cut_off_before_its_audit_row_keeps_the_old_text() {
    let (root, context) = prompt_fixture("prompt text cut off");
    crate::settings::apply(&context, "prompt.groupConduct", &json!("【在群里怎么说话】旧版 140"), "administrator", "console")
        .unwrap();
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let cut = crate::settings::apply(&context, "prompt.groupConduct", &json!("【在群里怎么说话】新版 140"), "administrator", "console");
    assert_eq!(cut.err(), Some((503, "settings storage is unavailable".to_string())));
    let text = next_render(&context);
    assert!(text.contains("【在群里怎么说话】旧版 140\n\n") && !text.contains("新版 140"), "{text}");
    assert_eq!(audit_rows(&root).len(), 1, "the cut-off save left no audit row");
}

#[test]
fn prompt_text_readers_see_one_whole_version_while_writers_save() {
    let (root, context) = prompt_fixture("prompt text concurrent");
    let first = format!("【别像客服】{}", "甲".repeat(1990));
    let second = format!("【别像客服】{}", "乙".repeat(1990));
    // The reader starts once a commit has returned, then reads until both writers have; every such read must show a
    // whole committed version, never the compiled default.
    let committed = std::sync::atomic::AtomicUsize::new(0);
    let (a, b, reads_after_commit) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while committed.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            let mut reads_after_commit = 0;
            loop {
                let done = committed.load(std::sync::atomic::Ordering::SeqCst);
                let text = next_render(&context);
                let default = !text.contains("【别像客服】");
                let seen = [
                    default,
                    text.contains(&format!("{first}\n\n【在群里怎么说话】")),
                    text.contains(&format!("{second}\n\n【在群里怎么说话】")),
                ];
                assert_eq!(seen.iter().filter(|seen| **seen).count(), 1, "one whole version: {text}");
                assert!(!default, "a read after a commit shows the default, not the committed text: {text}");
                reads_after_commit += 1;
                if done == 2 {
                    break;
                }
            }
            reads_after_commit
        });
        let a = scope.spawn(|| {
            let saved = crate::settings::apply(&context, "prompt.notAClerk", &json!(first.as_str()), "administrator", "console");
            committed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            saved.err()
        });
        let b = scope.spawn(|| {
            let saved = crate::settings::apply(&context, "prompt.notAClerk", &json!(second.as_str()), "administrator", "console");
            committed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            saved.err()
        });
        let reads = reader.join().unwrap();
        (a.join().unwrap(), b.join().unwrap(), reads)
    });
    assert_eq!((a, b), (None, None));
    assert!(reads_after_commit > 0, "no read ran after a commit");
    let settled = next_render(&context);
    assert!(settled.contains(&format!("{first}\n\n【在群里怎么说话】")) || settled.contains(&format!("{second}\n\n【在群里怎么说话】")), "{settled}");
    assert_eq!(audit_rows(&root).len(), 2);
}

#[test]
fn denied_term_split_by_format_characters_is_refused_on_save_and_load() {
    // Invisible format characters read as joiners and as word boundaries at once: each spelling reads as `DSH`.
    let rule = crate::settings::find("prompt.notAClerk").unwrap();
    for split in ["x\u{200b}D\u{200b}SH", "x\u{200b}DS\u{200b}H", "x\u{200b}D\u{200b}S\u{200b}H"] {
        assert!(crate::model_text_policy::denied_terms(split).contains(&"dsh".to_string()), "{split:?}");
        assert!(rule.normalize(&json!(split)).is_none(), "{split:?} is accepted on load");
        let (_root, context) = prompt_fixture("prompt text split denied");
        let (status, body) = set_text(&context, "prompt.notAClerk", json!(split));
        assert_eq!(status, 400, "{split:?}: {body}");
    }
    // A split that a letter or digit touches is one word, not the term.
    for clean in ["x\u{200b}DSHy", "XDSHX", "x\u{200b}DSH1"] {
        assert!(!crate::model_text_policy::denied_terms(clean).contains(&"dsh".to_string()), "{clean:?}");
    }
}

#[test]
fn prompt_text_rule_keeps_its_validated_text_when_chat_redaction_changes() {
    let (root, context) = prompt_fixture("prompt text strip denied");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】新版 140"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D鲸SH"));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["鲸"]}));
    let text = next_render(&context);
    assert!(text.contains("【别像客服】D鲸SH\n\n"), "the active rule renders verbatim: {text}");
    assert!(!text.contains("DSH"), "a denied term reached the model: {text}");
}

#[test]
fn prompt_text_strip_joining_rules_across_their_boundary_never_makes_a_denied_term() {
    let (root, context) = prompt_fixture("prompt text boundary strip");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.proactive", json!("SH【积极性】"));
    assert_eq!(status, 200, "{body}");
    // A chat redaction entry can also match the separator between two instructions.
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["\n\n"]}));
    let render = next_render(&context);
    assert!(!crate::model_text_policy::denied_terms(&render).contains(&"dsh".to_string()), "{render}");
    assert!(render.contains("【别像客服】D\n\nSH【积极性】"), "rule boundary must remain intact: {render}");
}

#[test]
fn model_strip_join_regression_preserves_rule_boundary_when_memory_holds_term() {
    let (root, context) = prompt_fixture("prompt text boundary with memory");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.proactive", json!("SH【积极性】"));
    assert_eq!(status, 200, "{body}");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"activeTopics":[{"text":"DSH"}]}}}"#).unwrap();
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["\n\n"]}));

    let render = next_render(&context);
    let delivered = render;
    assert!(delivered.contains("- DSH（"), "the memory topic must reach the model");
    assert!(delivered.contains("【别像客服】D\n\nSH【积极性】"), "the strip must preserve the boundary between the two rules");
}

#[test]
fn prompt_text_sticker_rules_keep_their_validated_text_under_chat_redaction() {
    let (root, context) = prompt_fixture("prompt text sticker strip denied");
    let (status, body) = set_text(&context, "prompt.stickerStrategy", json!("【表情包策略】D鲸SH 140"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.stickerSoften", json!("【用表情缓和语气】D鲸SH 140"));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["鲸"]}));
    let text = next_render(&context);
    assert!(!text.contains("DSH") && text.contains("【表情包策略】D鲸SH 140"), "{text}");
    // The softening rule is printed only with an inventory, so it is checked directly.
    let config = context.load_config_document().unwrap();
    assert_eq!(
        crate::prompt_text::model_text(&context, &config, crate::prompt_text::STICKER_SOFTEN_KEY),
        "【用表情缓和语气】D鲸SH 140"
    );
    assert_eq!(
        crate::prompt_text::model_text(&context, &config, crate::prompt_text::STICKER_STRATEGY_KEY),
        "【表情包策略】D鲸SH 140"
    );
}

#[test]
fn prompt_text_works_on_a_store_written_before_it() {
    // A store as #146 wrote it: schema version 1 with integer rows only. No migration is needed.
    let (root, context) = prompt_fixture("prompt text old store");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert!(!next_render(&context).contains("【别像客服】"), "the compiled defaults render");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】新版 140"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert!(next_render(&context).contains("【别像客服】新版 140\n\n"));
    assert_eq!(next_use_quiet(&context), Some(4500), "the integer setting is untouched");
    let version: i64 = StoreProbe::at(&root.0).user_version();
    assert_eq!(version, 1, "the schema version is unchanged");
}

#[test]
fn prompt_text_settings_page_edits_text_in_a_text_area() {
    let (root, context) = settings_fixture("prompt text page");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    let entry = |key: &str| body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == key).cloned();
    let mode = entry("prompt.mode.chat").unwrap_or_else(|| panic!("prompt.mode.chat is listed: {body}"));
    assert_eq!(mode["kind"], json!({"type": "text", "maxChars": 8000}));
    assert_eq!((mode["default"].clone(), mode["stored"].clone(), mode["value"].clone()), (json!(""), Value::Null, json!("")));
    assert_eq!(mode["group"], "prompt text");
    let rule = entry("prompt.notAClerk").unwrap_or_else(|| panic!("prompt.notAClerk is listed: {body}"));
    assert_eq!(rule["kind"], json!({"type": "text", "maxChars": 2000}));
    assert_eq!(rule["default"], "", "{rule}");
    assert!(!settings_db(&root).exists(), "listing prompt text creates no store");
}

#[test]
fn prompt_text_is_never_writable_from_chat() {
    // The model must not rewrite its own prompt: no text setting is marked `chat`, so the chat settings tool refuses
    // every prompt pointer.
    let (root, context) = settings_fixture("prompt text chat");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"private:7001":{"agentToken":"owner-token-140"}}}"#)
        .unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    for pointer in ["prompts.behaviour.notAClerk", "prompts.mode.chat", "prompts.mode.closedAgent", "prompts", "prompts.mode"] {
        let reply = crate::mcp::call_prepared("settings_set",
            &json!({"key": "private:7001", "token": "owner-token-140", "settingKey": pointer, "value": "x"}), &context).to_string();
        assert!(reply.contains("该项不可通过对话修改"), "{pointer}: {reply}");
    }
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing was written");
}

#[test]
fn prompt_text_reset_stores_the_default_and_undo_restores_only_accepted_text() {
    // #140 with #147: a reset stores a text setting's compiled default over the file's value (a mode prompt's empty
    // default included), and an undo restores an old text only while the registry still accepts it.
    let (root, context) = prompt_fixture("prompt text reset undo");
    write_config(&root, |config| {
        config["prompts"] = json!({
            "behaviour": {"notAClerk": "【别像客服】文件里的版本 140"},
            "mode": {"chat": "文件里的模式说明 140"},
        })
    });
    let edited = "【别像客服】改过的 140";
    let (status, body) = set_text(&context, "prompt.notAClerk", json!(edited));
    assert_eq!(status, 200, "{body}");
    // The file holds prompt text, so the first write seeded the store with it (revision 1, one migration row each);
    // the save is revision 2 and the reset below revision 3.
    let seeded: Vec<Value> = audit_rows(&root).into_iter().filter(|row| row["source"] == "migration").collect();
    for (key, text) in [("prompt.notAClerk", "【别像客服】文件里的版本 140"), ("prompt.mode.chat", "文件里的模式说明 140")] {
        assert!(
            seeded.iter().any(|row| row["key"] == key && row["new"] == json!(json!(text).to_string())),
            "{key} was not seeded from the file: {seeded:?}"
        );
    }
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.notAClerk"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"ok": true, "setting": "prompt.notAClerk", "value": crate::prompt_text::NOT_A_CLERK, "revision": 3, "changed": true})
    );
    assert!(!next_render(&context).contains("【别像客服】"), "the default wins over the file's value");
    assert_eq!(setting_view(&context, "prompt.notAClerk")["stored"], crate::prompt_text::NOT_A_CLERK);
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.notAClerk"}));
    assert_eq!((status, body["changed"].clone()), (200, json!(false)), "a stored default is not a change");
    // Undoing the reset restores the saved text through the registry's check.
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stored"], edited, "{body}");
    assert!(next_render(&context).contains(&format!("{edited}\n\n【在群里怎么说话】")), "the next render reads the restored text");
    // An entry whose old value the registry now refuses (a term a later release denies) is not undone.
    let entry = StoreProbe::at(&root.0).insert_audit(&[AuditRow::test(
        "prompt.notAClerk",
        Some(json!("【别像客服】我是 AI 140").to_string()),
        Some(json!(edited).to_string()),
        "older-release",
    )]);
    let rows = audit_rows(&root).len();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "the old value of this entry is no longer accepted");
    assert!(next_render(&context).contains(&format!("{edited}\n\n【在群里怎么说话】")), "a refused undo keeps the text");
    assert_eq!(audit_rows(&root).len(), rows, "a refused undo writes no audit row");
    // A mode prompt's default is empty: a reset stores the empty text over the file's, and no mode prompt is sent.
    let mode = |context: &RuntimeContext| {
        crate::prompt_text::mode_prompt(&context.load_config_document().unwrap(), crate::prompt_text::MODE_CHAT_KEY)
    };
    assert_eq!(mode(&context), "文件里的模式说明 140", "the file's mode prompt applies before any change");
    let (status, body) = set_text(&context, "prompt.mode.chat", json!("群聊模式说明 140"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.mode.chat"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": "prompt.mode.chat", "value": "", "revision": 3, "changed": true}));
    let view = setting_view(&context, "prompt.mode.chat");
    assert_eq!((view["stored"].clone(), view["value"].clone()), (json!(""), json!("")));
    assert_eq!(mode(&context), "", "the stored empty default wins over the file's mode prompt");
}
