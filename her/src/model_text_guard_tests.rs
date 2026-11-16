use super::*;

// Issue #142: no text HER renders for a model names a harness, the model, its platform or developer,
// shows the install layout, or calls her an AI. Every surface this crate can render in-process is
// checked here: the DSH briefing and wake delta, the compatibility, reminder and text-only renders,
// the MCP tool lists with their descriptions and schemas, the generated MCP server names and serve
// arguments, the digest and slang learner prompts, and representative tool errors.

/// Latin terms, matched case-insensitively as whole words (an ASCII letter or digit on either side
/// is not a match, so `dshReady` or `qq_send_message` pass while `user/dsh` and `deepseek娘` fail).
const DENIED_WORDS: [&str; 13] = [
    "deepseek", "dsh", "harness", "opencode", "codex", "claude", "anthropic", "astrbot", "cliproxy",
    "antigravity", "openai", "chatgpt", "gpt",
];

/// Phrases matched case-insensitively anywhere.
const DENIED_PHRASES: [&str; 9] = ["cli-proxy", "人工智能", "机器人", "语言模型", "大模型", "模型", "智能体", "助手", "深度求索"];

/// The denied terms `text` contains. "AI" is matched in upper case with no ASCII letter on either side,
/// so 反AI味 fails while `lastAiReplyAt`, `API` and `EMAIL` pass.
fn denied_terms(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lower = text.to_lowercase();
    let word_edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
    for word in DENIED_WORDS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(word) {
            let (start, end) = (from + at, from + at + word.len());
            if word_edge(lower[..start].chars().next_back()) && word_edge(lower[end..].chars().next()) {
                found.push(word.to_string());
                break;
            }
            from = end;
        }
    }
    let letter = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphabetic());
    let mut from = 0;
    while let Some(at) = text[from..].find("AI") {
        let (start, end) = (from + at, from + at + 2);
        if !letter(text[..start].chars().next_back()) && !letter(text[end..].chars().next()) {
            found.push("AI".to_string());
            break;
        }
        from = end;
    }
    for phrase in DENIED_PHRASES {
        if lower.contains(phrase) {
            found.push(phrase.to_string());
        }
    }
    found
}

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
fn the_denied_term_matcher_finds_whole_words_and_ai_next_to_han_text() {
    assert_eq!(denied_terms("【反AI味】"), vec!["AI".to_string()]);
    assert_eq!(denied_terms("AI 已暂停"), vec!["AI".to_string()]);
    assert_eq!(denied_terms("@deepseek-ai/dsh-mcp-client"), vec!["deepseek".to_string(), "dsh".to_string()]);
    assert_eq!(denied_terms("C:/runtime/user/dsh/.agent-presets"), vec!["dsh".to_string()]);
    assert_eq!(denied_terms("DeepSeek娘.png"), vec!["deepseek".to_string()]);
    assert_eq!(denied_terms("harness tool authority changed"), vec!["harness".to_string()]);
    assert_eq!(denied_terms("Claude-Haiku"), vec!["claude".to_string()]);
    assert_eq!(denied_terms("@机器人"), vec!["机器人".to_string()]);
    assert_eq!(denied_terms("返回给视觉模型"), vec!["模型".to_string()]);
    for clean in ["lastAiReplyAt", "API 令牌", "EMAIL", "dshReady", "qq_send_message", "mcp__web-search-safe__web_search",
        "snowluma_status", "end this turn now: produce your final answer and make no further tool calls"] {
        assert!(denied_terms(clean).is_empty(), "{clean}: {:?}", denied_terms(clean));
    }
    assert_eq!(error_literals("return error(\"a\");\nErr(error(\n    \"b\",\n))\nfn error(reason: &str)"), "a\nb");
}

#[test]
fn settings_get_never_returns_a_secret_at_any_depth() {
    let (_root, context) = fixture_runtime("settings secrets", "fixture-console-token-142s");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"private:7001":{"agentToken":"owner-token-142"}}}"#).unwrap();
    let mut cfg = context.load_config().unwrap();
    cfg["snowluma"] = json!({"httpUrl": "http://127.0.0.1:3000", "accessToken": "fixture-gateway-value-142"});
    cfg["webSearch"] = json!({"provider": "bing", "apiKey": "fixture-search-value-142", "maxResults": 8});
    cfg["relay"] = json!({"auth": {"bearer_token": "fixture-relay-value-142"}, "api-keys": ["fixture-listed-value-142"],
        "upstreams": [{"name": "a", "privateKey": "fixture-item-value-142"}]});
    cfg["security"] = json!({"outboundGuard": true});
    context.write_atomic(RuntimeArea::Config, "config.json", cfg.to_string().as_bytes()).unwrap();
    let read = |setting: &str| {
        memory_tool_action(&context, "settings-get", &json!({"key": "private:7001", "token": "owner-token-142", "settingKey": setting})).unwrap()
    };
    assert_eq!(read("snowluma.accessToken")["value"], "(已隐藏)");
    assert_eq!(read("snowluma")["value"], json!({"httpUrl": "http://127.0.0.1:3000", "accessToken": "(已隐藏)"}));
    assert_eq!(read("webSearch")["value"], json!({"provider": "bing", "apiKey": "(已隐藏)", "maxResults": 8}));
    assert_eq!(read("relay.auth.bearer_token")["value"], "(已隐藏)");
    assert_eq!(read("relay")["value"], json!({"auth": {"bearer_token": "(已隐藏)"}, "api-keys": "(已隐藏)",
        "upstreams": [{"name": "a", "privateKey": "(已隐藏)"}]}));
    assert_eq!(read("security.outboundGuard")["value"], "(已隐藏)");
    assert_eq!(read("security")["value"], "(已隐藏)");
    // Ordinary settings are unchanged.
    assert_eq!(read("snowluma.httpUrl")["value"], "http://127.0.0.1:3000");
    assert_eq!(read("ownerQQ")["value"], 7001);
    let text = crate::mcp::call_tool_with_context("settings_get", &json!({"key": "private:7001", "token": "owner-token-142", "settingKey": "relay"}), &context).unwrap();
    assert!(!text.contains("fixture-") && text.contains("(已隐藏)"), "{text}");
}

#[test]
fn no_model_visible_text_names_a_harness_the_model_its_maker_the_install_layout_or_ai() {
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
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, served_cfg.clone(), served.clone());
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

    // Representative tool results and errors.
    let refusal = crate::wake::normalize_wake_config(&json!({}), &json!({"infinite": true}), &cfg, key, 0, 0.5).unwrap_err();
    surfaces.push(("wake config refusal".into(), refusal.1));
    let image_error = super::super::handle_self_image(&context, &HashMap::from([("key".to_string(), key.to_string())]), &HashMap::new(), &cfg)
        .unwrap_err();
    assert_eq!(image_error.0, 404, "{image_error:?}");
    surfaces.push(("self image error".into(), image_error.1));
    let settings = crate::mcp::call_tool_with_context("settings_get", &json!({"key": "private:7001", "token": "owner-token-142"}), &context).unwrap();
    assert!(settings.contains("不可通过对话修改"), "{settings}");
    surfaces.push(("settings_get result".into(), settings));
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
