use crate::console::{admin_request, agent_request, serve_request, Request};
use crate::harness::{store::Store, Event, Limits, ModelRoute, Status, Tools, TurnRequest};
use crate::runtime::{tests::runtime_fixture, RuntimeContext};
use crate::turn_ledger::{mint_id, Recorder};
use genai::chat::{ChatMessage, ToolCall, ToolResponse};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

use super::{CONVERSATIONS, PAGE_PATH, TURN};

fn fixture() -> (crate::runtime::tests::FixtureRoot, RuntimeContext, Value) {
    let cfg = json!({
        "ownerQQ": 8808,
        "consoleToken": "fixture-console",
        "harness": {"model": "fixture-chat"}
    });
    let root = std::env::temp_dir().join(mint_id("her-inspector-"));
    let fixture = runtime_fixture(
        root,
        &cfg,
        &json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:2",
            "onebot": "http://127.0.0.1:3"
        }),
    );
    std::fs::write(
        fixture.0.join("config/harness-api-key"),
        "fixture-model-key",
    )
    .unwrap();
    let context = RuntimeContext::from_root(&fixture.0).unwrap();
    (fixture, context, cfg)
}

fn admit(context: &RuntimeContext, cfg: &Value) -> (Store, Recorder) {
    let generation = "fixture-generation";
    let turn = Recorder::begin(
        context,
        "private:8808",
        "harness",
        "message",
        generation,
        &[],
        None,
    )
    .unwrap();
    let store = Store::open(context).unwrap();
    store
        .admit(&TurnRequest {
            id: turn.id.clone(),
            conversation: turn.key.clone(),
            generation: generation.into(),
            prompt: "fixture question".into(),
            media: Vec::new(),
            profile: "closed-agent".into(),
            route: ModelRoute::from_runtime(context, cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: Tools::empty(),
            limits: Limits::from_config(cfg),
        })
        .unwrap();
    turn.attempt("fixture-dispatch", "").unwrap();
    turn.accepted("fixture-dispatch").unwrap();
    (store, turn)
}

fn timeline(context: &RuntimeContext, turn: &Recorder, after: i64) -> Value {
    let path = format!("{TURN}?turnId={}&after={after}", turn.id);
    let (status, reply) = admin_request(context, "GET", &path, None);
    assert_eq!(status, 200, "read timeline: {reply}");
    reply
}

#[test]
fn inspector_lists_conversations() {
    let (fixture, context, _) = fixture();
    let (_, empty) = admin_request(&context, "GET", CONVERSATIONS, None);
    assert_eq!(empty["conversations"], json!([]));
    assert!(!fixture.0.join("data/conversations.db").exists());
    let first = Recorder::begin(&context, "group:42", "dsh", "message", "", &[], None).unwrap();
    let second = Recorder::begin(
        &context,
        "private:8808",
        "harness",
        "message",
        "",
        &[],
        None,
    )
    .unwrap();
    let (status, reply) = admin_request(&context, "GET", CONVERSATIONS, None);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply["conversations"],
        json!(["group:42", "private:8808"]),
        "conversation keys come from saved turns"
    );
    let (_, reply) = admin_request(
        &context,
        "GET",
        &format!("{CONVERSATIONS}?conversation=group:42"),
        None,
    );
    assert_eq!(reply["turns"][0]["turnId"], first.id);
    assert_eq!(reply["turns"].as_array().unwrap().len(), 1);
    first.attempt("fixture-rpc", "fixture-session").unwrap();
    let dsh = timeline(&context, &first, 0);
    assert_eq!(dsh["ledger"]["attempts"][0]["rpcId"], "fixture-rpc");
    let (_, reply) = admin_request(
        &context,
        "GET",
        &format!("{TURN}?turnId={}", second.id),
        None,
    );
    assert_eq!(reply["events"], json!([]));
    assert_eq!(
        admin_request(&context, "GET", &format!("{TURN}?turnId=missing"), None).0,
        404
    );
}

