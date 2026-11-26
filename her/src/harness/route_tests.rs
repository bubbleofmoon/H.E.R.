//! #7: a turn's candidate chain on scripted loopback model fixtures; no real model service is contacted.
use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHUNK: &str = "chat.completion.chunk";

/// An HTTP 200 server-sent event stream of `chunks`.
fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The stop frame of every scripted answer: it reports 15 tokens.
fn stop(finish: &str) -> Value {
    json!({"id":"fake-7","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{},"finish_reason":finish}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}})
}

fn text(answer: &str) -> String {
    sse(&[
        json!({"id":"fake-7","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{"role":"assistant","content":answer},"finish_reason":null}]}),
        stop("stop"),
    ])
}

/// An answer with no text and no tool call.
fn empty() -> String {
    sse(&[stop("stop")])
}

/// A provider content-filter stop: a refusal.
fn refusal() -> String {
    sse(&[json!({"id":"fake-7","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})])
}

/// A failed response with `headers` (each ending in CRLF).
fn status(code: u16, headers: &str) -> String {
    let body = json!({"error":{"message":"fixture failure","type":"fixture"}}).to_string();
    format!(
        "HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// One `qq_send_private_message` call to the owner.
fn send_call(id: &str) -> String {
    let call = json!({"index":0,"id":id,"type":"function",
        "function":{"name":"qq_send_private_message","arguments":json!({"userId":8808,"message":id}).to_string()}});
    sse(&[
        json!({"id":"fake-7","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":null}]}),
        stop("tool_calls"),
    ])
}

async fn read_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "truncated fixture request");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..split])
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            if bytes.len() >= split + 4 + length {
                return serde_json::from_slice(&bytes[split + 4..split + 4 + length]).unwrap_or(Value::Null);
            }
        }
    }
}

/// Answers one request per connection with `responses`, in order, and returns each request body.
fn serve(listener: Arc<TcpListener>, responses: Vec<String>) -> tokio::task::JoinHandle<Vec<Value>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .expect("the fixture received no request")
                .unwrap();
            requests.push(read_body(&mut socket).await);
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        }
        requests
    })
}

struct Rig {
    root: std::path::PathBuf,
    context: RuntimeContext,
    generation: String,
    cfg: Value,
    model: Arc<TcpListener>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Expire the effective cap after the first request reaches the model, with no pipeline poll.
async fn reserved2_response_after_cap(response: String, recall: bool) -> (Rig, Result<String, String>) {
    let mut rig = rig("reserved2-response-after-cap", json!({"model":"fake","summaries":false}), None).await;
    let key = "private:8808";
    rig.cfg["allow"] = json!({"private":[8808]});
    rig.context.write_atomic(RuntimeArea::Config, "config.json", rig.cfg.to_string().as_bytes()).unwrap();
    rig.context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let began = crate::turn_ledger::now_ms();
    let budget = crate::pipeline::Reserved2Budget {
        deadline: began - 1, cap_ms: 60_000, ceiling: began + 120_000, began,
    };
    crate::conversation_state::update_raw(&rig.context, |state| {
        state["conversations"][key]["engagedAt"] = json!(began);
        Ok(())
    }).unwrap();
    let recalled = if recall {
        let conn = crate::memory::open_ready_for_runtime(&rig.context).unwrap();
        vec![crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
            owner_type: "session", owner_id: key, scope: "private", kind: "fact", subject: None,
            content: "recalled fixture memory", tags: None, source: "fixture", importance: 0.5,
        }).unwrap()]
    } else {
        Vec::new()
    };
    let model = rig.model.clone();
    let context = rig.context.clone();
    let server = async move {
        let (mut socket, _) = model.accept().await.unwrap();
        assert_eq!(read_body(&mut socket).await["stream"], true);
        // Returning to observation removes the extension. The dispatch deadline is already past.
        crate::conversation_state::update_raw(&context, |state| {
            state["conversations"][key]["engagedAt"] = json!(0);
            Ok(())
        }).unwrap();
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    };
    let harness = Harness::open(&rig.context).unwrap();
    let mut turn = harness.admit_with_delivery(TurnRequest {
        id: "reserved2-cap-boundary".into(), conversation: key.into(), generation: rig.generation.clone(),
        prompt: if recall { "fixture wake with recalled fixture memory" } else { "fixture wake" }.into(),
        media: Vec::new(), profile: "reserved2".into(),
        route: ModelRoute::for_conversation(&rig.context, &rig.cfg, key).unwrap(),
        deadline: Duration::from_secs(10),
        tools: Tools::capture(&rig.context, &rig.cfg, "reserved2", key, &rig.generation).unwrap(),
        limits: rig.limits(),
    }, TurnDelivery { reserved2_budget: Some(budget), recalled, ..Default::default() }).unwrap();
    let (_, result) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(server, turn.wait()) })
        .await.unwrap();
    (rig, result)
}

