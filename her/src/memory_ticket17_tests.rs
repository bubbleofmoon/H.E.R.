use super::*;

#[test]
fn fts_dot_http_and_mcp_search_surface_safe_errors_and_preserve_valid_results() {
    let (_root, context) = fixture_runtime("fts dot callers", "fixture-console-token-17");
    context.write_atomic(RuntimeArea::Data, "social-v2.json",
        br#"{"conversations":{"group:101":{"agentToken":"group-token"}}}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
        owner_type: "session", owner_id: "group:101", scope: "group", kind: "fact",
        subject: None, content: "deepseek-v4.1-flash v0.2.1", tags: None,
        source: "fixture", importance: 0.5,
    }).unwrap();
    let args = json!({"key":"group:101","token":"group-token","query":"deepseek-v4.1-flash"});
    let valid = crate::mcp::call_tool_with_context("memory_search", &args, &context).unwrap();
    assert_eq!(valid, "1. [#1 fact] deepseek-v4.1-flash v0.2.1");
    let block = crate::prompt::recall_block_with_runtime(&context, &context.load_config().unwrap(),
        &json!({"unread":[{"text":"deepseek-v4.1-flash v0.2.1"}]}), "group:101");
    assert!(block.contains("deepseek-v4.1-flash v0.2.1"), "dotted recall must inject: {block}");
    conn.execute_batch("CREATE TRIGGER fail_hits BEFORE UPDATE OF hits ON memory BEGIN SELECT RAISE(FAIL, 'private-query-token-path'); END;").unwrap();
    drop(conn);
    let error = crate::mcp::call_tool_with_context("memory_search", &args, &context).unwrap_err();
    assert!(error.contains("memory search failed"), "{error}");
    assert!(!error.contains("private-query-token-path"), "{error}");
    let (port, server) = start_http_fixture(context, 1);
    let (status, body) = http_json_with_agent(port, "POST", "/api/memory/search",
        "fixture-console-token-17", Some("group-token"), Some(&args.to_string()));
    assert_eq!(status, 500, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("memory search failed"), "{error}");
    assert!(!error.contains("private-query-token-path"), "{error}");
    server.join().unwrap();
}

#[test]
fn ticket17_settings_all_frozen_defaults_and_omissions() {
    let (_root, context) = fixture_runtime("ticket17 all defaults", "fixture-console-token-17");
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            br#"{"conversations":{"private:7001":{"agentToken":"owner-token"}}}"#,
        )
        .unwrap();
    let mut cfg = context.load_config().unwrap();
    for key in ["socialV2", "memory", "role", "sendDelayMs"] {
        cfg.as_object_mut().unwrap().remove(key);
    }
    let defaults = json!({
        "sendDelayMs":300,
        "socialV2.send.burstEnabled":true,
        "socialV2.send.burstIntervalMinMs":1000,
        "socialV2.send.burstIntervalMaxMs":3000,
        "socialV2.send.maxSendPerMinute":8,
        "socialV2.send.maxSendPerHour":60,
        "socialV2.wake.batchWindowMs":8000,
        "socialV2.sticker.collect.maxBytes":8388608,
        "socialV2.context.wakeMaxChars":6000,
        "socialV2.context.wakeRecentLimit":4,
        "memory.inject.relatedTopK":3,
        "role.maxInjectChars":6000
    });
    let bytes = serde_json::to_vec(&cfg).unwrap();
    context
        .write_atomic(RuntimeArea::Config, "config.json", &bytes)
        .unwrap();
    let args = json!({"key":"private:7001","token":"owner-token"});
    let all = memory_tool_action(&context, "settings-get", &args).unwrap();
    assert_eq!(
        all["settings"], defaults,
        "all-settings must include every JS default, omitting JS undefined paths"
    );
    for path in SETTINGS_WRITABLE {
        let mut named = args.clone();
        named["settingKey"] = json!(path);
        let result = memory_tool_action(&context, "settings-get", &named).unwrap();
        assert_eq!(result["value"], defaults[path], "{path}");
        if defaults.get(path).is_none() {
            assert!(
                result.get("value").is_none(),
                "JS omits undefined named values: {path}: {result}"
            );
        }
        let text = crate::mcp::call_tool_with_context("settings_get", &named, &context).unwrap();
        let expected = defaults
            .get(path)
            .map(Value::to_string)
            .unwrap_or_else(|| "undefined".into());
        assert_eq!(text, format!("{path} = {expected}"));
    }
    assert_eq!(std::fs::read(context.config_file()).unwrap(), bytes);
    // JS shallow-spreads send/wake/context; sticker.collect and role merge their defaults.
    cfg["socialV2"] = json!({"send":{"burstEnabled":false,"maxSendPerMinute":0},"wake":{},"context":{},"sticker":{"collect":{"maxBytes":null},"stickerEnabled":false}});
    cfg["role"] = json!({"maxInjectChars":null});
    cfg["memory"] = json!({"autoFlush":{"enabled":false}});
    context
        .write_atomic(
            RuntimeArea::Config,
            "config.json",
            &serde_json::to_vec(&cfg).unwrap(),
        )
        .unwrap();
    let all = memory_tool_action(&context, "settings-get", &args).unwrap();
    assert_eq!(
        all["settings"],
        json!({"sendDelayMs":300,"socialV2.send.burstEnabled":false,"socialV2.send.maxSendPerMinute":0,"socialV2.sticker.collect.maxBytes":null,"socialV2.sticker.stickerEnabled":false,"memory.inject.relatedTopK":3,"memory.autoFlush.enabled":false,"role.maxInjectChars":null}),
        "preserve explicit values and JS shallow replacement"
    );
}

