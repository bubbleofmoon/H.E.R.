// The turn-engine seam (`turn_engine`): every participation mode runs one admitted turn on the
// scripted adapter, and a harness turn runs on a pipeline built without a DSH login or credential.

/// A selected runtime for the seam tests. `dsh_login` writes a DSH credential. DSH is a loopback
/// port nothing answers, so a turn that reaches the DSH adapter fails instead of running.
struct SeamRuntime {
    root: PathBuf,
    context: RuntimeContext,
}

impl SeamRuntime {
    fn new(tag: &str, mode: &str, dsh_login: bool, config: Value) -> Self {
        let (root, _) = isolated_fixture(tag);
        if dsh_login {
            write_selected_secret(&root, [0x62; 32]);
        }
        std::fs::write(root.join("data/mode.json"), json!({"mode": mode}).to_string()).unwrap();
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        Self { root, context }
    }

    /// The configuration every scripted mode test uses: owner 8808, group 42 allowed.
    fn scripted_config() -> Value {
        json!({"consoleToken":"fixture-token","ownerQQ":8808,"allow":{"groups":[42]}})
    }

    /// Re-read the runtime after a test rewrote `dependencies.json`.
    fn reload(&mut self) {
        self.context = RuntimeContext::from_root(&self.root).unwrap();
    }

    fn cfg(&self) -> Value {
        self.context.load_config().unwrap()
    }

    /// A DSH pipeline whose next admitted turn runs on `script`.
    fn scripted(&self, script: &crate::turn_engine::Script) -> Pipeline {
        let mut pipe = Pipeline::new_with_runtime(&self.context, &self.cfg(), true).unwrap();
        pipe.scripted = Some(script.clone());
        pipe
    }

    fn ledger(&self) -> Vec<Value> {
        crate::turn_ledger::read_records(&self.context).unwrap()
    }

