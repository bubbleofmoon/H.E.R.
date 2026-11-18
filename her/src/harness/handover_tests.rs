//! #6: the history budget, summaries and idle rotation of harness turns, on scripted loopback model fixtures and a
//! scripted summariser (the digest's model port); no real model service is contacted.
use super::*;
use crate::harness::budget::{text_tokens, MESSAGE_TOKENS};
use crate::harness::handover::{CHAT_LABEL, HANDOVER_TOKENS, PREVIOUS_LABEL, SUMMARY_INPUT_TOKENS, SUMMARY_LABEL, SUMMARY_PROMPT};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHUNK: &str = "chat.completion.chunk";
const OWNER: &str = "private:8808";

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

/// A text answer whose provider reports `prompt_tokens` of input.
fn answer(text: &str, prompt_tokens: u64) -> String {
    sse(&[
        json!({"id":"fake-6","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
        json!({"id":"fake-6","object":CHUNK,"model":"fake","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":prompt_tokens,"completion_tokens":3,"total_tokens":prompt_tokens + 3}}),
    ])
}

/// A failed response.
fn status(code: u16) -> String {
    let body = json!({"error":{"message":"fixture failure","type":"fixture"}}).to_string();
    format!(
        "HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
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

type Answer = Box<dyn Fn(usize) -> Result<String, crate::digest::ModelError> + Send + Sync>;

/// The digest's model port, scripted: each request is recorded as (system, input) and `answer` replies from the
/// request's number (1 for the first). While `hold` is set a request waits, up to 10 s.
struct Summariser {
    seen: std::sync::Mutex<Vec<(String, String)>>,
    answer: Answer,
    hold: AtomicBool,
}

impl crate::digest::DigestModel for Summariser {
    fn complete(&self, system: &str, input: &str) -> Result<String, crate::digest::ModelError> {
        let count = {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            seen.push((system.to_string(), input.to_string()));
            seen.len()
        };
        let begun = std::time::Instant::now();
        while self.hold.load(Ordering::SeqCst) && begun.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        (self.answer)(count)
    }
}

struct Rig {
    root: std::path::PathBuf,
    context: RuntimeContext,
    generation: String,
    cfg: Value,
    model: Arc<TcpListener>,
    summariser: Arc<Summariser>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A selected runtime whose owner's private chat runs closed-agent harness turns with `harness` as its settings and
/// `answer` as its summariser.
async fn rig(
    tag: &str,
    harness: Value,
    answer: impl Fn(usize) -> Result<String, crate::digest::ModelError> + Send + Sync + 'static,
) -> Rig {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-6-{tag}-{}", crate::turn_ledger::mint_id(""))),
    );
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("http://{}", model.local_addr().unwrap());
    let cfg = json!({"ownerQQ":8808, "consoleToken":"synthetic-console-token-6", "harness":harness});
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-6").unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root).unwrap();
    let summariser = Arc::new(Summariser {
        seen: std::sync::Mutex::new(Vec::new()),
        answer: Box::new(answer),
        hold: AtomicBool::new(false),
    });
    crate::digest::install_test_model(context.runtime_root().unwrap(), summariser.clone());
    let token = crate::conversation_authority::ensure_or_rotate(&context, OWNER, false).unwrap();
    let generation = crate::send_outbox::token_epoch(&token);
    Rig {
        root,
        context,
        generation,
        cfg,
        model,
        summariser,
    }
}

impl Rig {
    fn request(&self, id: &str, prompt: &str) -> TurnRequest {
        TurnRequest {
            id: id.into(),
            conversation: OWNER.into(),
            generation: self.generation.clone(),
            prompt: prompt.into(),
            media: Vec::new(),
            profile: "closed-agent".into(),
            route: ModelRoute::from_runtime(&self.context, &self.cfg).unwrap(),
            deadline: Duration::from_secs(20),
            tools: Tools::capture(&self.context, &self.cfg, "closed-agent", OWNER, &self.generation).unwrap(),
            limits: Limits::from_config(&self.cfg).with_retry_unit(Duration::from_millis(1)),
        }
    }

    fn admit(&self, id: &str, prompt: &str) -> Turn {
        Harness::open(&self.context).unwrap().admit(self.request(id, prompt)).unwrap()
    }

    /// One turn that the model answers with `reply`; returns the request the model received.
    async fn turn(&self, id: &str, prompt: &str, reply: String) -> Value {
        let server = serve(self.model.clone(), vec![reply]);
        let mut turn = self.admit(id, prompt);
        let result = tokio::time::timeout(Duration::from_secs(15), turn.wait())
            .await
            .expect("the turn did not end");
        assert!(result.is_ok(), "{id}: {result:?}");
        server.await.unwrap().remove(0)
    }

    /// Completed past turns written straight to storage: prompt `prompt(i)`, answer `好的{i}`, and `raw` as each
    /// answer's provider record.
    fn seed(&self, rounds: std::ops::RangeInclusive<usize>, prompt: impl Fn(usize) -> String, raw: Value) {
        let store = store::Store::open(&self.context).unwrap();
        for i in rounds {
            let request = self.request(&format!("seed-{i}"), &prompt(i));
            store.admit(&request).unwrap();
            let text = format!("好的{i}");
            let event = Event::Assistant {
                text: text.clone(),
                raw: raw.clone(),
                message: ChatMessage::assistant(text.clone()),
            };
            store.finish(&request.id, &Status::Completed(text), Some(&event)).unwrap();
        }
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.root.join("data/conversations.db")).unwrap()
    }

    /// The payload of each event of `kind`, in order.
    fn events(&self, kind: &str) -> Vec<Value> {
        let db = self.db();
        let mut statement = db.prepare("SELECT payload FROM events WHERE kind = ?1 ORDER BY seq").unwrap();
        let rows = statement
            .query_map([kind], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.iter().map(|payload| serde_json::from_str(payload).unwrap()).collect()
    }

    /// The session slept: every ended turn ended `minutes` earlier.
    fn sleep(&self, minutes: i64) {
        self.db()
            .execute(
                "UPDATE turns SET ended_ms = ended_ms - ?1 WHERE ended_ms IS NOT NULL",
                [minutes * 60_000],
            )
            .unwrap();
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

    /// Each summary request so far: (system, input).
    fn summaries(&self) -> Vec<(String, String)> {
        self.summariser.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// A long round's prompt: about 1750 estimated tokens.
fn long_prompt(i: usize) -> String {
    format!("第{i}轮 {}", "y".repeat(7_000))
}

/// The text of each message of a delivered request, in order.
fn texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join(""),
            _ => String::new(),
        })
        .collect()
}

/// The estimated tokens of a delivered text-only request (`budget::estimate` of its messages).
fn request_tokens(body: &Value) -> u64 {
    texts(body).iter().map(|text| MESSAGE_TOKENS + text_tokens(text)).sum()
}

fn contains(texts: &[String], needle: &str) -> bool {
    texts.iter().any(|text| text.contains(needle))
}

/// A window of 40000 on the one `harness.model` route: a budget of 32800, 6000 kept verbatim by a budget checkpoint.
fn window_40k() -> Value {
    json!({"model":"fake-chat","contextTokens":{"fake-chat":40_000}})
}

#[tokio::test(flavor = "current_thread")]
async fn harness_summary_a_history_over_the_budget_is_summarised_and_its_newest_rounds_kept_verbatim() {
    let rig = rig("budget", window_40k(), |_| Ok("甲和乙约好周六一起爬山。".to_string())).await;
    // 18 rounds of about 1764 tokens each and a short request stay within the budget of 32800.
    rig.seed(1..=18, long_prompt, json!({}));
    let within = rig.turn("budget-19", "现在回复", answer("好的", 12)).await;
    assert!(
        rig.events("checkpoint").is_empty() && rig.summaries().is_empty(),
        "no checkpoint within the budget"
    );
    assert!(
        contains(&texts(&within), "第1轮 ") && contains(&texts(&within), "第18轮"),
        "a history within the budget is replayed whole"
    );
    // One more long round puts the history over the budget.
    rig.seed(20..=20, long_prompt, json!({}));
    let body = rig.turn("budget-21", "再回复", answer("好的", 12)).await;
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 1, "one budget checkpoint: {checkpoints:?}");
    assert_eq!(checkpoints[0]["purpose"], "budget");
    assert_eq!(checkpoints[0]["summary"], "甲和乙约好周六一起爬山。");
    let seen = rig.summaries();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, SUMMARY_PROMPT);
    let input = &seen[0].1;
    assert!(input.starts_with(CHAT_LABEL), "no earlier summary");
    assert!(
        input.contains("第16轮") && !input.contains("第17轮"),
        "the newest covered round is summarised; the three rounds kept verbatim are not"
    );
    assert!(!input.contains("第1轮 "), "the oldest covered rounds past the summary input bound are left out");
    assert!(
        text_tokens(input) <= SUMMARY_INPUT_TOKENS + 16,
        "the summary request is bounded: {}",
        text_tokens(input)
    );
    assert!(!input.contains("Tool session"), "the system instruction is never summarised");
    let texts = texts(&body);
    assert_eq!(body["messages"][0]["role"], "system", "the system instruction stays first");
    assert!(texts[1].starts_with(SUMMARY_LABEL) && texts[1].contains("甲和乙约好周六一起爬山。"), "{}", texts[1]);
    for kept in ["第17轮", "第18轮", "现在回复", "第20轮"] {
        assert!(contains(&texts, kept), "{kept} is kept verbatim");
    }
    assert!(!contains(&texts, "第16轮"), "a summarised round is not replayed");
    assert_eq!(texts.last().map(String::as_str), Some("再回复"), "the owner's latest message stays last");
    assert!(request_tokens(&body) <= 32_800, "{}", request_tokens(&body));
    assert_eq!(rig.records("history-checkpoint").len(), 1);
    // The next turn replays the summary and every round after the checkpoint, without asking again.
    let next = rig.turn("budget-22", "又回复", answer("好", 12)).await;
    assert_eq!(rig.summaries().len(), 1, "a history within the budget is not summarised again");
    let texts = self::texts(&next);
    assert!(texts[1].starts_with(SUMMARY_LABEL));
    assert!(contains(&texts, "第17轮") && contains(&texts, "再回复"), "the rounds after the checkpoint are replayed");
}

#[tokio::test(flavor = "current_thread")]
async fn harness_summary_a_summary_with_new_ai_wording_is_refused_and_the_oldest_rounds_leave_without_one() {
    let rig = rig("wording", window_40k(), |_| Ok("作为AI，我记得周六爬山。".to_string())).await;
    rig.seed(1..=19, long_prompt, json!({}));
    let body = rig.turn("wording-20", "现在回复", answer("好的", 12)).await;
    assert_eq!(rig.events("checkpoint_failed"), [json!({"purpose":"budget","class":"wording"})]);
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 1, "the older rounds still leave: {checkpoints:?}");
    assert!(checkpoints[0]["summary"].is_null(), "without a summary");
    let texts = texts(&body);
    assert!(
        !contains(&texts, "作为AI") && !texts.iter().any(|text| text.starts_with(SUMMARY_LABEL)),
        "the refused summary never reaches the model"
    );
    assert!(contains(&texts, "第19轮") && !contains(&texts, "第16轮"));
    assert_eq!(rig.records("summary-failed").len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn harness_summary_a_turn_stopped_mid_summary_writes_nothing_and_the_next_turn_summarises_again() {
    let rig = rig("mid-summary", window_40k(), |n| Ok(format!("第{n}次摘要：周六爬山。"))).await;
    rig.seed(1..=19, long_prompt, json!({}));
    let users = rig.events("user").len();
    rig.summariser.hold.store(true, Ordering::SeqCst);
    let mut turn = rig.admit("mid-summary-20", "现在回复");
    let begun = std::time::Instant::now();
    while rig.summaries().is_empty() {
        assert!(begun.elapsed() < Duration::from_secs(10), "the summary was never asked for");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    turn.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("the stopped turn did not end");
    assert!(result.is_err(), "{result:?}");
    assert!(
        rig.events("checkpoint").is_empty() && rig.events("checkpoint_failed").is_empty(),
        "a summary that never finished leaves no checkpoint"
    );
    rig.summariser.hold.store(false, Ordering::SeqCst);
    // A new harness (as after a restart) replays the same history and summarises again.
    let body = rig.turn("mid-summary-21", "再回复", answer("好", 12)).await;
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0]["summary"], "第2次摘要：周六爬山。");
    assert_eq!(rig.events("user").len(), users + 2, "every stored fact stays");
    assert!(texts(&body)[1].starts_with(SUMMARY_LABEL));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_summary_text_only_facts_from_before_tools_are_summarised_on_an_unchanged_schema() {
    let rig = rig("v1", json!({"model":"fake-chat","contextTokens":{"fake-chat":1_000}}), |_| {
        Ok("旧的约定还在。".to_string())
    })
    .await;
    drop(store::Store::open(&rig.context).unwrap());
    let db = rig.db();
    db.execute(
        "INSERT INTO sessions(id,conversation,generation) VALUES('v1-session',?1,?2)",
        rusqlite::params![OWNER, rig.generation],
    )
    .unwrap();
    db.execute(
        "INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms,ended_ms,result)
         VALUES('v1-turn','v1-session',?1,'closed-agent','completed',1,2,'旧的回答')",
        [OWNER],
    )
    .unwrap();
    for (kind, payload) in [("user", json!({"text":"旧的消息"})), ("assistant", json!({"text":"旧的回答","raw":{}}))] {
        db.execute(
            "INSERT INTO events(session_id,turn_id,kind,payload,created_ms) VALUES('v1-session','v1-turn',?1,?2,1)",
            rusqlite::params![kind, payload.to_string()],
        )
        .unwrap();
    }
    rig.seed(1..=1, |i| format!("第{i}轮 {}", "y".repeat(4_000)), json!({}));
    let body = rig.turn("v1-next", "现在回复", answer("好的", 12)).await;
    let seen = rig.summaries();
    assert_eq!(seen.len(), 1, "the history over the budget is summarised");
    assert!(
        seen[0].1.contains("〔对方〕旧的消息") && seen[0].1.contains("〔我〕旧的回答"),
        "old text-only facts are summarised"
    );
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 2, "checkpoints need no schema change");
    assert!(texts(&body)[1].starts_with(SUMMARY_LABEL));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_history_budget_a_failover_to_a_smaller_window_leaves_out_the_oldest_rounds() {
    let harness = json!({"routes":[{"id":"a","model":"model-a"},{"id":"b","model":"model-b"}],
        "contextTokens":{"a":40_000,"b":1_000}});
    let rig = rig("failover", harness, |_| Ok("never asked".to_string())).await;
    // Three rounds of about 370 tokens: within route a's budget, over route b's 820.
    rig.seed(1..=3, |i| format!("第{i}轮 {}", "y".repeat(1_400)), json!({}));
    let server = serve(rig.model.clone(), vec![status(500), answer("from b", 12)]);
    let mut turn = rig.admit("failover-4", "现在回复");
    let result = tokio::time::timeout(Duration::from_secs(15), turn.wait())
        .await
        .expect("the turn did not end");
    assert_eq!(result, Ok("from b".to_string()));
    let requests = server.await.unwrap();
    assert!(contains(&texts(&requests[0]), "第1轮"), "route a's budget holds the whole history");
    let to_b = texts(&requests[1]);
    assert!(!contains(&to_b, "第1轮"), "route b's budget leaves out the oldest round");
    assert_eq!(to_b.last().map(String::as_str), Some("现在回复"), "the owner's latest message stays");
    assert!(request_tokens(&requests[1]) <= 820, "{}", request_tokens(&requests[1]));
    assert!(rig.events("checkpoint").is_empty(), "a request's own budget writes no checkpoint");
    assert!(rig.summaries().is_empty());
}

/// Rotation settings: a cache life of 5 minutes and a cap of 20000 input tokens on the `harness.model` route.
fn rotating(extra: Value) -> Value {
    let mut harness = json!({"model":"fake-chat","cacheTtlMinutes":{"fake-chat":5},"rotation":{"caps":{"fake-chat":20_000}}});
    for (key, value) in extra.as_object().unwrap() {
        harness[key] = value.clone();
    }
    harness
}

#[tokio::test(flavor = "current_thread")]
async fn harness_idle_rotation_a_long_conversation_stays_inside_its_budget_and_rotates_once_after_idle() {
    let harness = rotating(json!({"contextTokens":{"fake-chat":40_000}}));
    let rig = rig("long", harness, |n| Ok(format!("第{n}份摘要：甲和乙约好周六爬山。"))).await;
    let mut bodies = Vec::new();
    for n in 1..=24 {
        // Every answer reports 30000 input tokens, over the cap: only the idle time decides.
        let body = rig.turn(&format!("long-{n}"), &long_prompt(n), answer(&format!("好的{n}"), 30_000)).await;
        assert!(request_tokens(&body) <= 32_800, "request {n} stays inside the budget: {}", request_tokens(&body));
        bodies.push(body);
    }
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 1, "one budget checkpoint in 24 back-to-back turns: {checkpoints:?}");
    assert_eq!(checkpoints[0]["purpose"], "budget", "turns in quick succession never rotate");
    assert!(texts(&bodies[18])[1].starts_with(SUMMARY_LABEL), "turn 19 replays the summary");
    rig.sleep(10);
    let rotated = rig.turn("long-25", &long_prompt(25), answer("好的25", 30_000)).await;
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 2, "{checkpoints:?}");
    assert_eq!(checkpoints[1]["purpose"], "rotation");
    assert_eq!(checkpoints[1]["summary"], "第2份摘要：甲和乙约好周六爬山。");
    let texts = texts(&rotated);
    assert!(texts[1].starts_with(SUMMARY_LABEL) && texts[1].contains("第2份摘要"), "{}", texts[1]);
    let handover = &texts[2..texts.len() - 1];
    assert!(contains(handover, "第23轮") && contains(handover, "第24轮"), "the newest rounds are handed over verbatim");
    assert!(!contains(&texts, "第22轮"), "older rounds are in the summary");
    let handover_tokens: u64 = handover.iter().map(|text| MESSAGE_TOKENS + text_tokens(text)).sum();
    assert!(handover_tokens <= HANDOVER_TOKENS, "{handover_tokens}");
    let seen = rig.summaries();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].1.starts_with(PREVIOUS_LABEL) && seen[1].1.contains("第1份摘要"),
        "the rotation's summary folds in the earlier one"
    );
    assert_eq!(rig.records("history-checkpoint").len(), 2);
    // The next turn comes soon after: the session does not rotate again.
    let after = rig.turn("long-26", &long_prompt(26), answer("好的26", 30_000)).await;
    assert_eq!(rig.events("checkpoint").len(), 2, "the session rotates once");
    assert_eq!(rig.summaries().len(), 2);
    assert!(contains(&self::texts(&after), "第25轮"));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_idle_rotation_a_failed_summary_leaves_the_session_unrotated_and_is_recorded() {
    let rig = rig("rotation-failed", rotating(json!({})), |_| Err(crate::digest::ModelError("timeout"))).await;
    rig.seed(1..=5, long_prompt, json!({"captured_usage":{"prompt_tokens":30_000}}));
    rig.sleep(10);
    let body = rig.turn("rotation-failed-6", "现在回复", answer("好的", 30_000)).await;
    assert_eq!(rig.summaries().len(), 1, "the rotation asked for one summary");
    assert_eq!(rig.events("checkpoint_failed"), [json!({"purpose":"rotation","class":"timeout"})]);
    assert!(rig.events("checkpoint").is_empty(), "a failed summary never rotates the session");
    let texts = texts(&body);
    for n in 1..=5 {
        assert!(contains(&texts, &format!("第{n}轮")), "round {n} is still replayed");
    }
    assert_eq!(rig.records("summary-failed").len(), 1);
    rig.turn("rotation-failed-7", "再回复", answer("好", 30_000)).await;
    assert_eq!(rig.summaries().len(), 1, "a turn soon after does not try again");
}