#[test]
fn ticket17_settings_get_reports_effective_send_delay_before_any_write() {
    let (_root, context) = fixture_runtime("ticket17 settings default", "fixture-console-token-17");
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            br#"{"conversations":{"private:7001":{"agentToken":"owner-token"}}}"#,
        )
        .unwrap();
    let mut config = context.load_config().unwrap();
    let args = json!({"key":"private:7001","token":"owner-token","settingKey":"sendDelayMs"});
    let all_args = json!({"key":"private:7001","token":"owner-token"});
    // Frozen JS loadConfig uses file.sendDelayMs ?? 300, including explicit null.
    for (stored, expected) in [
        (None, 300),
        (Some(Value::Null), 300),
        (Some(json!(0)), 0),
        (Some(json!(71)), 71),
    ] {
        config.as_object_mut().unwrap().remove("sendDelayMs");
        if let Some(value) = stored {
            config["sendDelayMs"] = value;
        }
        let bytes = serde_json::to_vec(&config).unwrap();
        context
            .write_atomic(RuntimeArea::Config, "config.json", &bytes)
            .unwrap();
        let result = crate::mcp::call_tool_with_context("settings_get", &args, &context).unwrap();
        assert_eq!(result, format!("sendDelayMs = {expected}"));
        let all = crate::mcp::call_tool_with_context("settings_get", &all_args, &context).unwrap();
        assert!(all.lines().any(|line| line == result), "{all}");
        assert_eq!(
            std::fs::read(context.config_file()).unwrap(),
            bytes,
            "reading effective settings must not rewrite config"
        );
    }
    let set_args = json!({"key":"private:7001","token":"owner-token","settingKey":"sendDelayMs","value":17});
    assert_eq!(
        crate::mcp::call_tool_with_context("settings_set", &set_args, &context).unwrap(),
        "已改 sendDelayMs = 17。已立即生效"
    );
    assert_eq!(
        crate::mcp::call_tool_with_context("settings_get", &args, &context).unwrap(),
        "sendDelayMs = 17"
    );
    assert_eq!(context.load_config().unwrap()["sendDelayMs"], 17);
}

