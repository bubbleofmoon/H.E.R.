//! #238: the digest's route walk (`RouteWalk`) on a scripted loopback relay; no real model service is contacted.
use super::*;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Mutex;

/// The harness credential of every fixture; one test puts it into a route id and a model.
const SECRET: &str = "synthetic-model-key-238";

/// What a scripted reply sends: a completion with `content` (`Answer`), a content-filter stop (`Refusal`) or an HTTP
/// failure with extra header lines, each ending in CRLF (`Status`).
#[derive(Clone)]
enum Kind {
    Answer(&'static str),
    Refusal,
    Status(u16, &'static str),
}

/// One scripted reply, sent after `delay`.
#[derive(Clone)]
struct Reply {
    delay: Duration,
    kind: Kind,
}

fn at_once(kind: Kind) -> Reply {
    Reply { delay: Duration::ZERO, kind }
}

fn after(millis: u64, kind: Kind) -> Reply {
    Reply { delay: Duration::from_millis(millis), kind }
}

type Queues = Mutex<HashMap<String, VecDeque<Reply>>>;

/// A relay on a loopback port. Each request is answered on its own thread by the next reply scripted for the model it
/// asks (an unscripted request gets a 500), and the models asked are kept in arrival order. It accepts for 20 s.
struct Relay {
    port: u16,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Relay {
    fn start(script: Vec<(&str, Reply)>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut queues: HashMap<String, VecDeque<Reply>> = HashMap::new();
        for (model, reply) in script {
            queues.entry(model.to_string()).or_default().push_back(reply);
        }
        let queues = Arc::new(Mutex::new(queues));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(20);
            while Instant::now() < end {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (queues, log) = (queues.clone(), log.clone());
                        std::thread::spawn(move || answer(stream, &queues, &log));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self { port, asked }
    }

    /// The models asked so far, in arrival order.
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// Reads one request, notes the model it asks and writes the reply scripted for that model.
fn answer(mut stream: std::net::TcpStream, queues: &Queues, log: &Mutex<Vec<String>>) {
    // An accepted socket may inherit the listener's non-blocking mode: restore blocking before the read timeout.
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let body = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..n]);
        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length]).unwrap_or(Value::Null);
        }
    };
    let model = body["model"].as_str().unwrap_or("").to_string();
    let reply = queues
        .lock()
        .unwrap()
        .get_mut(&model)
        .and_then(VecDeque::pop_front)
        .unwrap_or(at_once(Kind::Status(500, "")));
    log.lock().unwrap().push(model);
    std::thread::sleep(reply.delay);
    let completion = |content: &str, finish: &str| {
        json!({"id": "fixture-238", "object": "chat.completion", "created": 1, "model": "fixture",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}})
        .to_string()
    };
    let (status, headers, text) = match reply.kind {
        Kind::Answer(content) => (200, "", completion(content, "stop")),
        Kind::Refusal => (200, "", completion("", "content_filter")),
        Kind::Status(code, headers) => {
            (code, headers, json!({"error": {"message": "fixture failure", "type": "fixture"}}).to_string())
        }
    };
    // The walk may have given up on this request already (its timeout): a failed write is fine.
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