#[tokio::test]
async fn ticket159_reserved2_harness_does_not_complete_a_response_after_the_cap() {
    let (rig, result) = reserved2_response_after_cap(text("late internal output"), false).await;
    assert_eq!(result, Err(DEADLINE_EXCEEDED.into()), "a model response after the effective cap must time out");
    assert!(matches!(Harness::open(&rig.context).unwrap().status("reserved2-cap-boundary").unwrap(),
        Some(Status::Failed(reason)) if reason == DEADLINE_EXCEEDED));
}

#[tokio::test]
async fn ticket159_reserved2_harness_does_not_start_a_tool_after_the_cap() {
    let (rig, result) = reserved2_response_after_cap(send_call("late-tool"), false).await;
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    let intents: i64 = db.query_row("SELECT count(*) FROM tool_runs WHERE turn_id = ?1",
        ["reserved2-cap-boundary"], |row| row.get(0)).unwrap();
    assert_eq!(intents, 0, "a tool offered after the effective cap must create no dispatch intent");
    assert_eq!(result, Err(DEADLINE_EXCEEDED.into()));
}

#[tokio::test]
async fn ticket159_reserved2_harness_late_first_response_does_not_count_recall() {
    let (rig, result) = reserved2_response_after_cap(text("late recalled answer"), true).await;
    assert_eq!(result, Err(DEADLINE_EXCEEDED.into()));
    let conn = crate::memory::open_ready_for_runtime(&rig.context).unwrap();
    let hits: i64 = conn.query_row("SELECT hits FROM memory", [], |row| row.get(0)).unwrap();
    assert_eq!(hits, 0, "a response rejected at the effective cap must not count recall");
}

/// A selected runtime whose owner's private chat runs closed-agent harness turns with `harness` as its settings.
async fn rig(tag: &str, harness: Value, console: Option<u16>) -> Rig {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-7-{tag}-{}", crate::turn_ledger::mint_id(""))),
    );
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("http://{}", model.local_addr().unwrap());
    let cfg = json!({"ownerQQ":8808, "consoleToken":"synthetic-console-token-7", "harness":harness});
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-7").unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
    )
    .unwrap();
    let mut context = RuntimeContext::from_root(&root).unwrap();
    if let Some(port) = console {
        context = context.with_console_port(port);
    }
    let token = crate::social_state::ensure_or_rotate(&context, "private:8808", false).unwrap();
    let generation = crate::send_outbox::token_epoch(&token);
    Rig {
        root,
        context,
        generation,
        cfg,
        model,
    }
}

impl Rig {
    /// The configured limits, with retries waiting a millisecond instead of a second.
    fn limits(&self) -> Limits {
        Limits::from_config(&self.cfg).with_retry_unit(Duration::from_millis(1))
    }

    fn admit(&self, id: &str) -> Turn {
        Harness::open(&self.context)
            .unwrap()
            .admit(TurnRequest {
                id: id.into(),
                conversation: "private:8808".into(),
                generation: self.generation.clone(),
                prompt: "reply".into(),
                media: Vec::new(),
                profile: "closed-agent".into(),
                // As the pipeline builds it (#162): every route test walks the conversation's order.
                route: ModelRoute::for_conversation(&self.context, &self.cfg, "private:8808").unwrap(),
                deadline: Duration::from_secs(20),
                tools: Tools::capture(&self.context, &self.cfg, "closed-agent", "private:8808", &self.generation)
                    .unwrap(),
                limits: self.limits(),
            })
            .unwrap()
    }

    /// Each model attempt in order: (route id its start event recorded, model, state).
    fn attempts(&self) -> Vec<(String, String, String)> {
        let db = rusqlite::Connection::open(self.root.join("data/conversations.db")).unwrap();
        let mut statement = db
            .prepare(
                "SELECT e.payload, a.model, a.state FROM events e JOIN model_attempts a ON e.attempt_id = a.id
                 WHERE e.kind = 'model_attempt_started' ORDER BY e.seq",
            )
            .unwrap();
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.into_iter()
            .map(|(payload, model, state)| {
                let payload: Value = serde_json::from_str(&payload).unwrap();
                (payload["route"].as_str().unwrap_or("").to_string(), model, state)
            })
            .collect()
    }

    /// The reasons of the run-record entries named `event`, newest first.
    fn records(&self, event: &str) -> Vec<String> {
        crate::recovery_log::page(&self.context, &crate::run_record::Query::default())
            .unwrap()
            .records
            .into_iter()
            .filter(|record| record.event == event)
            .map(|record| record.reason)
            .collect()
    }