#[test]
fn ticket17_frozen_js_history_retrieval_and_acl_oracle_through_http() {
    let oracle: Value =
        serde_json::from_str(include_str!("../tests/fixtures/memory-acl-oracle.json")).unwrap();
    let (_root, context) = fixture_runtime("ticket17 oracle", "fixture-console-token-17");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"agentToken":"group-token"},"private:202":{"agentToken":"private-token"},"private:7001":{"agentToken":"owner-token"}}}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    conn.execute_batch(oracle["sql"].as_str().unwrap()).unwrap();
    drop(conn);
    for segment in oracle["segments"].as_array().unwrap() {
        assert_eq!(
            json!(crate::memory::segment(segment["text"].as_str().unwrap())),
            segment["tokens"]
        );
    }
    let checks = oracle["checks"].as_array().unwrap();
    let (port, server) = start_http_fixture(context.clone(), checks.len() * 2);
    let mut grant: Option<Value> = None;
    for expected in checks {
        if let Some(next) = expected.get("grant") {
            let mut args = next.clone();
            args["key"] = json!("private:7001");
            args["token"] = json!("owner-token");
            args["action"] = json!("grant");
            crate::mcp::call_tool_with_context("memory_acl", &args, &context).unwrap();
            grant = Some(args);
        }
        if expected["label"] == "revoked" {
            let mut args = grant.take().unwrap();
            args["action"] = json!("revoke");
            crate::mcp::call_tool_with_context("memory_acl", &args, &context).unwrap();
        }
        let key = expected["readerKey"].as_str().unwrap();
        let token = match key {
            "group:101" => "group-token",
            "private:202" => "private-token",
            _ => "owner-token",
        };
        let kinds = expected["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let body =
            json!({"key":key,"query":expected["query"],"kinds":kinds,"limit":50}).to_string();
        let (status, actual) = http_json_with_agent(
            port,
            "POST",
            "/api/memory/search",
            "fixture-console-token-17",
            Some(token),
            Some(&body),
        );
        assert_eq!(status, 200, "{actual}");
        let projection: Vec<Value> = actual["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let mut result = serde_json::Map::new();
                for field in [
                    "id", "owner_id", "scope", "kind", "subject", "content", "hits",
                ] {
                    result.insert(field.to_string(), m[field].clone());
                }
                Value::Object(result)
            })
            .collect();
        assert_eq!(
            json!(projection),
            expected["memories"],
            "{}",
            expected["label"]
        );
        let (status, index) = http_json_with_agent(
            port,
            "POST",
            "/api/memory/index",
            "fixture-console-token-17",
            Some(token),
            Some(&json!({"key":key}).to_string()),
        );
        assert_eq!(status, 200, "{index}");
        assert_eq!(index["total"], expected["index"]["total"]);
        assert_eq!(index["terms"], expected["index"]["terms"]);
    }
    server.join().unwrap();
}

#[test]
fn ticket17_mcp_cannot_self_grant_or_delete_foreign_memory() {
    let (_root, context) = fixture_runtime("ticket17 permissions", "fixture-console-token-17");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"agentToken":"group-token"},"private:7001":{"agentToken":"owner-token"}}}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    conn.execute("INSERT INTO memory(owner_type,owner_id,scope,kind,content,source,created_at) VALUES('session','private:202','private','fact','foreign-secret','fixture',1700000000000)", []).unwrap();
    let id = conn.last_insert_rowid();
    drop(conn);
    let grant = crate::mcp::call_tool_with_context(
        "memory_acl",
        &json!({"key":"group:101","token":"group-token","action":"grant","readType":"*","readId":"*"}),
        &context,
    );
    assert!(
        grant.is_err(),
        "a group cannot grant itself access: {grant:?}"
    );
    let forget = crate::mcp::call_tool_with_context(
        "memory_forget",
        &json!({"key":"group:101","token":"group-token","id":id}),
        &context,
    );
    assert!(
        forget.is_err(),
        "a group cannot delete another scope: {forget:?}"
    );
    let settings = crate::mcp::call_tool_with_context(
        "settings_get",
        &json!({"key":"group:101","token":"group-token","settingKey":"consoleToken"}),
        &context,
    );
    assert!(
        settings.is_err(),
        "only the owner can read settings: {settings:?}"
    );
    assert_eq!(fixture_memory_contents(&context), vec!["foreign-secret"]);
}

