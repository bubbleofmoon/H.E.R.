use super::*;

// Issue #142: no text HER renders for a model names a harness, the model, its platform or developer,
// shows the install layout, or calls her an AI. Every surface this crate can render in-process is
// checked here: the DSH briefing and wake delta, the compatibility, reminder and text-only renders,
// the MCP tool lists with their descriptions and schemas, the generated MCP server names and serve
// arguments, the digest and slang learner prompts, and representative tool errors.

// The #142 denied-term check every model-text check calls (#243); its own tests are in `model_text_policy`.
use crate::model_text_policy::denied_terms;

/// The string literals passed to `error(` in a source file: the harness's tool results, which are
/// private to `harness::tools` and so are checked in the source.
fn error_literals(source: &str) -> String {
    source
        .match_indices("error(")
        .filter_map(|(at, _)| {
            let body = source[at + "error(".len()..].trim_start().strip_prefix('"')?;
            Some(body[..body.find('"')?].to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_secret_rule_checks_every_path_segment() {
    for secret in ["relay.credentials.0", "relay.credentials", "snowluma.accessToken", "relay.auth.bearer_token", "relay.api-keys.1",
        "upstreams.privateKey.x", "security", "security.outboundGuard", "socialV2.send.apiKey", "consoleToken"] {
        assert!(super::super::setting_is_secret(secret), "{secret}");
    }
    for plain in ["socialV2.send.burstEnabled", "socialV2.wake.recommendedKeywords", "sendDelayMs", "memory.inject.relatedTopK", "role.maxInjectChars"] {
        assert!(!super::super::setting_is_secret(plain), "{plain}");
    }
    assert_eq!(super::super::redact_setting("relay.credentials.0", json!("fixture-relay-value-142")), json!("(已隐藏)"));
    assert_eq!(super::super::redact_setting("relay", json!({"credentials": {"0": "fixture-relay-value-142"}, "name": "a"})),
        json!({"credentials": "(已隐藏)", "name": "a"}));
}

#[test]
fn settings_get_reads_only_writable_settings_and_never_a_secret() {
    let (_root, context) = fixture_runtime("settings secrets", "fixture-console-token-142s");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"private:7001":{"agentToken":"owner-token-142"}}}"#).unwrap();
    let mut cfg = context.load_config().unwrap();
    cfg["snowluma"] = json!({"httpUrl": "http://127.0.0.1:3000", "accessToken": "fixture-gateway-value-142"});
    cfg["webSearch"] = json!({"provider": "bing", "apiKey": "fixture-search-value-142"});
    cfg["relay"] = json!({"credentials": {"0": "fixture-relay-value-142"}, "auth": {"bearer_token": "fixture-bearer-value-142"}});
    cfg["security"] = json!({"outboundGuard": true});
    cfg["dsh"]["model"] = json!("fixture-backend-model-142");
    cfg["sendDelayMs"] = json!(321);
    context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
    let args = |setting: &str| json!({"key": "private:7001", "token": "owner-token-142", "settingKey": setting});
    for setting in ["relay.credentials.0", "relay.credentials", "relay", "relay.auth.bearer_token", "snowluma.accessToken", "snowluma",
        "snowluma.httpUrl", "webSearch", "security", "security.outboundGuard", "dsh.model", "dsh", "consoleToken", "ownerQQ",
        "dependencies", "no.such.setting"] {
        assert_eq!(memory_tool_action(&context, "settings-get", &args(setting)), Err(SETTINGS_READ_REFUSED.to_string()), "{setting}");
        let text = crate::mcp::call_prepared("settings_get", &args(setting), &context).to_string();
        assert!(text.contains(SETTINGS_READ_REFUSED) && !text.contains(setting) && !text.contains("fixture-"), "{setting}: {text}");
    }
    // A writable setting still reads, alone and in the full list.
    assert_eq!(memory_tool_action(&context, "settings-get", &args("sendDelayMs")).unwrap()["value"], 321);
    let all = crate::mcp::call_tool_with_context("settings_get", &json!({"key": "private:7001", "token": "owner-token-142"}), &context).unwrap();
    assert!(all.contains("sendDelayMs = 321") && !all.contains("fixture-"), "{all}");
    // A refused write does not name the key either.
    let refused = crate::mcp::call_prepared("settings_set", &json!({"key": "private:7001", "token": "owner-token-142", "settingKey": "dsh.model", "value": "x"}), &context).to_string();
    assert!(refused.contains("禁止在对话中更改此项") && !refused.contains("dsh"), "{refused}");
}

#[test]
fn qq_get_prompt_hides_secrets_in_the_config_sections_it_copies() {
    let (_root, context) = fixture_runtime("prompt secrets", "fixture-console-token-142p");
    let mut cfg = context.load_config().unwrap();
    cfg["socialV2"] = json!({"send": {"burstEnabled": true, "apiKey": "fixture-send-value-142"},
        "wake": {"recommendedKeywords": ["小鲸鱼"], "auth": {"token": "fixture-wake-value-142"}}});
    let prompt = super::super::handle_prompt(&context, &cfg, "group:101");
    assert_eq!(prompt["recommended"]["send"]["burstEnabled"], true);
    assert_eq!(prompt["recommended"]["send"]["apiKey"], "(已隐藏)");
    assert_eq!(prompt["recommended"]["wake"]["recommendedKeywords"], json!(["小鲸鱼"]));
    assert_eq!(prompt["recommended"]["wake"]["auth"]["token"], "(已隐藏)");
    assert!(!prompt.to_string().contains("fixture-send-value-142") && !prompt.to_string().contains("fixture-wake-value-142"), "{prompt}");
}

/// Every name resolves to one public address (the policy refuses private ones).
fn public_resolver(_host: &str) -> Result<Vec<std::net::IpAddr>, String> {
    Ok(vec!["93.184.216.34".parse().unwrap()])
}

/// Every request is refused with HTTP 401 and a body that names the provider.
fn refusing_transfer(
    _transfer: &crate::aux_fetch::Transfer,
    on_head: &mut dyn FnMut(&crate::aux_fetch::Head) -> crate::aux_fetch::Flow,
    on_body: &mut dyn FnMut(&[u8]) -> crate::aux_fetch::Flow,
) -> Result<(crate::aux_fetch::Head, bool), crate::aux_fetch::TransferError> {
    let _ = on_head(&crate::aux_fetch::Head { status: 401, location: None });
    let _ = on_body(br#"{"error":{"message":"DeepSeek API key invalid"}}"#);
    Ok((crate::aux_fetch::Head { status: 401, location: None }, true))
}

/// The web search errors the model reads when the primary search answers 401: with the primary
/// provider selected, then in auto mode (Bing is refused too).
fn search_errors(context: &RuntimeContext) -> (String, String) {
    let net = crate::aux_search::Network { resolver: &public_resolver, transfer: &refusing_transfer };
    let config = |provider: &str| json!({"webSearch": {"provider": provider, "apiKey": "fixture-search-value-142", "baseURL": "https://search.example.test/v1"}});
    let primary = crate::aux_search::web_search("weather 142 primary", &config("deepseek"), context, &net).unwrap_err();
    let auto = crate::aux_search::web_search("weather 142 auto", &config("auto"), context, &net).unwrap_err();
    (primary, auto)
}

#[test]
fn web_search_errors_never_copy_the_provider_text() {
    let (_root, context) = fixture_runtime("search errors", "fixture-console-token-142w");
    let (primary, auto) = search_errors(&context);
    assert_eq!(primary, "主搜索接口 HTTP 401");
    assert_eq!(auto, "主搜索不可用：主搜索接口 HTTP 401；Bing 兜底也失败", "the Bing failure detail must not reach the model");
    for text in [&primary, &auto] {
        assert!(denied_terms(text).is_empty() && !text.contains("API key invalid"), "{text}");
    }
}

#[test]
fn no_model_visible_text_names_a_harness_the_model_its_maker_the_install_layout_or_ai() {
    // The error-literal reader below takes every string argument of `error(`, across line breaks.
    assert_eq!(error_literals("return error(\"a\");\nErr(error(\n    \"b\",\n))\nfn error(reason: &str)"), "a\nb");
    let (root, context) = fixture_runtime("model text guard", "fixture-console-token-142");
    let key = "group:101";
    context.write_atomic(RuntimeArea::Data, "social-v2.json", json!({"conversations":{
        key:{"agentToken":"epoch-142","bootstrapSent":true,"lastUnreadSeq":1,"lastReadThroughSeq":0,"lastIncomingAt":1_000,
            "unread":[{"seq":1,"sender":"甲","userId":"1001","text":"今天天气不错","time":1_000,"isSelf":false}],
            "recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}},
        "private:7001":{"agentToken":"owner-token-142"}}}).to_string().as_bytes()).unwrap();
    crate::session_map::bind(&context, key, "sid-142", None).unwrap();
    let mut allowed = context.load_config().unwrap();
    allowed["allow"]["groups"] = json!([101]);
    context.write_atomic(RuntimeArea::Config, "config.json", allowed.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    drop(initialize_fixture_memory_database(&context));
    let open: crate::persona::Authority<'static> = &crate::persona::open_authority;
    let id = crate::persona::create(&context, &json!({"name": "小鲸鱼", "core": "你是一只住在群里的小鲸鱼，爱聊天。"}), open)
        .unwrap()["id"].as_str().unwrap().to_string();
    crate::persona::approve(&context, &json!({"id": id, "version": 1}), open).unwrap();
    crate::persona::assign(&context, &json!({"default": id}), open).unwrap();
    let cfg = context.load_config().unwrap();
    let mut surfaces: Vec<(String, String)> = Vec::new();

    // The live DSH route: the full briefing, then (after DSH accepted it) a wake delta. The message
    // packet of each wake comes from the console route, served here twice.
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
    let (briefing, _) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-142", "epoch-142", "atMention", "rpc-142-1", "").unwrap();
    crate::session_context::observe(&context, key, "sid-142", "epoch-142", &json!({
        "type": "user/message", "seq": 1, "data": {"source": {"rpcId": "rpc-142-1"}},
    })).unwrap();
    let (delta, _) = crate::prompt::prepare_dsh_wake(&context, &cfg, key, "sid-142", "epoch-142", "atMention", "rpc-142-2", "").unwrap();
    server.join().unwrap();
    assert!(briefing.contains("以下各节是本代会话的当前完整简报") && briefing.contains("【人设】小鲸鱼"), "{briefing}");
    assert!(delta.contains("【唤醒增量】") && delta.contains("【本次消息快照】"), "{delta}");
    surfaces.push(("briefing".into(), briefing));
    surfaces.push(("wake delta".into(), delta));

    // The compatibility, reminder and text-only renders.
    for reason in ["bootstrap", "replyCheck", "poke", "timeout", "proactiveCheck", "reminder:rem-none", "atMention"] {
        surfaces.push((format!("compatibility render {reason}"), crate::prompt::build_with_runtime(&context, key, reason, "小鲸鱼", "9001")));
    }
    surfaces.push(("wake reminder".into(), crate::prompt::build_reminder_with_runtime(&context, key, "小鲸鱼", "9001")));
    surfaces.push(("text-only render".into(), crate::prompt::build_text_only_with_runtime(&context, key, &[], "小鲸鱼", "9001")));
    surfaces.push(("text-only reminder render".into(), crate::prompt::build_text_only_for_wake(&context, key, "reminder:rem-none", "小鲸鱼", "9001")));

    // Tool definitions: every QQ, memory and settings tool, and the auxiliary servers' tools.
    surfaces.push(("tool list".into(), crate::mcp::tool_list(&cfg).to_string()));
    surfaces.push(("auxiliary tool list".into(), Value::Array(crate::aux_mcp::tool_list()).to_string()));

    // The generated DSH MCP block: the server names and serve flags DSH can render into tool names.
    let block = crate::dsh_mcp_config::render_block("/opt/her/her-cli", None);
    let named: Vec<&str> = block.lines().map(str::trim_start)
        .filter(|line| line.starts_with("serverName:") || line.starts_with("- '--")).collect();
    assert_eq!(named.len(), 6, "three server names and three serve flags:\n{block}");
    surfaces.push(("MCP server names and serve flags".into(), named.join("\n")));

    // The digest and slang learner prompts.
    surfaces.push(("digest prompt".into(), crate::digest::DIGEST_PROMPT.to_string()));
    let extraction = crate::slang_learn::build_extraction_prompt(&[], &cfg);
    surfaces.push(("slang repair prompt".into(), crate::slang_learn::build_repair_prompt(&extraction, "not json")));
    surfaces.push(("slang extraction prompt".into(), extraction));
    surfaces.push((
        "slang research prompt".into(),
        crate::slang_learn::research_prompt(&[json!({"content": "破防", "evidence": [{"text": "今天又破防了"}]})], &cfg),
    ));

    // Representative tool results and errors.
    let refusal = crate::wake::normalize_wake_config(&json!({}), &json!({"infinite": true}), &cfg, key, 0, 0.5).unwrap_err();
    surfaces.push(("wake config refusal".into(), refusal.1));
    let image_error = super::super::handle_self_image(&context, &HashMap::from([("key".to_string(), key.to_string())]), &HashMap::new(), &cfg)
        .unwrap_err();
    assert_eq!(image_error.0, 404, "{image_error:?}");
    surfaces.push(("self image error".into(), image_error.1));
    let settings = crate::mcp::call_tool_with_context("settings_get", &json!({"key": "private:7001", "token": "owner-token-142"}), &context).unwrap();
    assert!(settings.contains("禁止在聊天中更改"), "{settings}");
    surfaces.push(("settings_get result".into(), settings));
    for setting in ["socialV2.send.burstEnabled", "dsh.model", "relay.credentials.0"] {
        let read = crate::mcp::call_prepared("settings_get", &json!({"key": "private:7001", "token": "owner-token-142", "settingKey": setting}), &context);
        surfaces.push((format!("settings_get {setting}"), read.to_string()));
    }
    surfaces.push(("harness session line".into(), crate::harness::session_instruction(key)));
    surfaces.push(("harness image placeholder".into(), crate::harness::NO_IMAGE_INPUT.to_string()));
    surfaces.push(("harness summary request and history labels".into(), crate::harness::handover::label_sample()));
    let (primary_search_error, auto_search_error) = search_errors(&context);
    surfaces.push(("web search error (primary)".into(), primary_search_error));
    surfaces.push(("web search error (auto)".into(), auto_search_error));
    surfaces.push(("harness tool errors".into(), format!("{}\n{}", error_literals(include_str!("harness.rs")), error_literals(include_str!("harness/tools.rs")))));
    mutate_social_state_for(&context, |state| {
        state["paused"] = json!(true);
        Ok(())
    })
    .unwrap();
    let paused = super::super::Request {
        method: "GET".into(),
        path: "/api/socialV2/unread".into(),
        query: HashMap::new(),
        headers: HashMap::from([("x-agent-token".to_string(), "epoch-142".to_string())]),
        body: String::new(),
        body_too_large: false,
    };
    let denial = super::super::agent::agent_global_denial(&paused, &cfg, &context).expect("a paused state refuses v2 tools");
    surfaces.push(("paused tool error".into(), denial.1));

    // No surface shows the install layout either.
    let install_root = root.0.to_string_lossy().to_string();
    let found: Vec<String> = surfaces
        .iter()
        .flat_map(|(name, text)| {
            let mut terms = denied_terms(text);
            if text.contains(&install_root) || text.contains(&install_root.replace('/', "\\")) {
                terms.push("the runtime root path".to_string());
            }
            terms.into_iter().map(move |term| format!("{name}: {term}"))
        })
        .collect();
    assert!(found.is_empty(), "model-visible text names a harness, the model, its maker, the install layout or AI:\n{}", found.join("\n"));
}