    fn timings(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("data/logs/turn-timing.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// One admitted turn ran on the scripted adapter: it got the turn's prompt; the ledger shows
    /// begun (as `backend`), attempt, accepted, started and a completed end; the timing record
    /// comes from the adapter's facts.
    fn assert_scripted_turn(&self, script: &crate::turn_engine::Script, key: &str, text: &str, backend: &str) {
        let seen = script.seen();
        assert_eq!(seen.len(), 1, "the scripted adapter ran exactly one turn: {seen:?}");
        assert_eq!(seen[0].0, key);
        assert!(seen[0].1.contains(text), "the adapter got the turn's prompt: {}", seen[0].1);
        let ledger = self.ledger();
        let begun = ledger.iter().find(|r| r["k"] == "begun" && r["key"] == key).expect("a ledger turn");
        assert_eq!(begun["backend"], backend);
        let id = begun["turn"].as_str().unwrap();
        let attempt = format!("scripted:{id}");
        for kind in ["attempt", "accepted"] {
            assert!(
                ledger.iter().any(|r| r["turn"] == id && r["k"] == kind && r["rpcId"] == attempt.as_str()),
                "{kind} {attempt}: {ledger:?}"
            );
        }
        assert!(
            ledger.iter().any(|r| r["turn"] == id && r["k"] == "started"),
            "the thinking start is recorded: {ledger:?}"
        );
        assert!(
            ledger.iter().any(|r| r["turn"] == id && r["k"] == "ended" && r["outcome"] == "completed"),
            "{ledger:?}"
        );
        let timings = self.timings();
        let timing = timings
            .iter()
            .find(|t| t["turnId"] == id)
            .expect("a timing record from the adapter's facts");
        assert_eq!(
            (&timing["key"], &timing["attemptRpcId"], &timing["endKind"], &timing["ok"], &timing["cancelled"], &timing["attempts"]),
            (&json!(key), &json!(attempt), &json!("completed"), &json!(true), &json!(false), &json!(1))
        );
    }
}

impl Drop for SeamRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_chat_turn_runs_on_the_scripted_adapter() {
    let runtime = SeamRuntime::new("seam-chat", "chat", true, SeamRuntime::scripted_config());
    let script = crate::turn_engine::Script::answering("[SILENT]");
    let mut pipe = runtime.scripted(&script);
    let result = pipe
        .run_chat_turn("group:42", "hello seam chat", None, &runtime.cfg(), &[], &[], "chat", false)
        .await;
    assert!(result.is_ok(), "the chat turn must run on the scripted adapter: {result:?}");
    runtime.assert_scripted_turn(&script, "group:42", "hello seam chat", "scripted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_closed_agent_turn_runs_on_the_scripted_adapter() {
    let runtime = SeamRuntime::new("seam-closed", "closed-agent", true, SeamRuntime::scripted_config());
    let script = crate::turn_engine::Script::answering("[SILENT]");
    let mut pipe = runtime.scripted(&script);
    let result = pipe
        .run_chat_turn("private:8808", "hello seam owner", None, &runtime.cfg(), &[], &[], "closed-agent", true)
        .await;
    assert!(result.is_ok(), "the closed-agent turn must run on the scripted adapter: {result:?}");
    runtime.assert_scripted_turn(&script, "private:8808", "hello seam owner", "scripted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_reserved_turn_runs_on_the_scripted_adapter() {
    let runtime = SeamRuntime::new("seam-reserved", "reserved", true, SeamRuntime::scripted_config());
    let script = crate::turn_engine::Script::answering("[SILENT]");
    let mut pipe = runtime.scripted(&script);
    let authority = crate::reserved::authority_for_test(&runtime.context, "group:42").unwrap();
    let result = pipe
        .run_reserved_turn("group:42", "hello seam reserved", &runtime.cfg(), &[], &[], true, authority)
        .await;
    assert!(result.is_ok(), "the reserved turn must run on the scripted adapter: {result:?}");
    runtime.assert_scripted_turn(&script, "group:42", "hello seam reserved", "scripted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_reserved2_turn_runs_on_the_scripted_adapter() {
    let runtime = SeamRuntime::new("seam-reserved2", "reserved2", true, SeamRuntime::scripted_config());
    let script = crate::turn_engine::Script::answering("[SILENT]");
    let mut pipe = runtime.scripted(&script);
    let result = pipe
        .run_turn(&Target::Group(42), "hello seam reserved2", "cli fixture prompt", &runtime.cfg())
        .await;
    assert_eq!(result, Ok(8), "the reserved2 turn must run on the scripted adapter");
    // A reserved2 ledger turn is begun before its engine is chosen; it is recorded as DSH's.
    runtime.assert_scripted_turn(&script, "group:42", "hello seam reserved2", "dsh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_harness_turn_builds_and_runs_without_a_dsh_credential() {
    use std::io::Write as _;
    let dsh = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    dsh.set_nonblocking(true).unwrap();
    let model = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    model.set_nonblocking(true).unwrap();
    let mut runtime = SeamRuntime::new(
        "seam-no-dsh",
        "closed-agent",
        false,
        json!({"consoleToken":"fixture-token","ownerQQ":8808,
            "conversationEngines":{"private:8808":"harness"},"harness":{"model":"fake-chat"}}),
    );
    std::fs::write(runtime.root.join("config/harness-api-key"), "synthetic-model-key-62").unwrap();
    std::fs::write(
        runtime.root.join("dependencies.json"),
        json!({
            "dsh": format!("http://{}", dsh.local_addr().unwrap()),
            "model": format!("http://{}", model.local_addr().unwrap()),
            "onebot": "http://127.0.0.1:43203",
        })
        .to_string(),
    )
    .unwrap();
    runtime.reload();
    let cfg = runtime.cfg();
    let mut pipe = Pipeline::new_without_dsh(&runtime.context, &cfg, true)
        .unwrap_or_else(|error| panic!("a pipeline without DSH must build without a DSH credential: {error}"));
    assert!(!pipe.has_dsh(), "the pipeline holds no DSH login");
    let server = std::thread::spawn(move || {
        let (mut stream, request) = fake_dsh_accept(&model, std::time::Duration::from_secs(10))
            .expect("the harness turn reached the model");
        let chunk = json!({"id":"seam-62","object":"chat.completion.chunk","model":"fake-chat",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"[SILENT]"},"finish_reason":null}]});
        let end = json!({"id":"seam-62","object":"chat.completion.chunk","model":"fake-chat",
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        serde_json::from_str::<Value>(&request.body).unwrap()
    });
    let result = pipe
        .run_chat_turn("private:8808", "hello seam harness", None, &cfg, &[], &[], "closed-agent", true)
        .await;
    let request = server.join().unwrap();
    assert!(result.is_ok(), "the harness turn must run without DSH: {result:?}");
    assert_eq!((&request["model"], &request["stream"]), (&json!("fake-chat"), &json!(true)));
    assert!(!selected_credentials_path(&runtime.root).exists(), "the runtime has no DSH credential");
    match dsh.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(10035) => {}
        other => panic!("the harness turn contacted DSH: {other:?}"),
    }
    let ledger = runtime.ledger();
    let begun = ledger.iter().find(|r| r["k"] == "begun").expect("a ledger turn");
    assert_eq!(begun["backend"], "harness");
    let id = begun["turn"].as_str().unwrap();
    let attempt = format!("harness:{id}");
    for kind in ["attempt", "accepted"] {
        assert!(
            ledger.iter().any(|r| r["turn"] == id && r["k"] == kind && r["rpcId"] == attempt.as_str()),
            "{kind} {attempt}: {ledger:?}"
        );
    }
    assert!(
        ledger.iter().any(|r| r["turn"] == id && r["k"] == "started"),
        "the harness thinking start is recorded: {ledger:?}"
    );
    assert!(
        ledger.iter().any(|r| r["turn"] == id && r["k"] == "ended" && r["outcome"] == "completed"),
        "{ledger:?}"
    );
    let timings = runtime.timings();
    let timing = timings
        .iter()
        .find(|t| t["turnId"] == id)
        .expect("a timing record from the harness facts");
    assert_eq!(
        (&timing["attemptRpcId"], &timing["endKind"], &timing["ok"], &timing["cancelled"], &timing["attempts"], &timing["model"]),
        (&json!(attempt), &json!("completed"), &json!(true), &json!(false), &json!(1), &json!("openai/fake-chat"))
    );
}