#[test]
fn ticket17_new_database_and_failed_index_write_are_atomic() {
    let (_root, context) = fixture_runtime("ticket17 new database", "fixture-console-token-17");
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            br#"{"conversations":{"group:101":{"agentToken":"group-token"}}}"#,
        )
        .unwrap();
    let args = json!({"key":"group:101","token":"group-token","content":"first-memory"});
    crate::mcp::call_tool_with_context("memory_write", &args, &context).unwrap();
    assert!(
        crate::mcp::call_tool_with_context("memory_index", &args, &context)
            .unwrap()
            .contains("共 1 条")
    );
    let conn = crate::memory::open_for_runtime(&context).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_index BEFORE INSERT ON memory_fts_content BEGIN SELECT RAISE(ABORT,'injected index failure'); END;").unwrap();
    let mut fail = args.clone();
    fail["content"] = json!("must-rollback");
    assert!(crate::mcp::call_tool_with_context("memory_write", &fail, &context).is_err());
    assert_eq!(fixture_memory_contents(&context), vec!["first-memory"]);
    conn.execute_batch("DROP TRIGGER reject_index;").unwrap();
    assert!(crate::mcp::call_tool_with_context(
        "memory_search",
        &json!({"key":"group:101","token":"group-token","query":"first"}),
        &context
    )
    .unwrap()
    .contains("first-memory"));
    let archive = memory_tool_action(
        &context,
        "archive",
        &json!({"key":"group:101","token":"group-token","id":1,"archived":true}),
    );
    assert!(archive.is_err(), "a group cannot archive memory");
}

#[test]
fn ticket17_http_and_mcp_share_history_acl_settings_archive_and_revocation() {
    let (_root, context) = fixture_runtime("ticket17 loop", "fixture-console-token-17");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"agentToken":"group-token"},"private:7001":{"agentToken":"owner-token"},"private:202":{"agentToken":"private-token"}}}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    conn.execute_batch("INSERT INTO memory(owner_type,owner_id,scope,kind,subject,content,source,created_at,archived) VALUES('session','group:303','group','fact','history','shared-history','import',1700000000000,0),('session','private:202','private','fact','secret','private-secret','import',1700000000000,0),('person','person:202','group','preference','person','person-history','import',1700000000000,0),('session','group:101','group','fact','archive','archived-history','import',1700000000000,1); INSERT INTO team_member VALUES('readers','session','group:101'),('writers','session','group:303');").unwrap();
    drop(conn);
    let owner = json!({"key":"private:7001","token":"owner-token"});
    let group = json!({"key":"group:101","token":"group-token"});
    let call = |name: &str, base: &Value, extra: Value| {
        let mut args = base.clone();
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        crate::mcp::call_tool_with_context(name, &args, &context)
    };
    let (port, server) = start_http_fixture(context.clone(), 6);
    let before = call("memory_search", &group, json!({"query":""})).unwrap();
    assert!(!before.contains("shared-history"));
    let grant = json!({"action":"grant","readerType":"team","readerId":"readers","readType":"team","readId":"writers"}).to_string();
    assert_eq!(
        http_json(
            port,
            "POST",
            "/api/memory/acl",
            "fixture-console-token-17",
            Some(&grant)
        )
        .0,
        200
    );
    assert!(call("memory_search", &group, json!({"query":""}))
        .unwrap()
        .contains("shared-history"));
    let wildcard = call("memory_acl",&owner,json!({"action":"grant","readerType":"session","readerId":"group:101","readType":"*","readId":"*"})).unwrap();
    assert!(wildcard.contains("已授权"));
    let all = call("memory_search", &group, json!({"query":""})).unwrap();
    assert!(all.contains("person-history"));
    assert!(!all.contains("private-secret"));
    assert!(!all.contains("archived-history"));
    assert!(call("memory_search", &owner, json!({"query":""}))
        .unwrap()
        .contains("private-secret"));
    let write = call(
        "memory_write",
        &group,
        json!({"content":"管理员偏好 Alpha_1","subject":"偏好"}),
    )
    .unwrap();
    assert!(write.contains("已记住"));
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["unread"] = json!([{"text":"管理员偏好 Alpha_1"}]);
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            &serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    let preview = http_json_with_agent(
        port,
        "POST",
        "/api/memory/preview",
        "fixture-console-token-17",
        Some("group-token"),
        Some(&json!({"key":"group:101"}).to_string()),
    );
    assert_eq!(preview.0, 200, "{}", preview.1);
    assert!(preview.1["block"]
        .as_str()
        .unwrap()
        .contains("管理员偏好 Alpha_1"));
    assert!(!preview.1["block"]
        .as_str()
        .unwrap()
        .contains("private-secret"));
    let (status, search) = http_json_with_agent(
        port,
        "POST",
        "/api/memory/search",
        "fixture-console-token-17",
        Some("group-token"),
        Some(&json!({"key":"group:101","query":"管理员偏好 Alpha_1"}).to_string()),
    );
    assert_eq!(status, 200, "{search}");
    assert_eq!(search["memories"].as_array().unwrap().len(), 1);
    let id = search["memories"][0]["id"].as_i64().unwrap();
    let archived = http_json(
        port,
        "POST",
        "/api/memory/archive",
        "fixture-console-token-17",
        Some(&json!({"id":id,"archived":true}).to_string()),
    );
    assert_eq!(archived.0, 200, "{}", archived.1);
    assert!(!call("memory_search", &group, json!({"query":""}))
        .unwrap()
        .contains("管理员偏好"));
    let restored = http_json(
        port,
        "POST",
        "/api/memory/archive",
        "fixture-console-token-17",
        Some(&json!({"id":id,"archived":false}).to_string()),
    );
    assert_eq!(restored.0, 200, "{}", restored.1);
    call("memory_forget", &group, json!({"id":id})).unwrap();
    call(
        "settings_set",
        &owner,
        json!({"settingKey":"sendDelayMs","value":1717}),
    )
    .unwrap();
    let settings = http_json_with_agent(
        port,
        "POST",
        "/api/memory/settings-get",
        "fixture-console-token-17",
        Some("owner-token"),
        Some(&json!({"key":"private:7001","settingKey":"sendDelayMs"}).to_string()),
    );
    assert_eq!(settings.1["value"], 1717);
    for (rt, rid, wt, wid) in [
        ("team", "readers", "team", "writers"),
        ("session", "group:101", "*", "*"),
    ] {
        call(
            "memory_acl",
            &owner,
            json!({"action":"revoke","readerType":rt,"readerId":rid,"readType":wt,"readId":wid}),
        )
        .unwrap();
    }
    assert!(!call("memory_search", &group, json!({"query":""}))
        .unwrap()
        .contains("shared-history"));
    // Reopen the store through another call: revocation persists and private ownership remains.
    let private = json!({"key":"private:202","token":"private-token"});
    let own = call("memory_search", &private, json!({"query":""})).unwrap();
    assert!(own.contains("private-secret") && own.contains("person-history"));
    server.join().unwrap();
}