struct Fixture {
    root: std::path::PathBuf,
    context: RuntimeContext,
    cfg: Value,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// A selected runtime whose model relay is `relay`, with the members of `extra` in its configuration.
    fn new(tag: &str, relay: &Relay, extra: Value) -> Self {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-238-{tag}-{}", crate::turn_ledger::mint_id(""))),
        );
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut cfg = json!({"consoleToken": "synthetic-console-token-238"});
        cfg.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("config/harness-api-key"), SECRET).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        let dependencies = json!({"dsh": "http://127.0.0.1:1", "model": format!("http://127.0.0.1:{}", relay.port),
            "onebot": "http://127.0.0.1:1"});
        std::fs::write(root.join("dependencies.json"), dependencies.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        Self { root, context, cfg }
    }

    /// The digest's walk over the configured chain, bounded by `timeout`, with retries waiting a millisecond.
    fn walk(&self, timeout: Duration) -> RouteWalk {
        let walk = Walk { routes: routes::chain(&self.cfg).unwrap(), chain: true };
        RouteWalk::new(&self.context, Job::Digest, usage::DIGEST_KEY, walk, timeout, 64)
            .unwrap()
            .with_unit(Duration::from_millis(1))
    }

    /// Each usage fact as (route, retry, turn), in ledger order.
    fn usage(&self) -> Vec<(String, i64, i64)> {
        std::fs::read_to_string(self.root.join("data/token-usage-rust.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|row| {
                let route = row["route"].as_str().unwrap_or("").to_string();
                (route, row["retry"].as_i64().unwrap_or(-1), row["turn"].as_i64().unwrap_or(-1))
            })
            .collect()
    }

    /// Each route of the configured chain as `<id>=<state>` (`route_health::report`).
    fn health(&self) -> Vec<String> {
        let report = route_health::report(&self.context, &self.cfg).unwrap();
        report["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|route| format!("{}={}", route["id"].as_str().unwrap(), route["state"].as_str().unwrap()))
            .collect()
    }

    /// The reasons of the run-record entries named `event`.
    fn records(&self, event: &str) -> Vec<String> {
        crate::recovery_log::page(&self.context, &crate::run_record::Query::default())
            .unwrap()
            .records
            .into_iter()
            .filter(|record| record.event == event)
            .map(|record| record.reason)
            .collect()
    }
}

/// A configuration whose chain is one route per id, each asking `model-<id>`.
fn chain_of(ids: &[&str]) -> Value {
    let routes: Vec<Value> = ids.iter().map(|id| json!({"id": id, "model": format!("model-{id}")})).collect();
    json!({"harness": {"routes": routes}})
}

/// (route, retry) of each usage fact.
fn places(usage: &[(String, i64, i64)]) -> Vec<(&str, i64)> {
    usage.iter().map(|(route, retry, _)| (route.as_str(), *retry)).collect()
}

#[test]
fn digest_walk_moves_to_the_next_route_after_an_explicit_error() {
    let relay = Relay::start(vec![("model-a", at_once(Kind::Status(500, ""))), ("model-b", at_once(Kind::Answer("[]")))]);
    let fixture = Fixture::new("explicit", &relay, chain_of(&["a", "b"]));
    let model = crate::digest::model_for(&fixture.context, &fixture.cfg).unwrap();
    assert_eq!(model.complete("system", "input"), Ok("[]".to_string()), "the digest's model port walks the chain");
    assert_eq!(relay.asked(), ["model-a", "model-b"], "the next route after a 500, and nothing after the answer");
    assert_eq!(places(&fixture.usage()), [("b", 1)], "a 500 has no response end; the answer is the walk's second request");
    assert_eq!(fixture.health(), ["a=failed", "b=ok"]);
    let failovers = fixture.records("failover-step");
    assert!(
        failovers.len() == 1 && failovers[0].contains("internal:digest: route a (backend)") && failovers[0].contains("next route b"),
        "{failovers:?}"
    );
}

#[test]
fn digest_walk_a_refusal_moves_on_and_each_response_end_is_a_usage_fact() {
    let relay = Relay::start(vec![("model-a", at_once(Kind::Refusal)), ("model-b", at_once(Kind::Answer("[]")))]);
    let fixture = Fixture::new("refusal", &relay, chain_of(&["a", "b"]));
    assert_eq!(fixture.walk(Duration::from_secs(10)).complete("system", "input"), Ok("[]".to_string()));
    assert_eq!(relay.asked(), ["model-a", "model-b"]);
    let usage = fixture.usage();
    assert_eq!(places(&usage), [("a", 0), ("b", 1)], "each response end is a usage fact on its route with its retry count");
    assert_eq!(usage[0].2, usage[1].2, "one turn number per walk");
    assert_eq!(fixture.health(), ["a=unknown", "b=ok"], "a refusal is no route failure");
}

#[test]
fn digest_walk_a_first_429_waits_its_hint_on_the_same_route_and_a_repeat_429_moves_on() {
    let limited = || at_once(Kind::Status(429, "retry-after-ms: 1\r\n"));
    let relay = Relay::start(vec![("model-a", limited()), ("model-a", limited()), ("model-b", at_once(Kind::Answer("[]")))]);
    let fixture = Fixture::new("rate-limit", &relay, chain_of(&["a", "b"]));
    assert_eq!(fixture.walk(Duration::from_secs(10)).complete("system", "input"), Ok("[]".to_string()));
    assert_eq!(relay.asked(), ["model-a", "model-a", "model-b"], "a first 429 is retried on the same route, a repeat moves on");
    let retries = fixture.records("route-retry");
    assert!(retries.len() == 1 && retries[0].contains("route a (quota)") && retries[0].contains("after 1 ms"), "{retries:?}");
    assert_eq!(fixture.health(), ["a=failed", "b=ok"]);
}

