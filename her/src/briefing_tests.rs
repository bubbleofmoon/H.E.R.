use super::*;

const KEY: &str = "group:101";

/// A DSH-ready group whose briefing fills every layer: bot identity (optional), operator background, one
/// member impression, a topic, a pending thought, participation, and one memory the unread message recalls.
fn layered_wake_fixture(label: &str, identity: bool) -> (FixtureRoot, RuntimeContext, Value) {
    let (root, context) = fixture_runtime(label, "fixture-console-token-18");
    let state = json!({"conversations":{KEY:{
        "agentToken":"epoch-18","bootstrapSent":true,"lastUnreadSeq":1,"lastReadThroughSeq":0,"lastIncomingAt":1_000,
        "unread":[{"seq":1,"sender":"甲","userId":"1001","text":"deepseek-v4.1-flash v0.2.1","time":1_000,"isSelf":false}],
        "recentMessages":[],
        "activeTopics":[{"text":"层级话题甲","lastMentionAt":1_000}],
        "pendingThoughts":[{"text":"层级想法乙"}],
        "memberImpressions":{"甲":{"traits":["层级印象丙"],"lastSeenAt":1_000,"interactionCount":0}},
        "wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}}}});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "initial-prompts.json", json!({KEY:{"text":"层级背景丁"}}).to_string().as_bytes()).unwrap();
    if identity {
        context.write_atomic(RuntimeArea::Data, "identity.json", r#"{"nickname":"小鲸鱼","userId":"9001"}"#.as_bytes()).unwrap();
    }
    crate::session_map::bind(&context, KEY, "sid-18", None).unwrap();
    let mut allowed = context.load_config().unwrap();
    allowed["allow"]["groups"] = json!([101]);
    context.write_atomic(RuntimeArea::Config, "config.json", allowed.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let conn = initialize_fixture_memory_database(&context);
    crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
        owner_type: "session", owner_id: KEY, scope: "group", kind: "fact", subject: Some("评测"),
        content: "deepseek-v4.1-flash v0.2.1 层级材料戊", tags: None, source: "fixture", importance: 0.5,
    }).unwrap();
    let mut cfg = context.load_config().unwrap();
    cfg["memory"] = json!({"enabled":true,"inject":{"relatedTopK":1}});
    (root, context, cfg)
}

/// Serve `n` wake message packets from the console route; returns the context that knows the port.
fn serve_packets(context: &RuntimeContext, n: usize) -> (RuntimeContext, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
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

fn wake(context: &RuntimeContext, cfg: &Value, rpc: &str) -> crate::prompt::DshWake {
    crate::prompt::prepare_dsh_wake_layered(context, cfg, KEY, "sid-18", "epoch-18", "atMention", rpc, "").unwrap()
}

fn section(text: &str, name: &str) -> String {
    text.split(&format!("【简报节 {name}】\n")).nth(1)
        .map(|rest| rest.split("\n\n【").next().unwrap().to_string()).unwrap_or_default()
}

#[test]
fn every_briefing_section_belongs_to_its_memory_layer_and_the_wake_reports_counts() {
    let (_root, context, cfg) = layered_wake_fixture("layers assigned", true);
    let (context, server) = serve_packets(&context, 1);
    let w = wake(&context, &cfg, "rpc-18");
    server.join().unwrap();
    assert!(section(&w.text, "identity").contains("小鲸鱼"), "{}", w.text);
    assert!(section(&w.text, "operator").contains("层级背景丁"), "{}", w.text);
    assert!(section(&w.text, "impressions").contains("层级印象丙"), "impressions are Relationship's own section:\n{}", w.text);
    let memory = section(&w.text, "memory");
    assert!(memory.contains("层级话题甲") && memory.contains("层级想法乙") && !memory.contains("层级印象丙"), "{memory}");
    assert!(section(&w.text, "participation").contains("参与参考基准"), "{}", w.text);
    assert!(w.text.contains("层级材料戊"), "the recalled memory is Material in the wake delta");
    assert_eq!(w.layers, crate::briefing::Counts { identity: 1, relationship: 2, conversation: 3, material: 1 });
    assert_eq!(w.cut, crate::briefing::Counts::default());
    assert!(w.alerted.is_empty());
}

#[test]
fn under_budget_pressure_material_is_cut_first_then_conversation_and_identity_relationship_never() {
    let (_root, context, mut cfg) = layered_wake_fixture("layers cut order", true);
    let (context, server) = serve_packets(&context, 2);
    // Identity is over a 1-token budget; the overflow takes all of Material's 10 tokens first.
    cfg["memory"]["layers"] = json!({"identityTokens":1,"materialTokens":10});
    let w = wake(&context, &cfg, "rpc-18a");
    assert!(!w.text.contains("层级材料戊"), "material is cut first:\n{}", w.text);
    assert!(w.recalled.is_empty(), "a cut memory is not carried, so it is never counted as a hit");
    assert_eq!((w.layers.material, w.cut.material), (0, 1));
    assert!(w.text.contains("小鲸鱼") && w.text.contains("层级背景丁") && w.text.contains("层级印象丙") && w.text.contains("层级话题甲"));
    // Budgets are runtime configuration: the next wake reads the edited value.
    cfg["memory"]["layers"] = json!({"conversationTokens":1});
    let w = wake(&context, &cfg, "rpc-18b");
    server.join().unwrap();
    for gone in ["层级话题甲", "层级想法乙", "参与参考基准"] {
        assert!(!w.text.contains(gone), "conversation is cut: {gone}\n{}", w.text);
    }
    assert!(w.text.contains("小鲸鱼") && w.text.contains("层级背景丁") && w.text.contains("层级印象丙"), "identity and relationship are never cut");
    assert!(w.text.contains("层级材料戊"), "material fits its own budget again");
    assert_eq!(w.layers, crate::briefing::Counts { identity: 1, relationship: 2, conversation: 0, material: 1 });
    assert_eq!(w.cut.conversation, 3);
}

#[test]
fn a_wake_with_an_empty_identity_raises_the_owner_alert_once_per_generation() {
    let (_root, context, mut cfg) = layered_wake_fixture("layers empty identity", false);
    cfg.as_object_mut().unwrap().remove("ownerQQ"); // the alert is only logged: no owner message from a unit test
    let (context, server) = serve_packets(&context, 2);
    let first = wake(&context, &cfg, "rpc-18c");
    let again = wake(&context, &cfg, "rpc-18d");
    server.join().unwrap();
    assert_eq!(first.layers.identity, 0, "{}", first.text);
    assert_eq!(first.alerted, vec![crate::briefing::Layer::Identity]);
    assert!(again.alerted.is_empty(), "one alert per conversation and session generation");
}

#[test]
fn a_strip_that_empties_the_identity_section_is_not_counted_as_delivered_and_alerts() {
    let (_root, context, mut cfg) = layered_wake_fixture("layers stripped identity", true);
    cfg.as_object_mut().unwrap().remove("ownerQQ"); // the alert is only logged: no owner message from a unit test
    cfg["socialV2"]["context"]["stripFromModelText"] = json!([crate::prompt::identity_line("小鲸鱼", "9001")]);
    let (context, server) = serve_packets(&context, 1);
    let w = wake(&context, &cfg, "rpc-18e");
    server.join().unwrap();
    assert!(!w.text.contains("你在 QQ 上的昵称是"), "the strip removed the identity text:\n{}", w.text);
    assert_eq!(w.layers.identity, 0, "a section the strip emptied delivers nothing");
    assert_eq!(w.alerted, vec![crate::briefing::Layer::Identity]);
}

#[test]
fn the_index_hint_line_stays_within_its_allowance() {
    let (_root, context, cfg) = layered_wake_fixture("layers index hint", true);
    let conn = crate::memory::open_for_runtime(&context).unwrap();
    for n in 0..6 {
        let subject = format!("{}{n}", "长话题".repeat(30));
        let content = format!("与当前消息无关的记录{n}");
        crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
            owner_type: "session", owner_id: KEY, scope: "group", kind: "fact",
            subject: Some(&subject), content: &content, tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
    }
    let state = context.read_social_state().unwrap();
    let block = crate::prompt::recall_block_with_runtime(&context, &cfg, &state["conversations"][KEY], KEY);
    assert!(block.contains("【记忆】"), "the fixture recalls its matching memory:\n{block}");
    if let Some(hint) = block.lines().find(|line| line.starts_with("【记忆索引】")) {
        let tokens = crate::briefing::estimate_tokens(hint);
        assert!(tokens <= crate::prompt::RECALL_HINT_MAX, "the index hint takes {tokens} tokens:\n{hint}");
    }
}

#[test]
fn a_stripped_operator_section_delivers_nothing_and_relationship_counts_only_what_is_left() {
    let (_root, context, mut cfg) = layered_wake_fixture("layers stripped operator", true);
    cfg.as_object_mut().unwrap().remove("ownerQQ"); // the alert is only logged: no owner message from a unit test
    let operator = crate::initial_prompts::format_injection(&context, &cfg, KEY);
    assert!(operator.contains("层级背景丁"), "{operator}");
    cfg["socialV2"]["context"]["stripFromModelText"] = json!([operator]);
    let (context, server) = serve_packets(&context, 1);
    let w = wake(&context, &cfg, "rpc-18f");
    server.join().unwrap();
    assert!(!w.text.contains("层级背景丁"), "the strip removed the operator text:\n{}", w.text);
    assert!(w.text.contains("层级印象丙"), "{}", w.text);
    assert_eq!(w.layers.relationship, 1, "the operator section delivers nothing; only the impression is left: {:?}", w.layers);
}

#[test]
fn a_stripped_slang_section_drops_its_conversation_items() {
    let (_root, context, mut cfg) = layered_wake_fixture("layers stripped slang", true);
    cfg.as_object_mut().unwrap().remove("ownerQQ"); // the alert is only logged: no owner message from a unit test
    context.write_atomic(RuntimeArea::Data, "slang.json",
        json!([{"status":"confirmed","content":"层级黑话戊","meaning":"层级含义己","count":3}]).to_string().as_bytes()).unwrap();
    let slang = crate::slang::build_context(&crate::slang::load_entries_for_runtime(&context), None);
    assert!(slang.contains("层级黑话戊"), "{slang}");
    cfg["socialV2"]["context"]["stripFromModelText"] = json!([slang]);
    let (context, server) = serve_packets(&context, 1);
    let w = wake(&context, &cfg, "rpc-18g");
    server.join().unwrap();
    assert!(!w.text.contains("层级黑话戊"), "the strip removed the slang text:\n{}", w.text);
    assert_eq!(w.layers.conversation, 3, "topic, thought and participation; the stripped slang delivers nothing: {:?}", w.layers);
}