#[test]
fn ticket17_concurrent_writes_deduplicate_and_waiting_token_is_rechecked() {
    let (_root, context) = fixture_runtime("ticket17 concurrency", "fixture-console-token-17");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"agentToken":"group-token"},"group:202":{"agentToken":"other-token"}}}"#).unwrap();
    drop(initialize_fixture_memory_database(&context));
    let workers: Vec<_> = (0..8).map(|i| {
        let ctx = context.clone();
        std::thread::spawn(move || crate::mcp::call_tool_with_context("memory_write", &json!({"key":if i%2==0 {"group:101"} else {"group:202"},"token":if i%2==0 {"group-token"} else {"other-token"},"content":"same-content"}), &ctx).unwrap())
    }).collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        fixture_memory_contents(&context).len(),
        2,
        "one row per owner, with atomic deduplication"
    );
    let held = acquire_social_state_gate(&context).unwrap();
    let ctx = context.clone();
    let worker = std::thread::spawn(move || {
        crate::mcp::call_tool_with_context(
            "memory_write",
            &json!({"key":"group:101","token":"group-token","content":"revoked-write"}),
            &ctx,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"group:101":{"agentToken":"rotated"},"group:202":{"agentToken":"other-token"}}}"#).unwrap();
    drop(held);
    assert!(worker.join().unwrap().is_err());
    assert_eq!(
        fixture_memory_contents(&context),
        vec!["same-content", "same-content"]
    );
}

const RECALL_KEY: &str = "group:101";