    async fn no_more_requests(&self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), self.model.accept())
                .await
                .is_err(),
            "the harness made another model request"
        );
    }
}

async fn ended(turn: &mut Turn) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(15), turn.wait())
        .await
        .expect("the turn did not end")
}

fn models(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .map(|request| request["model"].as_str().unwrap_or("").to_string())
        .collect()
}

fn attempt(route: &str, model: &str, state: &str) -> (String, String, String) {
    (route.into(), model.into(), state.into())
}

fn class(summary: &str) -> &'static str {
    crate::model_failure::Failure::classify("", summary).class
}

fn two_routes() -> Value {
    json!({"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]})
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_explicit_error_fails_over_to_the_next_route() {
    let rig = rig("failover", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(500, ""), text("from b")]);
    let mut turn = rig.admit("failover-1");
    assert_eq!(ended(&mut turn).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
    assert_eq!(
        rig.attempts(),
        [attempt("a", "model-a", "failed"), attempt("b", "model-b", "completed")],
        "each attempt records its route"
    );
    let failures = turn.route_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!((failures[0].route.as_str(), failures[0].model.as_str()), ("a", "model-a"));
    assert_eq!(class(&failures[0].summary), "backend");
    assert_eq!(turn.model(), "model-b", "the turn reports the route that answered");
    let steps = rig.records("failover-step");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!(
        steps[0].contains("route a") && steps[0].contains(" bytes;") && steps[0].contains("next route b"),
        "the record names the route and the request size: {steps:?}"
    );
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_refusal_fails_over_to_the_next_route() {
    let rig = rig("refusal", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![refusal(), text("from b")]);
    let mut turn = rig.admit("refusal-1");
    assert_eq!(ended(&mut turn).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
    let failures = turn.route_failures();
    assert!(
        failures.len() == 1 && failures[0].model == "model-a" && class(&failures[0].summary) == "refusal",
        "the refused route is reported with its model: {failures:?}"
    );
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_the_degraded_route_starts_the_next_turn_until_the_chain_changes() {
    let mut rig = rig("sticky", two_routes(), None).await;
    let first = serve(rig.model.clone(), vec![status(500, ""), text("from b")]);
    assert_eq!(ended(&mut rig.admit("sticky-1")).await, Ok("from b".to_string()));
    assert_eq!(models(&first.await.unwrap()), ["model-a", "model-b"]);
    let second = serve(rig.model.clone(), vec![text("still b")]);
    assert_eq!(ended(&mut rig.admit("sticky-2")).await, Ok("still b".to_string()));
    assert_eq!(models(&second.await.unwrap()), ["model-b"], "the next turn starts on the degraded route");
    rig.cfg["harness"]["routes"][1]["maxOutputTokens"] = json!(1000);
    let third = serve(rig.model.clone(), vec![text("from a")]);
    assert_eq!(ended(&mut rig.admit("sticky-3")).await, Ok("from a".to_string()));
    assert_eq!(models(&third.await.unwrap()), ["model-a"], "a changed chain starts on its first route");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_restart_starts_on_the_first_route() {
    let rig = rig("restart", two_routes(), None).await;
    let first = serve(rig.model.clone(), vec![status(500, ""), text("from b")]);
    assert_eq!(ended(&mut rig.admit("restart-1")).await, Ok("from b".to_string()));
    assert_eq!(models(&first.await.unwrap()), ["model-a", "model-b"]);
    // A new process has an empty route memory.
    routes::with_memory(&routes::scope_of(&rig.context), |memory| *memory = routes::RouteMemory::default());
    let second = serve(rig.model.clone(), vec![text("from a")]);
    assert_eq!(ended(&mut rig.admit("restart-2")).await, Ok("from a".to_string()));
    assert_eq!(models(&second.await.unwrap()), ["model-a"]);
    assert_eq!(
        rig.attempts(),
        [
            attempt("a", "model-a", "failed"),
            attempt("b", "model-b", "completed"),
            attempt("a", "model-a", "completed"),
        ],
        "the durable attempts keep each route"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_two_failovers_spend_the_conversation_budget_across_turns() {
    let chain = json!({"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"},
        {"id":"c","model":"model-c"},{"id":"d","model":"model-d"}]});
    let rig = rig("cap", chain, None).await;
    let first = serve(rig.model.clone(), vec![status(500, ""), status(500, ""), status(500, "")]);
    let mut turn = rig.admit("cap-1");
    assert!(ended(&mut turn).await.is_err());
    assert_eq!(models(&first.await.unwrap()), ["model-a", "model-b", "model-c"], "route d is past the cap of two");
    assert_eq!(turn.route_failures().len(), 3, "each failed route is reported");
    assert_eq!(rig.records("route-failed").len(), 1);
    let second = serve(rig.model.clone(), vec![status(500, "")]);
    assert!(ended(&mut rig.admit("cap-2")).await.is_err());
    assert_eq!(
        models(&second.await.unwrap()),
        ["model-c"],
        "the next turn starts on the degraded route and the spent budget holds until a turn completes"
    );
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_plain_429_retries_the_same_route_after_its_wait_hint() {
    let rig = rig("rate-limit", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, "retry-after-ms: 40\r\n"), text("after the wait")]);
    let begun = std::time::Instant::now();
    let mut turn = rig.admit("rate-limit-1");
    assert_eq!(ended(&mut turn).await, Ok("after the wait".to_string()));
    assert!(begun.elapsed() >= Duration::from_millis(40), "the wait hint was honoured");
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a"]);
    assert!(turn.route_failures().is_empty(), "a retried rate limit is not a route failure");
    let retries = rig.records("route-retry");
    assert!(
        retries.len() == 1 && retries[0].contains("route a") && retries[0].contains("after 40 ms"),
        "{retries:?}"
    );
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_repeat_429_on_the_unchanged_request_moves_to_the_next_route() {
    let rig = rig("repeat-429", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, ""), status(429, ""), text("from b")]);
    let mut turn = rig.admit("repeat-429-1");
    assert_eq!(ended(&mut turn).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a", "model-b"]);
    let failures = turn.route_failures();
    assert!(failures.len() == 1 && failures[0].route == "a" && class(&failures[0].summary) == "quota", "{failures:?}");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_quota_on_every_route_ends_the_turn_after_two_requests_each() {
    let rig = rig("all-quota", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, ""), status(429, ""), status(429, ""), status(429, "")]);
    let mut turn = rig.admit("all-quota-1");
    let result = ended(&mut turn).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "quota"), "{result:?}");
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a", "model-b", "model-b"]);
    let routes: Vec<String> = turn.route_failures().into_iter().map(|failure| failure.route).collect();
    assert_eq!(routes, ["a", "b"]);
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_retry429_off_or_a_long_wait_hint_moves_on_at_the_first_429() {
    let off = json!({"routes":[{"id":"a","model":"model-a","retry429":false},{"id":"b","model":"model-b"}]});
    let rig_off = rig("retry429-off", off, None).await;
    let server = serve(rig_off.model.clone(), vec![status(429, ""), text("from b")]);
    assert_eq!(ended(&mut rig_off.admit("retry429-off-1")).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
    let rig_long = rig("long-hint", two_routes(), None).await;
    let server = serve(rig_long.model.clone(), vec![status(429, "retry-after: 31\r\n"), text("from b")]);
    let begun = std::time::Instant::now();
    assert_eq!(ended(&mut rig_long.admit("long-hint-1")).await, Ok("from b".to_string()));
    assert!(begun.elapsed() < Duration::from_secs(10), "a hint over 30 s is not waited for");
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_empty_answers_retry_three_times_then_fail_over() {
    let rig = rig("empty", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![empty(), empty(), empty(), empty(), text("from b")]);
    let mut turn = rig.admit("empty-1");
    assert_eq!(ended(&mut turn).await, Ok("from b".to_string()));
    assert_eq!(
        models(&server.await.unwrap()),
        ["model-a", "model-a", "model-a", "model-a", "model-b"],
        "three retries on the same route, then the next route"
    );
    let failures = turn.route_failures();
    assert!(failures.len() == 1 && class(&failures[0].summary) == "empty-response", "{failures:?}");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_one_empty_answer_retried_completes_on_the_same_route() {
    let rig = rig("empty-once", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![empty(), text("second try")]);
    let mut turn = rig.admit("empty-once-1");
    assert_eq!(ended(&mut turn).await, Ok("second try".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a"]);
    assert!(turn.route_failures().is_empty());
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_successful_send_stops_failover() {
    let console = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let rig = rig("sent", two_routes(), Some(console.local_addr().unwrap().port())).await;
    let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
    let console_task = serve(console.clone(), vec![ok.to_string()]);
    let server = serve(rig.model.clone(), vec![send_call("send-7"), status(500, "")]);
    let mut turn = rig.admit("sent-1");
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a"]);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), rig.model.accept())
            .await
            .is_err(),
        "no other route after a send"
    );
    assert!(ended(&mut turn).await.is_err());
    assert!(turn.sent_tool_reply(), "the send went out");
    assert_eq!(console_task.await.unwrap()[0]["message"], "send-7");
    assert_eq!(turn.route_failures().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_every_request_counts_against_max_steps() {
    let chain = json!({"maxSteps":2,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"},{"id":"c","model":"model-c"}]});
    let rig = rig("max-steps", chain, None).await;
    let server = serve(rig.model.clone(), vec![status(500, ""), status(500, "")]);
    assert!(ended(&mut rig.admit("max-steps-1")).await.is_err());
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"], "a failover is a step");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_spent_token_budget_stops_retries() {
    let at = |tokens: u64| json!({"maxTokens":tokens,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let spent = rig("budget-spent", at(15), None).await;
    let server = serve(spent.model.clone(), vec![empty()]);
    let result = ended(&mut spent.admit("budget-spent-1")).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "empty-response"), "{result:?}");
    assert_eq!(models(&server.await.unwrap()), ["model-a"], "the empty answer spent the whole budget");
    spent.no_more_requests().await;
    let left = rig("budget-left", at(16), None).await;
    let server = serve(left.model.clone(), vec![empty(), text("over budget")]);
    let result = ended(&mut left.admit("budget-left-1")).await;
    assert_eq!(result, Err("harness token limit exceeded".to_string()));
    let requests = server.await.unwrap();
    assert_eq!(models(&requests), ["model-a", "model-a"], "one token left allows the retry");
    assert_eq!(requests[1]["max_tokens"], 1, "the retry asks for the token left");
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_send_each_routes_effort_and_output_cap() {
    let chain = json!({"routes":[{"id":"a","model":"model-a","reasoningEffort":"high","maxOutputTokens":1000}]});
    let rig = rig("wire", chain, None).await;
    let server = serve(rig.model.clone(), vec![text("ok")]);
    assert_eq!(ended(&mut rig.admit("wire-1")).await, Ok("ok".to_string()));
    let requests = server.await.unwrap();
    let request = &requests[0];
    assert_eq!(request["reasoning_effort"], "high");
    assert_eq!(request["max_tokens"], 1000);
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_a_config_without_routes_keeps_its_one_route() {
    let rig = rig("legacy", json!({"model":"legacy-model"}), None).await;
    let server = serve(rig.model.clone(), vec![text("ok")]);
    assert_eq!(ended(&mut rig.admit("legacy-1")).await, Ok("ok".to_string()));
    let requests = server.await.unwrap();
    let request = &requests[0];
    assert_eq!(request["model"], "legacy-model");
    assert_eq!(request["max_tokens"], 4096);
    assert!(request.get("reasoning_effort").is_none(), "no effort is sent: {request}");
    assert_eq!(rig.attempts(), [attempt("legacy-model", "legacy-model", "completed")]);
}

/// A stop on `length`: the answer ran out of output. It reports 15 tokens of usage, as the other answers do.
fn length() -> String {
    sse(&[stop("length")])
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_unsafe_stop_is_charged_before_the_failover_asks_for_what_is_left() {
    let chain = json!({"maxTokens":20,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let rig = rig("unsafe-left", chain, None).await;
    let server = serve(rig.model.clone(), vec![length(), length()]);
    let mut turn = rig.admit("unsafe-left-1");
    assert!(ended(&mut turn).await.is_err());
    let requests = server.await.unwrap();
    assert_eq!(models(&requests), ["model-a", "model-b"], "a stop on length fails over");
    assert_eq!(requests[1]["max_tokens"], 5, "route a's 15 tokens are charged: route b asks for the 5 left");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_unsafe_stop_that_spends_the_budget_does_not_fail_over() {
    let chain = json!({"maxTokens":15,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let rig = rig("unsafe-spent", chain, None).await;
    let server = serve(rig.model.clone(), vec![length()]);
    let result = ended(&mut rig.admit("unsafe-spent-1")).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "backend"), "{result:?}");
    assert_eq!(models(&server.await.unwrap()), ["model-a"], "the stop spent the whole budget: no route b request");
    rig.no_more_requests().await;
}

/// A provider content-filter stop: the refused flag is set on its frame, and the stream reports 15 tokens of usage.
fn content_filter() -> String {
    sse(&[stop("content_filter")])
}

/// A refusal in `delta.refusal`, then a normal stop with 15 tokens of usage.
fn refusal_delta() -> String {
    sse(&[
        json!({"id":"fake-7","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{"refusal":"declined"},"finish_reason":null}]}),
        stop("stop"),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_unsafe_stop_refusal_is_charged_before_the_failover_asks_for_what_is_left() {
    let chain = json!({"maxTokens":20,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let rig = rig("unsafe-refusal-left", chain, None).await;
    let server = serve(rig.model.clone(), vec![content_filter(), content_filter()]);
    let result = ended(&mut rig.admit("unsafe-refusal-left-1")).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "refusal"), "{result:?}");
    let requests = server.await.unwrap();
    assert_eq!(models(&requests), ["model-a", "model-b"], "a refusal fails over");
    assert_eq!(requests[1]["max_tokens"], 5, "route a's 15 tokens are charged: route b asks for the 5 left");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_unsafe_stop_refusal_that_spends_the_budget_does_not_fail_over() {
    let chain = json!({"maxTokens":15,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let rig = rig("unsafe-refusal-spent", chain, None).await;
    let server = serve(rig.model.clone(), vec![content_filter()]);
    let result = ended(&mut rig.admit("unsafe-refusal-spent-1")).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "refusal"), "{result:?}");
    assert_eq!(models(&server.await.unwrap()), ["model-a"], "the refusal spent the whole budget: no route b request");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_routes_an_unsafe_stop_refusal_delta_is_charged_before_the_failover_asks_for_what_is_left() {
    let chain = json!({"maxTokens":20,"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}]});
    let rig = rig("unsafe-delta-left", chain, None).await;
    let server = serve(rig.model.clone(), vec![refusal_delta(), refusal_delta()]);
    let result = ended(&mut rig.admit("unsafe-delta-left-1")).await;
    assert!(result.as_ref().is_err_and(|reason| class(reason) == "refusal"), "{result:?}");
    let requests = server.await.unwrap();
    assert_eq!(models(&requests), ["model-a", "model-b"], "a refused delta fails over");
    assert_eq!(requests[1]["max_tokens"], 5, "route a's 15 tokens are charged: route b asks for the 5 left");
    rig.no_more_requests().await;
}

/// #173: the token-ledger rows the turns' usage facts wrote, in order.
fn usage_rows(rig: &Rig) -> Vec<Value> {
    std::fs::read_to_string(rig.root.join("data/token-usage-rust.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Each row's (route, model, step, retry, buckets).
fn usage_places(rows: &[Value]) -> Vec<(String, String, i64, i64, Value)> {
    rows.iter()
        .map(|row| {
            (
                row["route"].as_str().unwrap_or("").to_string(),
                row["model"].as_str().unwrap_or("").to_string(),
                row["step"].as_i64().unwrap_or(-1),
                row["retry"].as_i64().unwrap_or(-1),
                row["b"].clone(),
            )
        })
        .collect()
}

/// A place with the buckets of the 15 tokens every scripted answer reports (`stop`).
fn usage_place(route: &str, model: &str, step: i64, retry: i64) -> (String, String, i64, i64, Value) {
    (
        route.into(),
        model.into(),
        step,
        retry,
        json!({"cacheMiss": 12, "cacheHit": 0, "cacheWrite": 0, "output": 3, "reasoning": 0}),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn harness_usage_each_answered_attempt_is_one_sample_and_the_console_totals_include_it() {
    let rig = rig("usage-attempts", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(500, ""), empty(), text("from b")]);
    let mut turn = rig.admit("usage-attempts-1");
    assert_eq!(ended(&mut turn).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b", "model-b"]);
    let rows = usage_rows(&rig);
    assert_eq!(
        usage_places(&rows),
        [usage_place("b", "model-b", 1, 1), usage_place("b", "model-b", 2, 2)],
        "the 500 brought no response end and records nothing; the empty answer and its retry are one sample each"
    );
    for row in &rows {
        assert_eq!(row["sid"], json!(usage::session("private:8808")), "{row}");
        assert_eq!(
            (row["k"].as_str(), row["key"].as_str(), row["job"].as_str(), row["engine"].as_str()),
            (Some("turn"), Some("private:8808"), Some("turn"), Some("harness"))
        );
    }
    assert_eq!(rows[0]["turn"], rows[1]["turn"], "one turn number for the whole turn");
    let ledger = crate::ledger::Ledger::load_all_for_runtime(&rig.context).unwrap();
    let summary = ledger.summary(&crate::pricing::resolve(None), 50);
    assert_eq!(summary["conversations"][0]["key"], "private:8808");
    assert_eq!(
        (summary["totals"]["turns"].as_i64(), summary["totals"]["steps"].as_i64(), summary["totals"]["totalTokens"].as_f64()),
        (Some(1), Some(2), Some(30.0)),
        "the console totals count the turn, both answered attempts and their tokens"
    );
    assert_eq!(ledger.dsh_comparable_samples().count(), 0, "harness usage is never compared with DSH's own total");
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn harness_usage_a_refused_answer_is_recorded_before_its_failover() {
    let rig = rig("usage-refused", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![content_filter(), text("from b")]);
    assert_eq!(ended(&mut rig.admit("usage-refused-1")).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
    assert_eq!(
        usage_places(&usage_rows(&rig)),
        [usage_place("a", "model-a", 0, 0), usage_place("b", "model-b", 1, 1)],
        "the refused answer was paid for: it is a sample of route a"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn harness_usage_each_turn_has_its_own_later_number() {
    let rig = rig("usage-turns", two_routes(), None).await;
    let first = serve(rig.model.clone(), vec![text("one")]);
    assert_eq!(ended(&mut rig.admit("usage-turns-1")).await, Ok("one".to_string()));
    first.await.unwrap();
    let second = serve(rig.model.clone(), vec![text("two")]);
    assert_eq!(ended(&mut rig.admit("usage-turns-2")).await, Ok("two".to_string()));
    second.await.unwrap();
    let rows = usage_rows(&rig);
    assert_eq!(usage_places(&rows), [usage_place("a", "model-a", 0, 0), usage_place("a", "model-a", 0, 0)]);
    assert!(rows[0]["turn"].as_i64() < rows[1]["turn"].as_i64(), "{rows:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn harness_usage_an_answer_past_the_token_budget_is_still_recorded() {
    let chain = json!({"maxTokens": 10, "routes": [{"id": "a", "model": "model-a"}]});
    let rig = rig("usage-budget", chain, None).await;
    let server = serve(rig.model.clone(), vec![text("over budget")]);
    assert_eq!(ended(&mut rig.admit("usage-budget-1")).await, Err("harness token limit exceeded".to_string()));
    server.await.unwrap();
    assert_eq!(usage_places(&usage_rows(&rig)), [usage_place("a", "model-a", 0, 0)], "the tokens were spent");
}

#[tokio::test(flavor = "current_thread")]
async fn harness_usage_requests_without_a_response_end_record_nothing() {
    let rig = rig("usage-quota", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, ""), status(429, ""), status(429, ""), status(429, "")]);
    assert!(ended(&mut rig.admit("usage-quota-1")).await.is_err());
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a", "model-b", "model-b"]);
    assert!(!rig.root.join("data/token-usage-rust.jsonl").exists(), "a 429 has no usage to record");
}

/// #174: the health check's view of the rig's chain (`route_health::report`): each route as `id=state`, and whether
/// every route has failed.
fn health(rig: &Rig) -> (Vec<String>, bool) {
    let report = route_health::report(&rig.context, &rig.cfg).unwrap();
    let states = report["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| format!("{}={}", route["id"].as_str().unwrap(), route["state"].as_str().unwrap()))
        .collect();
    (states, report["allFailed"] == true)
}

fn states(list: &[&str]) -> Vec<String> {
    list.iter().map(|state| state.to_string()).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn route_health_a_failover_marks_the_failed_route_and_the_answering_route() {
    let rig = rig("health-failover", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(500, ""), text("from b")]);
    assert_eq!(ended(&mut rig.admit("health-failover-1")).await, Ok("from b".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-b"]);
    assert_eq!(health(&rig), (states(&["a=failed", "b=ok"]), false));
    assert_eq!(route_health::report(&rig.context, &rig.cfg).unwrap()["routes"][0]["class"], "backend");
}

#[tokio::test(flavor = "current_thread")]
async fn route_health_a_retried_rate_limit_is_no_route_failure() {
    let rig = rig("health-retry", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, "retry-after-ms: 1\r\n"), text("after the wait")]);
    assert_eq!(ended(&mut rig.admit("health-retry-1")).await, Ok("after the wait".to_string()));
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a"]);
    assert_eq!(health(&rig), (states(&["a=ok", "b=unknown"]), false));
}

#[tokio::test(flavor = "current_thread")]
async fn route_health_quota_on_every_route_starts_an_episode_that_the_next_answer_ends() {
    let rig = rig("health-all-failed", two_routes(), None).await;
    let server = serve(rig.model.clone(), vec![status(429, ""), status(429, ""), status(429, ""), status(429, "")]);
    assert!(ended(&mut rig.admit("health-all-failed-1")).await.is_err());
    assert_eq!(models(&server.await.unwrap()), ["model-a", "model-a", "model-b", "model-b"]);
    assert_eq!(health(&rig), (states(&["a=failed", "b=failed"]), true));
    let next = serve(rig.model.clone(), vec![text("back")]);
    assert_eq!(ended(&mut rig.admit("health-all-failed-2")).await, Ok("back".to_string()));
    assert_eq!(models(&next.await.unwrap()), ["model-b"], "the next turn starts on the degraded route");
    assert_eq!(health(&rig), (states(&["a=failed", "b=ok"]), false), "an answer ends the episode");
}

#[tokio::test(flavor = "current_thread")]
async fn route_health_a_retry_that_never_answers_is_no_route_failure() {
    let rig = rig("health-stalled-retry", two_routes(), None).await;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let listener = rig.model.clone();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_body(&mut first).await;
        first.write_all(status(429, "retry-after-ms: 1\r\n").as_bytes()).await.unwrap();
        let _ = first.shutdown().await;
        let (mut second, _) = listener.accept().await.unwrap();
        read_body(&mut second).await;
        let _ = reached_tx.send(());
        // The retried request is never answered: the connection stays open until the test drops the server.
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(second);
    });
    let mut turn = rig.admit("health-stalled-retry-1");
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("the retry reached the model")
        .unwrap();
    assert_eq!(health(&rig), (states(&["a=unknown", "b=unknown"]), false), "a retried miss is no route failure");
    assert!(turn.route_failures().is_empty());
    turn.cancel();
    assert!(ended(&mut turn).await.is_err(), "the cancelled turn ends");
    server.abort();
    assert_eq!(health(&rig), (states(&["a=unknown", "b=unknown"]), false), "a cancelled turn is no route failure");
}

/// #162: routes a, b and c, with `harness.firstRoute` set to `first` in the configuration.
fn three_routes_first(first: &str) -> Value {
    json!({"firstRoute": first, "routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"},{"id":"c","model":"model-c"}]})
}

#[tokio::test(flavor = "current_thread")]
async fn model_route_health_keys_on_the_configured_chain_whatever_order_a_turn_walks() {
    let rig = rig("route-health-walk", three_routes_first("b"), None).await;
    let server = serve(rig.model.clone(), vec![status(500, ""), text("from a")]);
    assert_eq!(ended(&mut rig.admit("route-health-walk-1")).await, Ok("from a".to_string()));
    let requests = server.await.unwrap();
    assert_eq!(
        health(&rig),
        (states(&["a=ok", "b=failed", "c=unknown"]), false),
        "b is walked first; route health names each route of the configured chain"
    );
    assert_eq!(
        models(&requests),
        ["model-b", "model-a"],
        "harness.firstRoute first, then the other routes in configured order"
    );
    // The next turn starts on the degraded route a and fails on a and c: with b failed before, every route of the
    // configured chain has failed, and the outage episode is the configured chain's.
    let second = serve(rig.model.clone(), vec![status(500, ""), status(500, "")]);
    assert!(ended(&mut rig.admit("route-health-walk-2")).await.is_err());
    assert_eq!(models(&second.await.unwrap()), ["model-a", "model-c"]);
    assert_eq!(
        health(&rig),
        (states(&["a=failed", "b=failed", "c=failed"]), true),
        "every route failed: one episode of the configured chain"
    );
    rig.no_more_requests().await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_route_a_conversations_own_route_starts_its_next_turn_and_a_reset_restores_the_configured_order() {
    let rig = rig("own-route", three_routes_first("b"), None).await;
    let chosen = crate::settings::apply_conversation(
        &rig.context, "private:8808", "conversation.route", &json!("c"), "administrator", "console",
    );
    assert_eq!(chosen.map(|applied| applied.changed), Ok(true), "the route is stored for the conversation");
    assert_eq!(
        routes::selected(&rig.context, &rig.cfg, "private:8808").as_deref(),
        Some("c"),
        "the conversation's own route wins over harness.firstRoute"
    );
    let first = serve(rig.model.clone(), vec![status(500, ""), status(500, ""), text("from b")]);
    assert_eq!(ended(&mut rig.admit("own-route-1")).await, Ok("from b".to_string()));
    assert_eq!(
        models(&first.await.unwrap()),
        ["model-c", "model-a", "model-b"],
        "its own route first, then the other routes in configured order"
    );
    // A reset stores none for this conversation: its next turn walks the configured order whatever harness.firstRoute
    // says, and starts on that order's first route, not on route b, where the other order's failover memory stands.
    let reset = crate::settings::reset_conversation(&rig.context, "private:8808", "conversation.route", "administrator");
    assert_eq!(reset.map(|applied| applied.changed), Ok(true), "the reset is stored");
    assert_eq!(routes::selected(&rig.context, &rig.cfg, "private:8808"), None, "a reset stores none");
    let second = serve(rig.model.clone(), vec![text("from a")]);
    assert_eq!(ended(&mut rig.admit("own-route-2")).await, Ok("from a".to_string()));
    assert_eq!(models(&second.await.unwrap()), ["model-a"], "the configured order from the next turn");
    rig.no_more_requests().await;
}