#[tokio::test(flavor = "current_thread")]
async fn harness_idle_rotation_without_summaries_hands_over_the_newest_rounds_only() {
    let rig = rig("rotation-plain", rotating(json!({"summaries":false})), |_| Ok("never asked".to_string())).await;
    rig.seed(1..=5, long_prompt, json!({"captured_usage":{"prompt_tokens":30_000}}));
    // Due one minute before the 5-minute cache life runs out: 3 minutes asleep is not yet due.
    rig.sleep(3);
    let early = rig.turn("rotation-plain-6", "现在回复", answer("好的", 30_000)).await;
    assert!(rig.events("checkpoint").is_empty(), "three minutes asleep does not rotate");
    assert!(contains(&texts(&early), "第1轮 "));
    rig.sleep(4);
    let body = rig.turn("rotation-plain-7", "再回复", answer("好", 30_000)).await;
    let checkpoints = rig.events("checkpoint");
    assert_eq!(checkpoints.len(), 1, "four minutes asleep rotates: {checkpoints:?}");
    assert_eq!(checkpoints[0]["purpose"], "rotation");
    assert!(checkpoints[0]["summary"].is_null(), "no summary without summaries");
    assert!(rig.summaries().is_empty(), "no summary is asked for");
    let texts = texts(&body);
    assert!(!texts.iter().any(|text| text.starts_with(SUMMARY_LABEL)));
    assert!(contains(&texts, "第4轮") && contains(&texts, "第5轮") && contains(&texts, "现在回复"));
    assert!(!contains(&texts, "第3轮"), "rounds past the 4000-token handover leave");
}