/// A DSH-ready conversation whose unread message matches two stored memories, `relatedTopK` 1.
fn recall_wake_fixture(label: &str) -> (FixtureRoot, RuntimeContext, Value, rusqlite::Connection, Value) {
    let (root, context) = fixture_runtime(label, "fixture-console-token-17");
    let key = RECALL_KEY;
    let state = json!({"conversations":{key:{
        "agentToken":"epoch-148","bootstrapSent":true,"lastUnreadSeq":1,"lastReadThroughSeq":0,"lastIncomingAt":1_000,
        "unread":[{"seq":1,"sender":"甲","userId":"1001","text":"deepseek-v4.1-flash v0.2.1","time":1_000,"isSelf":false}],
        "recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}}}});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    crate::session_map::bind(&context, key, "sid-148", None).unwrap();
    let mut allowed = context.load_config().unwrap();
    allowed["allow"]["groups"] = json!([101]);
    context.write_atomic(RuntimeArea::Config, "config.json", allowed.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    for (subject, content) in [("评测", "deepseek-v4.1-flash v0.2.1 评测记录一"), ("别的", "deepseek-v4.1-flash v0.2.1 评测记录二")] {
        crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
            owner_type: "session", owner_id: key, scope: "group", kind: "fact",
            subject: Some(subject), content, tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
    }
    let mut cfg = context.load_config().unwrap();
    cfg["memory"] = json!({"enabled":true,"inject":{"relatedTopK":1}});
    (root, context, state, conn, cfg)
}

fn recall_hits(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT SUM(hits) FROM memory", [], |row| row.get(0)).unwrap()
}

#[test]
fn a_wake_whose_snapshot_cannot_be_fetched_counts_no_recall_hit() {
    let (_root, context, _state, conn, cfg) = recall_wake_fixture("dsh recall snapshot failure");
    // No console port is known, so the wake's message packet cannot be fetched: nothing is delivered.
    let error = crate::prompt::prepare_dsh_wake(&context, &cfg, RECALL_KEY, "sid-148", "epoch-148", "atMention", "rpc-148", "").unwrap_err();
    assert!(error.contains("snapshot"), "{error}");
    assert_eq!(recall_hits(&conn), 0, "a wake that was never delivered used no memory");
}

#[test]
fn the_dsh_wake_delivers_the_recall_the_preview_shows_and_counts_a_hit_only_when_delivered() {
    let (_root, context, state, conn, cfg) = recall_wake_fixture("dsh recall delivery");
    let key = RECALL_KEY;
    let st = &state["conversations"][key];
    let preview = crate::prompt::recall_block_with_runtime(&context, &cfg, st, key);
    assert!(preview.contains("评测记录"), "{preview}");
    assert_eq!(preview.matches("\n- [fact").count(), 1, "relatedTopK is honoured: {preview}");
    // Rendering (the preview and the compatibility render that precedes a delivery) counts nothing.
    let _ = crate::prompt::build_with_runtime(&context, key, "atMention", "bot", "9001");
    assert_eq!(recall_hits(&conn), 0, "reading recall for preview or for a later delivery is not a use");

    // The wake's message packet is fetched from the console route; serve that one request.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handle_conn(stream, served_cfg, served);
    });
    let (delivered, recalled) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-148", "").unwrap();
    server.join().unwrap();
    assert!(delivered.contains(preview.trim_end()), "the wake carries exactly the previewed recall:\n{delivered}");
    assert_eq!(recalled.len(), 1, "the ids of the one memory sent are returned");
    assert_eq!(recall_hits(&conn), 0, "preparing the wake counts nothing: DSH has not accepted it");
    crate::prompt::record_recall_hits(&context, &recalled);
    assert_eq!(recall_hits(&conn), 1, "delivery counts one hit for the one memory sent");
}

#[test]
fn a_session_generation_never_receives_the_same_recalled_memory_twice() {
    let (_root, context, _state, conn, cfg) = recall_wake_fixture("dsh recall no repeat");
    let key = RECALL_KEY;
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    // One message packet is fetched per wake.
    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, served_cfg.clone(), served.clone());
        }
    });
    // DSH accepting a wake installs its briefing record (what `session_context::observe` does).
    let accept = |rpc: &str, seq: i64| {
        crate::session_context::observe(&context, key, "sid-148", "epoch-148", &json!({
            "type": "user/message", "seq": seq, "data": {"source": {"rpcId": rpc}},
        })).unwrap();
    };
    let wake = |rpc: &str| {
        let (text, recalled) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", rpc, "").unwrap();
        (text, recalled)
    };
    let (_, unaccepted) = wake("rpc-1");
    assert_eq!(unaccepted.len(), 1, "relatedTopK is 1");
    // A wake DSH never accepted holds nothing back: the next wake carries the same memory.
    let (first_text, first) = wake("rpc-1b");
    assert_eq!(first, unaccepted, "an unaccepted recall is carried again");
    accept("rpc-1b", 1);
    let (second_text, second) = wake("rpc-2");
    assert_eq!(second.len(), 1, "the other matching memory is next");
    assert_ne!(first, second, "an installed recall is not repeated in the generation");
    let which = |text: &str| ["评测记录一", "评测记录二"].iter().position(|mark| text.contains(mark));
    assert!(which(&first_text).is_some() && which(&second_text).is_some() && which(&first_text) != which(&second_text),
        "each wake renders a different memory:\n{first_text}\n{second_text}");
    accept("rpc-2", 2);
    let (_, third) = wake("rpc-3");
    assert!(third.is_empty(), "both memories were delivered in this generation: {third:?}");
    server.join().unwrap();
    assert_eq!(recall_hits(&conn), 0, "preparing wakes counts nothing");

    // Compaction starts a new generation: the briefing is full again and the memories return.
    crate::session_map::update_context(&context, key, "sid-148", "epoch-148", |r| {
        r["generation"] = json!(r["generation"].as_u64().unwrap_or(0) + 1);
        r["pending"] = Value::Null;
    }).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handle_conn(stream, served_cfg, served);
    });
    let (_, fresh) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-5", "").unwrap();
    server.join().unwrap();
    assert_eq!(fresh.len(), 1, "a new generation recalls again");
}

