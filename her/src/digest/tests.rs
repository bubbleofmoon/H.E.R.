use super::conversation::PROMPT;
use super::*;
use crate::memory::MemoryStore;
use crate::runtime::RuntimeContext;
use serde_json::{json, Value};
use std::{collections::VecDeque, path::PathBuf, sync::{atomic::AtomicBool, Arc, Mutex}};

struct Root(PathBuf);
impl Drop for Root { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

fn fixture(mode: Option<&str>, cfg: Value, model_url: &str) -> (Root, RuntimeContext) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!("her-digest-unit-{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    for d in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(d)).unwrap(); }
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(root.join("dependencies.json"), json!({"dsh":"http://127.0.0.1:1","model":model_url,"onebot":"http://127.0.0.1:1"}).to_string()).unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    if let Some(mode) = mode { std::fs::write(root.join("data/mode.json"), json!({"mode":mode}).to_string()).unwrap(); }
    let context = RuntimeContext::from_root(&root).unwrap();
    (Root(root), context)
}

struct Fake { replies: Mutex<VecDeque<Result<String, &'static str>>>, inputs: Mutex<Vec<String>> }
impl Fake {
    fn new(replies: Vec<Result<&str, &'static str>>) -> Arc<Self> {
        Arc::new(Self { replies: Mutex::new(replies.into_iter().map(|r| r.map(str::to_string)).collect()), inputs: Mutex::new(Vec::new()) })
    }
    fn inputs(&self) -> Vec<String> { self.inputs.lock().unwrap().clone() }
}
impl DigestModel for Fake {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        assert_eq!(system, PROMPT);
        self.inputs.lock().unwrap().push(input.to_string());
        self.replies.lock().unwrap().pop_front().unwrap_or(Ok("[]".into())).map_err(ModelError)
    }
}

const ANY: Limits = Limits { min_messages: 1, min_interval_ms: 0 };

fn run(context: &RuntimeContext, model: &Fake, key: &str, limits: &Limits) -> Result<Outcome, DigestError> {
    let cfg = context.load_config().unwrap();
    Digest::new(context, &cfg, model).run_conversation(key, limits, &AtomicBool::new(false))
}

fn rows(root: &Root) -> Vec<(String, String, String, String)> {
    let conn = rusqlite::Connection::open(root.0.join("data/memory.db")).unwrap();
    let mut stmt = conn.prepare("SELECT owner_id, kind, source, content FROM memory WHERE source LIKE 'digest:%' ORDER BY id").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect()
}

fn social(root: &Root, key: &str, messages: Value) {
    std::fs::write(root.0.join("data/social-v2.json"), json!({"conversations":{key:{"agentToken":"t","recentMessages":messages}}}).to_string()).unwrap();
}

fn journal_message(context: &RuntimeContext, key: &str, n: i64, event: Value) {
    let journal = crate::inbound::Journal::open(context).unwrap();
    let event_key = format!("{key}|message:{n}");
    journal.record_received(&event_key, key, &event.to_string(), crate::parity_clock::now_ms()).unwrap();
    journal.mark_handled(&[event_key], crate::parity_clock::now_ms()).unwrap();
}

fn private_text(user: i64, text: &str) -> Value {
    json!({"post_type":"message","message_type":"private","self_id":1000,"user_id":user,
        "message":[{"type":"text","data":{"text":text}}],"sender":{"nickname":"主人"}})
}

const CFG: fn() -> Value = || json!({"consoleToken":"digest-unit","ownerQQ":9001});

#[test]
fn reserved2_digest_writes_inferences_with_bridge_provenance_from_a_fake_model() {
    let (root, context) = fixture(None, CFG(), "http://127.0.0.1:1");
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":"周六去爬山","isSelf":false},
        {"sender":"我","text":"好呀一起","isSelf":true},
        {"seq":2,"sender":"管理员","userId":"9001","isOwner":true,"text":"/reset","isSelf":false},
        {"seq":3,"sender":"乙","userId":"5002","text":"我下周搬家","isSelf":false}
    ]));
    let model = Fake::new(vec![Ok(r#"[{"subject":"爬山","content":"甲周六去爬山","source":"model-claimed","kind":"fact"},{"subject":"x","content":"  "}]"#)]);
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 3, memories: 1 }));
    assert_eq!(rows(&root), vec![("group:42".to_string(), "inference".to_string(), "digest:group:42:msg:1-3".to_string(), "甲周六去爬山".to_string())]);
    let inputs = model.inputs();
    assert_eq!(inputs.len(), 1);
    assert!(inputs[0].contains("#1 甲：周六去爬山"), "{}", inputs[0]);
    assert!(inputs[0].contains("我：好呀一起"), "{}", inputs[0]);
    assert!(inputs[0].contains("#3 乙：我下周搬家"), "{}", inputs[0]);
    assert!(!inputs[0].contains("/reset"), "{}", inputs[0]);
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Quiet));
    assert_eq!(model.inputs().len(), 1);
}