#[test]
fn inspector_bounds_and_pages_conversation_turns() {
    let (_fixture, context, _) = fixture();
    let key = "group:42";
    for n in 100..203 {
        Recorder::begin(
            &context,
            &format!("group:{n}"),
            "dsh",
            "message",
            "",
            &[],
            None,
        )
        .unwrap();
    }
    let turns: Vec<_> = (0..103)
        .map(|_| {
            Recorder::begin(&context, key, "dsh", "message", "", &[], None)
                .unwrap()
                .id
        })
        .collect();
    let browse = format!("{CONVERSATIONS}?conversation={key}");
    let (status, first) = admin_request(&context, "GET", &format!("{browse}&limit=999"), None);
    assert_eq!(status, 200, "{first}");
    assert_eq!(
        first["turns"].as_array().unwrap().len(),
        100,
        "turn list is capped even when the client requests more"
    );
    assert_eq!(first["conversations"].as_array().unwrap().len(), 100);
    assert_eq!(first["turns"][0]["turnId"], turns[102]);
    assert_eq!(first["nextBefore"], turns[3]);
    let (_, default_page) = admin_request(&context, "GET", &browse, None);
    assert_eq!(default_page["turns"].as_array().unwrap().len(), 50);

    let newer = Recorder::begin(&context, key, "dsh", "message", "", &[], None).unwrap();
    let (_, newest) = admin_request(&context, "GET", &format!("{browse}&limit=1"), None);
    assert_eq!(newest["turns"][0]["turnId"], newer.id);
    let (_, older) = admin_request(
        &context,
        "GET",
        &format!("{browse}&before={}&limit=3", turns[3]),
        None,
    );
    let older_ids: Vec<_> = older["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["turnId"].as_str().unwrap())
        .collect();
    assert_eq!(
        older_ids,
        [&turns[2], &turns[1], &turns[0]].map(String::as_str)
    );
    assert!(older["nextBefore"].is_null());
    for query in ["limit=0", "before=missing"] {
        assert_eq!(
            admin_request(&context, "GET", &format!("{browse}&{query}"), None).0,
            400
        );
    }
}

#[test]
fn inspector_replays_events_in_sequence() {
    let (_fixture, context, cfg) = fixture();
    let (store, turn) = admit(&context, &cfg);
    let call = ToolCall {
        call_id: "fixture-call".into(),
        fn_name: "qq_get_status".into(),
        fn_arguments: json!({"example":true}),
        thought_signatures: None,
    };
    let assistant = Event::Assistant {
        text: "Checking.".into(),
        raw: json!({
            "captured_reasoning_content":"fixture reasoning",
            "captured_usage":{"prompt_tokens":12,"completion_tokens":3}
        }),
        message: ChatMessage::assistant("Checking."),
    };
    let attempt = store.complete_step(&turn.id, &assistant).unwrap();
    let run = store.tool_start(&turn.id, &attempt, &call).unwrap();
    let message = ChatMessage::tool(ToolResponse::from_tool_call(
        &call,
        "fixture result".to_string(),
    ));
    store
        .tool_result(
            &turn.id,
            &run,
            &message,
            &json!({"result":"fixture result"}),
        )
        .unwrap();
    store
        .finish(&turn.id, &Status::Completed("done".into()), None)
        .unwrap();
    turn.end("completed", "done").unwrap();

    let reply = timeline(&context, &turn, 0);
    let events = reply["events"].as_array().unwrap();
    let kinds: Vec<_> = events
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "turn_admitted",
            "user",
            "model_attempt_started",
            "assistant",
            "model_attempt_ended",
            "tool_started",
            "tool_result",
            "turn_ended"
        ],
        "timeline follows durable event sequence"
    );
    assert_eq!(events[3]["payload"]["reasoning"], "fixture reasoning");
    assert_eq!(events[3]["payload"]["usage"]["tokens"].as_f64(), Some(15.0));
    assert!(events[3]["payload"]["usage"]["cost"].as_f64().unwrap() > 0.0);
    assert_eq!(events[5]["payload"]["id"], events[6]["payload"]["id"]);
    assert_eq!(events[6]["payload"]["result"]["result"], "fixture result");
    assert_eq!(reply["turn"]["state"], "completed");
    assert_eq!(reply["turn"]["outcomeReason"], "done");
    assert_eq!(reply["live"], false);
}

#[test]
fn inspector_masks_stored_event_payloads() {
    let (_fixture, context, cfg) = fixture();
    let (store, turn) = admit(&context, &cfg);
    let token_shape = "ab".repeat(24);
    let detail = format!("before fixture-model-key and {token_shape} after");
    let masked = "before *** and *** after";
    let call = ToolCall {
        call_id: "fixture-call".into(),
        fn_name: "qq_get_status".into(),
        fn_arguments: json!({"detail": detail}),
        thought_signatures: None,
    };
    let assistant = Event::Assistant {
        text: "Checking.".into(),
        raw: json!({"captured_reasoning_content": detail}),
        message: ChatMessage::assistant("Checking."),
    };
    let attempt = store.complete_step(&turn.id, &assistant).unwrap();
    let run = store.tool_start(&turn.id, &attempt, &call).unwrap();
    let message = ChatMessage::tool(ToolResponse::from_tool_call(&call, detail.clone()));
    store
        .tool_result(&turn.id, &run, &message, &json!({"detail": detail}))
        .unwrap();

    let reply = timeline(&context, &turn, 0);
    let events = reply["events"].as_array().unwrap();
    let payload =
        |kind: &str| &events.iter().find(|event| event["kind"] == kind).unwrap()["payload"];
    assert_eq!(
        payload("assistant")["reasoning"],
        masked,
        "stored reasoning is masked"
    );
    assert_eq!(
        payload("tool_started")["call"]["fn_arguments"]["detail"],
        masked
    );
    assert_eq!(payload("tool_result")["result"]["detail"], masked);
}