/// Serve `n` wake message packets from the console route; returns the context that knows the port.
fn serve_wake_packets(context: &RuntimeContext, n: usize) -> (RuntimeContext, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.clone().with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..n {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, served_cfg.clone(), served.clone());
        }
    });
    (context, server)
}

fn accept_wake(context: &RuntimeContext, rpc: &str, seq: i64) {
    crate::session_context::observe(context, RECALL_KEY, "sid-148", "epoch-148", &json!({
        "type": "user/message", "seq": seq, "data": {"source": {"rpcId": rpc}},
    })).unwrap();
}

#[test]
fn a_history_gap_refresh_within_a_generation_keeps_the_memories_already_carried() {
    let (_root, context, _state, _conn, cfg) = recall_wake_fixture("dsh recall gap refresh");
    let key = RECALL_KEY;
    let (context, server) = serve_wake_packets(&context, 2);
    let (_, first) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-1", "").unwrap();
    accept_wake(&context, "rpc-1", 1);
    // A gap in the observed history demands a full briefing without starting a new generation.
    crate::session_map::update_context(&context, key, "sid-148", "epoch-148", |r| {
        r["needsBriefing"] = json!(true);
        r["pending"] = Value::Null;
    }).unwrap();
    let (text, second) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-2", "").unwrap();
    server.join().unwrap();
    assert!(text.contains("以下各节是本代会话的当前完整简报"), "the refresh is a full briefing:\n{text}");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first, second, "the same generation does not receive an installed memory again");
}

#[test]
fn the_console_preview_shows_what_the_next_wake_would_deliver_after_an_accepted_wake() {
    let (_root, context, state, _conn, cfg) = recall_wake_fixture("dsh recall preview held");
    let key = RECALL_KEY;
    let st = &state["conversations"][key];
    let which = |text: &str| ["评测记录一", "评测记录二"].iter().position(|mark| text.contains(mark));
    let before = crate::prompt::recall_block_with_runtime(&context, &cfg, st, key);
    assert!(which(&before).is_some(), "{before}");
    let (context, server) = serve_wake_packets(&context, 1);
    let (delivered, _) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-1", "").unwrap();
    server.join().unwrap();
    assert_eq!(which(&delivered), which(&before));
    // Prepared but not accepted: the preview is unchanged.
    assert_eq!(which(&crate::prompt::recall_block_with_runtime(&context, &cfg, st, key)), which(&before));
    accept_wake(&context, "rpc-1", 1);
    let after = crate::prompt::recall_block_with_runtime(&context, &cfg, st, key);
    assert!(which(&after).is_some() && which(&after) != which(&before), "the preview skips the memory the generation holds:\n{after}");
}

#[test]
fn a_message_that_matches_no_memory_renders_no_recalled_memory() {
    let (_root, context, state, _conn, cfg) = recall_wake_fixture("dsh recall unrelated");
    let key = RECALL_KEY;
    let mut st = state["conversations"][key].clone();
    st["unread"][0]["text"] = json!("今天中午吃什么好呢，想不出来");
    let block = crate::prompt::recall_block_with_runtime(&context, &cfg, &st, key);
    assert!(!block.contains("\n- [fact"), "no pinned or recent padding: {block}");
    assert!(!block.contains("【记忆】你可能需要回顾"), "{block}");
}

