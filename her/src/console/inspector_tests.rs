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
    assert_eq!(serve_request(&req, &cfg, &context, 3999).0, 401);
    req.headers
        .insert("x-console-token".into(), "fixture-console".into());
    let (status, content_type, html) = serve_request(&req, &cfg, &context, 3999);
    assert_eq!(status, 200);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(html.contains(TURN));
    req.headers
        .insert("x-agent-call".into(), "fixture-agent".into());
    assert_eq!(serve_request(&req, &cfg, &context, 3999).0, 403);
    assert_eq!(std::fs::read(ledger).unwrap(), before);
    assert!(!audit.exists());
    assert!(!fixture.0.join("data/conversations.db").exists());
}