#[test]
fn commands_and_her_own_notices_never_reach_the_digest_input() {
    let (root, context) = fixture(Some("chat"), CFG(), "http://127.0.0.1:1");
    journal_message(&context, "private:9001", 1, private_text(9001, "放弃 0a1b2c3d"));
    journal_message(&context, "private:9001", 2, private_text(9001, "/new"));
    journal_message(&context, "private:9001", 3, private_text(1000, "会话已暂停，回复 重试 0a1b2c3d"));
    let mut echo = private_text(1000, "会话已暂停，回复 重试 0a1b2c3d");
    echo["post_type"] = json!("message_sent");
    journal_message(&context, "private:9001", 4, echo);
    journal_message(&context, "private:9001", 5, private_text(9001, "明天提醒我交房租"));
    let model = Fake::new(vec![Ok(r#"[{"subject":"房租","content":"主人明天要交房租"}]"#)]);
    assert_eq!(run(&context, &model, "private:9001", &ANY), Ok(Outcome::Written { through: 5, memories: 1 }));
    assert_eq!(rows(&root)[0].2, "digest:private:9001:inbound:1-5");
    let inputs = model.inputs();
    assert!(inputs[0].contains("#5 管理员：明天提醒我交房租"), "{}", inputs[0]);
    for hidden in ["放弃", "/new", "会话已暂停"] {
        assert!(!inputs[0].contains(hidden), "{hidden} reached the input: {}", inputs[0]);
    }
}

#[test]
fn harness_conversation_is_digested_with_her_replies_from_conversation_storage() {
    let (root, context) = fixture(Some("chat"), json!({"consoleToken":"digest-unit","ownerQQ":9001,"conversationEngines":{"group:77":"harness"}}), "http://127.0.0.1:1");
    journal_message(&context, "group:77", 1, json!({"post_type":"message","message_type":"group","group_id":77,"self_id":1000,"user_id":5002,
        "message":[{"type":"text","data":{"text":"周末一起打球吗"}}],"sender":{"card":"乙"}}));
    drop(crate::harness::store::Store::open(&context).unwrap());
    let now = crate::parity_clock::now_ms();
    let conn = rusqlite::Connection::open(root.0.join("data/conversations.db")).unwrap();
    conn.execute("INSERT INTO sessions(id,conversation,generation) VALUES('s1','group:77','g1')", []).unwrap();
    conn.execute("INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms,ended_ms) VALUES('t1','s1','group:77','chat','completed',?1,?1)", [now]).unwrap();
    conn.execute("INSERT INTO events(session_id,turn_id,kind,payload,created_ms) VALUES('s1','t1','user',?1,?2)",
        rusqlite::params![json!({"text":"【角色卡】小鲸鱼的设定\n\n乙：周末一起打球吗"}).to_string(), now]).unwrap();
    conn.execute("INSERT INTO events(session_id,turn_id,kind,payload,created_ms) VALUES('s1','t1','assistant',?1,?2)",
        rusqlite::params![json!({"text":"好啊周六下午"}).to_string(), now + 1000]).unwrap();
    drop(conn);
    let model = Fake::new(vec![Ok(r#"[{"subject":"打球","content":"乙约周六下午打球"}]"#)]);
    assert_eq!(run(&context, &model, "group:77", &ANY), Ok(Outcome::Written { through: 1, memories: 1 }));
    let inputs = model.inputs();
    assert!(inputs[0].contains("#1 乙：周末一起打球吗"), "{}", inputs[0]);
    assert!(inputs[0].contains("我：好啊周六下午"), "{}", inputs[0]);
    assert!(!inputs[0].contains("角色卡"), "{}", inputs[0]);
    assert!(rows(&root).iter().any(|r| r.2 == "digest:group:77:inbound:1-1"));
}

#[test]
fn model_failure_and_unusable_output_write_nothing_and_keep_the_cursor() {
    let (root, context) = fixture(None, CFG(), "http://127.0.0.1:1");
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":"第一句","isSelf":false},
        {"seq":2,"sender":"甲","userId":"5001","text":"第二句","isSelf":false},
        {"seq":3,"sender":"甲","userId":"5001","text":"第三句","isSelf":false}
    ]));
    let model = Fake::new(vec![Err("quota"), Ok("content_filter"), Ok("好的"), Ok(""), Ok("[]")]);
    let cfg = context.load_config().unwrap();
    let expected = [Err(DigestError::Model("quota")), Err(DigestError::Output("refusal-or-error")),
        Err(DigestError::Output("unparseable")), Err(DigestError::Output("empty"))];
    for want in expected {
        assert_eq!(run(&context, &model, "group:42", &ANY), want);
        assert!(rows(&root).is_empty());
        assert_eq!(MemoryStore::open(&context, &cfg).unwrap().for_reader("group:42").digest_cursor("msg").unwrap(), None);
    }
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 3, memories: 0 }));
    let inputs = model.inputs();
    assert_eq!(inputs.len(), 5);
    assert!(inputs.windows(2).all(|w| w[0] == w[1]), "every attempt reads the same messages");
    assert_eq!(DigestError::Output("unparseable").step(), "digest-result");
}