#[test]
fn related_top_k_accepts_only_the_depth_a_wake_executes_and_reads_show_that_depth() {
    let (_root, context) = fixture_runtime("related top k", "fixture-console-token-17");
    context
        .write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"private:7001":{"agentToken":"owner-token"}}}"#)
        .unwrap();
    let call = |action: &str, extra: Value| {
        let mut args = json!({"key":"private:7001","token":"owner-token","settingKey":"memory.inject.relatedTopK"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        memory_tool_action(&context, action, &args)
    };
    let stored = || context.load_config().unwrap().pointer("/memory/inject/relatedTopK").cloned();
    for bad in [json!(0), json!(21), json!(-3), json!(2.5), json!("many"), json!(null), json!(true), json!(1000)] {
        let error = call("settings-set", json!({"value": bad})).expect_err(&format!("{bad} is outside what a wake executes"));
        assert!(error.contains("1") && error.contains("20"), "the error names the range: {error}");
        assert_eq!(stored(), None, "{bad}: a refused write changes nothing");
    }
    for (given, executed) in [(json!(1), 1), (json!(20), 20), (json!("7"), 7), (json!(" 12 "), 12)] {
        let reply = call("settings-set", json!({"value": given})).unwrap();
        assert_eq!(reply["value"], executed);
        assert_eq!(stored(), Some(json!(executed)), "stored as the number the wake executes");
        assert_eq!(call("settings-get", json!({})).unwrap()["value"], executed);
    }
    // A hand-edited value outside the range reads as the depth the wake actually runs.
    for (edited, executed) in [(json!(50), 20), (json!(0), 1), (json!(-4), 1), (json!("x"), 3), (json!(2.6), 3), (json!(null), 3)] {
        let mut cfg = context.load_config().unwrap();
        cfg["memory"] = json!({"inject":{"relatedTopK": edited.clone()}});
        context.write_atomic(RuntimeArea::Config, "config.json", &serde_json::to_vec(&cfg).unwrap()).unwrap();
        assert_eq!(call("settings-get", json!({})).unwrap()["value"], executed, "stored {edited}");
        assert_eq!(call("settings-get", json!({"settingKey": null})).unwrap()["settings"]["memory.inject.relatedTopK"], executed, "{edited}");
    }
}

#[test]
fn a_persona_switch_while_a_wake_is_prepared_never_installs_the_old_persona() {
    let (_root, context, _state, _conn, cfg) = recall_wake_fixture("dsh persona switch race");
    let key = RECALL_KEY;
    let open: crate::persona::Authority<'static> = &crate::persona::open_authority;
    let mut ids = Vec::new();
    for (name, core) in [("小鲸鱼", "你是一只爱聊天的小鲸鱼。"), ("会计", "你是会计助手，只讲数字。")] {
        let id = crate::persona::create(&context, &json!({"name": name, "core": core}), open).unwrap()["id"].as_str().unwrap().to_string();
        crate::persona::approve(&context, &json!({"id": id, "version": 1}), open).unwrap();
        ids.push(id);
    }
    crate::persona::assign(&context, &json!({"default": ids[0]}), open).unwrap();
    let (context, server) = serve_wake_packets(&context, 2);
    let (switcher, clerk) = (context.clone(), ids[1].clone());
    // The switch lands after the identity section was rendered with the old persona.
    crate::prompt::AFTER_SECTIONS.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        crate::persona::assign(&switcher, &json!({"default": clerk}), &crate::persona::open_authority).unwrap();
    })));
    let error = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-1", "").unwrap_err();
    assert_eq!(error, crate::prompt::WAKE_BRIEFING_CHANGED, "a wake rendered with the old persona is never installed");
    let (text, _) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-148", "epoch-148", "atMention", "rpc-1", "").unwrap();
    server.join().unwrap();
    assert!(text.contains("以下各节是本代会话的当前完整简报"), "the retry is the new generation's full briefing:\n{text}");
    assert!(text.contains("【人设】会计\n你是会计助手，只讲数字。") && !text.contains("【人设】小鲸鱼"), "{text}");
}