#[test]
fn inspector_returns_new_events_during_a_running_turn() {
    let (_fixture, context, cfg) = fixture();
    let (store, turn) = admit(&context, &cfg);
    let first = timeline(&context, &turn, 0);
    assert_eq!(first["live"], true);
    let after = first["nextAfter"].as_i64().unwrap();
    assert_eq!(timeline(&context, &turn, after)["events"], json!([]));

    store.fail_attempt(&turn.id).unwrap();
    let later = timeline(&context, &turn, after);
    assert_eq!(
        later["events"].as_array().unwrap().len(),
        1,
        "poll sees new events before the turn ends"
    );
    assert_eq!(later["events"][0]["kind"], "model_attempt_ended");
    assert_eq!(later["live"], true);
    let after = later["nextAfter"].as_i64().unwrap();
    for _ in 0..49 {
        store
            .checkpoint(
                &turn.id,
                Some(json!({"purpose":"budget","class":"timeout"})),
                None,
            )
            .unwrap();
    }
    store
        .checkpoint(
            &turn.id,
            None,
            Some(json!({"summary":"x".repeat(64 * 1024)})),
        )
        .unwrap();
    let boundary = timeline(&context, &turn, after);
    assert_eq!(boundary["events"].as_array().unwrap().len(), 50);
    assert_eq!(boundary["more"], false);
    assert!(boundary["events"][49]["payload"]["omitted"].is_string());
    store
        .finish(&turn.id, &Status::Completed("done".into()), None)
        .unwrap();
    turn.end("completed", "done").unwrap();
    let full = timeline(&context, &turn, after);
    assert_eq!(full["events"].as_array().unwrap().len(), 50);
    assert_eq!(full["more"], true);
    let finished = timeline(&context, &turn, full["nextAfter"].as_i64().unwrap());
    assert_eq!(finished["live"], false);
    assert_eq!(finished["more"], false);
    assert_eq!(finished["events"][0]["kind"], "turn_ended");
    let invalid = format!("{TURN}?turnId={}&after=-1", turn.id);
    assert_eq!(admin_request(&context, "GET", &invalid, None).0, 400);
}

#[test]
fn inspector_pages_events_within_a_byte_budget() {
    let (_fixture, context, cfg) = fixture();
    let (store, turn) = admit(&context, &cfg);
    let after = timeline(&context, &turn, 0)["nextAfter"].as_i64().unwrap();
    let summary = "测".repeat(20 * 1024);
    for index in 0..9 {
        store
            .checkpoint(
                &turn.id,
                None,
                Some(json!({"summary": summary, "index": index})),
            )
            .unwrap();
    }

    let first = timeline(&context, &turn, after);
    let first_events = first["events"].as_array().unwrap();
    assert_eq!(first_events.len(), 8);
    assert!(first["events"].to_string().len() <= 512 * 1024);
    assert_eq!(first["more"], true);
    let next_after = first["nextAfter"].as_i64().unwrap();
    assert_eq!(first_events[7]["seq"], next_after);
    let next = timeline(&context, &turn, next_after);
    let next_events = next["events"].as_array().unwrap();
    assert_eq!(next_events.len(), 1);
    assert_eq!(next_events[0]["seq"], next_after + 1);
    assert_eq!(next_events[0]["payload"]["summary"], summary);
    assert_eq!(next["more"], false);
}

#[test]
fn inspector_get_rejects_foreign_host_and_origin() {
    let (_fixture, context, cfg) = fixture();
    let turn = Recorder::begin(&context, "group:42", "dsh", "message", "", &[], None).unwrap();
    for path in [CONVERSATIONS, TURN, PAGE_PATH] {
        let mut req = Request {
            method: "GET".into(),
            path: path.into(),
            query: HashMap::from([("turnId".into(), turn.id.clone())]),
            headers: HashMap::from([
                ("host".into(), "127.0.0.1:3999".into()),
                ("origin".into(), "http://127.0.0.1:3999".into()),
                (
                    "x-console-token".into(),
                    context.effective_console_token(&cfg),
                ),
            ]),
            body: String::new(),
            body_too_large: false,
        };
        assert_eq!(serve_request(&req, &cfg, &context, 3999).status, 200, "{path}");
        req.headers
            .insert("host".into(), "foreign.invalid:3999".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, 3999).status,
            403,
            "{path} Host"
        );
        req.headers.insert("host".into(), "127.0.0.1:3999".into());
        req.headers
            .insert("origin".into(), "http://foreign.invalid:3999".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, 3999).status,
            403,
            "{path} Origin"
        );
        req.headers.insert("origin".into(), String::new());
        assert_eq!(
            serve_request(&req, &cfg, &context, 3999).status,
            403,
            "{path} empty Origin"
        );
        req.headers.remove("x-console-token");
        req.headers
            .insert("origin".into(), "http://foreign.invalid:3999".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, 3999).status,
            403,
            "{path} unauthenticated foreign Origin"
        );
    }
}