#[test]
fn quiet_young_and_cancelled_runs_never_ask_the_model() {
    let (root, context) = fixture(None, CFG(), "http://127.0.0.1:1");
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":"第一句","isSelf":false},
        {"seq":2,"sender":"甲","userId":"5001","text":"第二句","isSelf":false}
    ]));
    let model = Fake::new(vec![Ok("[]"), Ok("[]")]);
    let young = Limits { min_messages: 3, min_interval_ms: 0 };
    assert_eq!(run(&context, &model, "group:42", &young), Ok(Outcome::Quiet));
    let cfg = context.load_config().unwrap();
    assert_eq!(Digest::new(&context, &cfg, &*model).run_conversation("group:42", &ANY, &AtomicBool::new(true)), Err(DigestError::Cancelled));
    assert!(model.inputs().is_empty());
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 2, memories: 0 }));
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":"第一句","isSelf":false},
        {"seq":2,"sender":"甲","userId":"5001","text":"第二句","isSelf":false},
        {"seq":3,"sender":"甲","userId":"5001","text":"第三句","isSelf":false}
    ]));
    let spaced = Limits { min_messages: 1, min_interval_ms: 3_600_000 };
    assert_eq!(run(&context, &model, "group:42", &spaced), Ok(Outcome::Quiet));
    assert_eq!(model.inputs().len(), 1);
}