#[test]
fn digest_walk_empty_answers_retry_three_times_on_the_same_route_then_move_on() {
    let empty = || at_once(Kind::Answer(""));
    let relay = Relay::start(vec![
        ("model-a", empty()),
        ("model-a", empty()),
        ("model-a", empty()),
        ("model-a", empty()),
        ("model-b", at_once(Kind::Answer("[]"))),
    ]);
    let fixture = Fixture::new("empty", &relay, chain_of(&["a", "b"]));
    assert_eq!(fixture.walk(Duration::from_secs(10)).complete("system", "input"), Ok("[]".to_string()));
    assert_eq!(
        relay.asked(),
        ["model-a", "model-a", "model-a", "model-a", "model-b"],
        "three retries of an empty answer, then the next route"
    );
    assert_eq!(fixture.usage().iter().map(|(_, retry, _)| *retry).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
    assert_eq!(fixture.health(), ["a=failed", "b=ok"]);
    // With no route left, the empty answers end the walk with their class.
    let relay = Relay::start(vec![("model-a", empty()), ("model-a", empty()), ("model-a", empty()), ("model-a", empty())]);
    let fixture = Fixture::new("empty-last", &relay, chain_of(&["a"]));
    assert_eq!(fixture.walk(Duration::from_secs(10)).complete("system", "input"), Err(ModelError("empty-response")));
    assert_eq!(relay.asked().len(), 4);
}

#[test]
fn digest_walk_stays_within_the_digest_timeout() {
    // The first route fails late and the second never answers: the second gets only the time left.
    let relay = Relay::start(vec![("model-a", after(800, Kind::Status(500, ""))), ("model-b", after(5000, Kind::Answer("[]")))]);
    let fixture = Fixture::new("timeout", &relay, chain_of(&["a", "b"]));
    let begun = Instant::now();
    assert_eq!(fixture.walk(Duration::from_millis(1000)).complete("system", "input"), Err(ModelError("timeout")));
    let took = begun.elapsed();
    assert!(took < Duration::from_millis(1400), "the walk took {took:?}, past its 1000 ms bound");
    assert_eq!(relay.asked(), ["model-a", "model-b"]);
    assert_eq!(fixture.health(), ["a=failed", "b=failed"]);
    // A route that never answers spends the whole bound: no later route is asked.
    let relay = Relay::start(vec![("model-a", after(5000, Kind::Answer("[]"))), ("model-b", at_once(Kind::Answer("[]")))]);
    let fixture = Fixture::new("timeout-first", &relay, chain_of(&["a", "b"]));
    assert_eq!(fixture.walk(Duration::from_millis(500)).complete("system", "input"), Err(ModelError("timeout")));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(relay.asked(), ["model-a"], "nothing is sent once the bound has passed");
}

#[test]
fn digest_walk_ends_after_two_failovers_with_the_last_failure() {
    let failing = || at_once(Kind::Status(500, ""));
    let relay = Relay::start(vec![
        ("model-a", failing()),
        ("model-b", failing()),
        ("model-c", failing()),
        ("model-d", at_once(Kind::Answer("[]"))),
    ]);
    let fixture = Fixture::new("cap", &relay, chain_of(&["a", "b", "c", "d"]));
    assert_eq!(
        fixture.walk(Duration::from_secs(10)).complete("system", "input"),
        Err(ModelError("backend")),
        "two failovers until a route answers (`routes::RETRY_CAP`)"
    );
    assert_eq!(relay.asked(), ["model-a", "model-b", "model-c"]);
    assert_eq!(fixture.records("failover-step").len(), 2);
    let failed = fixture.records("route-failed");
    assert!(failed.len() == 1 && failed[0].contains("route c (backend)"), "{failed:?}");
}

#[test]
fn digest_walk_a_degraded_route_starts_the_next_walk() {
    let relay = Relay::start(vec![
        ("model-a", at_once(Kind::Status(500, ""))),
        ("model-b", at_once(Kind::Answer("[]"))),
        ("model-b", at_once(Kind::Answer("[]"))),
    ]);
    let fixture = Fixture::new("sticky", &relay, chain_of(&["a", "b"]));
    let walk = fixture.walk(Duration::from_secs(10));
    assert_eq!(walk.complete("system", "first"), Ok("[]".to_string()));
    assert_eq!(walk.complete("system", "second"), Ok("[]".to_string()));
    assert_eq!(
        relay.asked(),
        ["model-a", "model-b", "model-b"],
        "the degraded route starts the next walk, as for a conversation's turns"
    );
}

#[test]
fn digest_walk_a_failure_shaped_answer_moves_on() {
    let relay = Relay::start(vec![
        ("model-a", at_once(Kind::Answer("Error: upstream failed"))),
        ("model-b", at_once(Kind::Answer("[]"))),
    ]);
    let fixture = Fixture::new("failure-text", &relay, chain_of(&["a", "b"]));
    assert_eq!(
        fixture.walk(Duration::from_secs(10)).complete("system", "input"),
        Ok("[]".to_string()),
        "an error printed as an answer is no answer"
    );
    assert_eq!(relay.asked(), ["model-a", "model-b"]);
    assert_eq!(fixture.health(), ["a=failed", "b=ok"]);
}

#[test]
fn digest_walk_the_digest_model_setting_is_one_route_without_failover_or_route_health() {
    let relay = Relay::start(vec![("own-model", at_once(Kind::Status(500, ""))), ("model-a", at_once(Kind::Answer("[]")))]);
    let mut extra = chain_of(&["a", "b"]);
    extra["memory"] = json!({"digest": {"model": "own-model"}});
    let fixture = Fixture::new("own-model", &relay, extra);
    let model = crate::digest::model_for(&fixture.context, &fixture.cfg).unwrap();
    assert_eq!(model.complete("system", "input"), Err(ModelError("backend")));
    assert_eq!(relay.asked(), ["own-model"], "the digest's own model has no other route");
    assert!(
        !fixture.root.join("data").join(route_health::FILE).exists(),
        "the digest's own model is no route of the chain: its failure is no route health"
    );
}

#[test]
fn digest_walk_records_name_routes_without_the_credential() {
    let model_a = format!("m-{SECRET}");
    let relay = Relay::start(vec![(model_a.as_str(), at_once(Kind::Status(500, ""))), ("model-b", at_once(Kind::Answer("[]")))]);
    let extra = json!({"harness": {"routes": [{"id": format!("r-{SECRET}"), "model": model_a}, {"id": "b", "model": "model-b"}]}});
    let fixture = Fixture::new("credential", &relay, extra);
    assert_eq!(fixture.walk(Duration::from_secs(10)).complete("system", "input"), Ok("[]".to_string()));
    let failovers = fixture.records("failover-step");
    assert!(failovers.len() == 1 && failovers[0].contains("route r-[REDACTED] (backend)"), "{failovers:?}");
    let ledger = std::fs::read_to_string(fixture.root.join("data/token-usage-rust.jsonl")).unwrap_or_default();
    assert!(!ledger.is_empty() && !ledger.contains(SECRET), "the ledger never keeps the credential");
}

#[test]
fn digest_walk_concurrent_walks_keep_every_route_health_observation() {
    let relay = Relay::start(vec![
        ("model-a", at_once(Kind::Status(500, ""))),
        ("model-a", at_once(Kind::Status(500, ""))),
        ("model-b", at_once(Kind::Answer("[]"))),
        ("model-b", at_once(Kind::Answer("[]"))),
    ]);
    let fixture = Fixture::new("concurrent", &relay, chain_of(&["a", "b"]));
    let walk = Arc::new(fixture.walk(Duration::from_secs(10)));
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let walk = walk.clone();
            std::thread::spawn(move || walk.complete("system", &format!("input {n}")))
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), Ok("[]".to_string()));
    }
    assert_eq!(fixture.health(), ["a=failed", "b=ok"], "every observation of both walks is kept");
    assert!(fixture.records("route-failed").is_empty());
}