#[test]
fn inspector_refuses_mutation_and_agent_headers() {
    let (fixture, context, cfg) = fixture();
    let turn = Recorder::begin(&context, "group:42", "dsh", "message", "", &[], None).unwrap();
    let ledger = fixture
        .0
        .join("data")
        .join(crate::turn_ledger::ISOLATED_LEDGER_FILE);
    let before = std::fs::read(&ledger).unwrap();
    let audit = fixture.0.join("data/audit/audit.jsonl");
    assert!(!audit.exists());
    let detail = format!("{TURN}?turnId={}", turn.id);
    for path in [CONVERSATIONS, detail.as_str(), PAGE_PATH] {
        for method in ["POST", "PATCH", "DELETE", "PUT"] {
            let (status, _) =
                admin_request(&context, method, path, Some(&json!({"action":"abandon"})));
            assert_eq!(status, 405, "inspector refuses {method} {path}");
        }
        assert_eq!(
            agent_request(&context, "GET", path, None, "fixture-agent").0,
            403
        );
    }
    let mut req = Request {
        method: "GET".into(),
        path: PAGE_PATH.into(),
        query: HashMap::new(),
        headers: HashMap::from([("host".into(), "127.0.0.1:3999".into())]),
        body: String::new(),
        body_too_large: false,
    };
    assert_eq!(serve_request(&req, &cfg, &context, 3999).status, 401);
    req.headers
        .insert("x-console-token".into(), "fixture-console".into());
    let resp = serve_request(&req, &cfg, &context, 3999);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "text/html; charset=utf-8");
    assert!(resp.body.contains(TURN));
    req.headers
        .insert("x-agent-call".into(), "fixture-agent".into());
    assert_eq!(serve_request(&req, &cfg, &context, 3999).status, 403);
    assert_eq!(std::fs::read(ledger).unwrap(), before);
    assert!(!audit.exists());
    assert!(!fixture.0.join("data/conversations.db").exists());
}

#[test]
fn inspector_accepts_browser_session_and_rejects_cross_site_browser_requests() {
    let (_fixture, context, cfg) = fixture();
    let turn = Recorder::begin(&context, "group:42", "dsh", "message", "", &[], None).unwrap();
    let port = 3999;
    let issued = crate::console_auth::auth_first_run_browser(
        &context,
        port,
        &context.effective_console_token(&cfg),
        "fixture-password-123",
    )
    .unwrap();
    let cookie = crate::console::front::BrowserCookie::Renew {
        port,
        digest: issued.id_digest,
        id: issued.id,
    }
    .header()
    .split_once(';')
    .unwrap()
    .0
    .to_string();
    let canonical = crate::console_auth::canonical_host(port);
    let canonical_origin = crate::console_auth::canonical_origin(port);

    for (path, expect_snippet) in [
        (PAGE_PATH, TURN),
        (CONVERSATIONS, "\"group:42\""),
        (TURN, &turn.id),
    ] {
        let mut req = Request {
            method: "GET".into(),
            path: path.into(),
            query: HashMap::from([("turnId".into(), turn.id.clone())]),
            headers: HashMap::from([
                ("host".into(), canonical.clone()),
                ("cookie".into(), cookie.clone()),
                ("sec-fetch-site".into(), "same-origin".into()),
                ("origin".into(), canonical_origin.clone()),
            ]),
            body: String::new(),
            body_too_large: false,
        };
        let ok = serve_request(&req, &cfg, &context, port);
        assert_eq!(ok.status, 200, "{path}: {}", ok.body);
        assert!(ok.body.contains(expect_snippet), "{path}");

        req.headers
            .insert("sec-fetch-site".into(), "cross-site".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, port).status,
            403,
            "{path} cross-site"
        );

        req.headers
            .insert("sec-fetch-site".into(), "same-origin".into());
        req.headers
            .insert("origin".into(), "http://foreign.invalid:3999".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, port).status,
            403,
            "{path} foreign origin"
        );

        req.headers
            .insert("origin".into(), canonical_origin.clone());
        req.headers
            .insert("x-agent-token".into(), "fixture-agent".into());
        assert_eq!(
            serve_request(&req, &cfg, &context, port).status,
            401,
            "{path} agent header prevents browser cookie fallback"
        );
    }
}