#[test]
fn a_restarted_sequence_is_read_from_the_start() {
    let (root, context) = fixture(None, CFG(), "http://127.0.0.1:1");
    let cfg = context.load_config().unwrap();
    MemoryStore::open(&context, &cfg).unwrap().for_reader("group:42").write_digest("msg", None, 50, &[]).unwrap();
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":"第一句","isSelf":false},
        {"seq":2,"sender":"甲","userId":"5001","text":"第二句","isSelf":false}
    ]));
    let model = Fake::new(vec![Ok(r#"[{"subject":"甲","content":"甲说了两句"}]"#)]);
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 2, memories: 1 }));
    assert!(rows(&root)[0].2.ends_with(":msg:1-2"), "{:?}", rows(&root));
}

#[test]
fn input_is_capped_and_the_rest_waits_for_the_next_run() {
    let (root, context) = fixture(None, json!({"consoleToken":"digest-unit","ownerQQ":9001,"memory":{"digest":{"maxInputChars":1000}}}), "http://127.0.0.1:1");
    let long = "字".repeat(400);
    social(&root, "group:42", json!([
        {"seq":1,"sender":"甲","userId":"5001","text":long,"isSelf":false},
        {"seq":2,"sender":"甲","userId":"5001","text":long,"isSelf":false},
        {"seq":3,"sender":"甲","userId":"5001","text":long,"isSelf":false},
        {"seq":4,"sender":"甲","userId":"5001","text":long,"isSelf":false}
    ]));
    let model = Fake::new(vec![Ok("[]"), Ok("[]")]);
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 2, memories: 0 }));
    assert_eq!(run(&context, &model, "group:42", &ANY), Ok(Outcome::Written { through: 4, memories: 0 }));
}

fn read_http(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 { break; }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
            if bytes.len() >= end + 4 + len { break; }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// Answers successive requests with (status, body, delay ms); records each raw request.
fn relay(replies: Vec<(u16, String, u64)>) -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || for (status, body, delay) in replies {
        let Ok((mut stream, _)) = listener.accept() else { return };
        log.lock().unwrap().push(read_http(&mut stream));
        std::thread::sleep(std::time::Duration::from_millis(delay));
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    });
    (url, seen)
}

fn completion(content: &str, finish: &str) -> String {
    json!({"id":"fixture","object":"chat.completion","created":1,"model":"digest-unit-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":finish}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string()
}

#[test]
fn relay_model_answers_and_classifies_failures_without_provider_text() {
    let (url, seen) = relay(vec![
        (200, completion("[]", "stop"), 0),
        (429, r#"{"error":{"message":"private-provider-text"}}"#.to_string(), 0),
        (500, "{}".to_string(), 0),
        (200, completion("", "content_filter"), 0),
        (200, completion("[]", "stop"), 3000),
    ]);
    let (root, context) = fixture(None, json!({"consoleToken":"digest-unit","harness":{"model":"digest-unit-model"},"memory":{"digest":{"timeoutSecs":1}}}), &url);
    std::fs::write(root.0.join("config/harness-api-key"), "digest-unit-key").unwrap();
    let cfg = context.load_config().unwrap();
    let model = super::model::RelayModel::from_runtime(&context, &cfg).unwrap();
    assert_eq!(model.complete("system", "input"), Ok("[]".to_string()));
    assert_eq!(model.complete("system", "input"), Err(ModelError("quota")));
    assert_eq!(model.complete("system", "input"), Err(ModelError("backend")));
    assert_eq!(model.complete("system", "input"), Err(ModelError("refusal")));
    assert_eq!(model.complete("system", "input"), Err(ModelError("timeout")));
    let seen = seen.lock().unwrap();
    assert!(seen[0].starts_with("POST /v1/chat/completions"), "{}", seen[0]);
    assert!(seen[0].to_ascii_lowercase().contains("authorization: bearer digest-unit-key"));
    assert!(seen[0].contains("input"));
    let unconfigured = json!({"consoleToken":"digest-unit"});
    assert!(matches!(super::model::RelayModel::from_runtime(&context, &unconfigured), Err(ModelError("unconfigured"))));
}
