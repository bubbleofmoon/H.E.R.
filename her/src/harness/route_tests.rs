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
    let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
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
                route: ModelRoute::from_runtime(&self.context, &self.cfg).unwrap(),
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
