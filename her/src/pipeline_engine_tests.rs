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
        .run_chat_turn("group:42", "hello seam chat".into(), None, &runtime.cfg(), &[], &[], "chat", false)
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
        .run_chat_turn("private:8808", "hello seam owner".into(), None, &runtime.cfg(), &[], &[], "closed-agent", true)
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
    runtime.assert_scripted_turn(&script, "group:42", "hello seam reserved2", "scripted");
}

#[tokio::test]
async fn ticket159_reserved2_uses_the_cli_pin_added_while_waiting_for_admission() {
    cli_pin_added_at_admission(false).await;
}

#[tokio::test]
async fn ticket159_expired_admission_never_starts_late_cli_pin() {
    cli_pin_added_at_admission(true).await;
}

async fn cli_pin_added_at_admission(expire: bool) {
    let mut queued = SeamRuntime::scripted_config();
    queued["engineDefault"] = json!("harness");
    let runtime = SeamRuntime::new("seam-cli-pin-at-admission", "reserved2", false, queued);
    let queued = runtime.cfg();
    let key = "group:42";
    let token = crate::social_state::ensure_or_rotate(&runtime.context, key, false).unwrap();
    let gate = crate::engine_switch::turn_gate(&runtime.context, key).await.unwrap();
    let mut pipe = Pipeline::new_without_dsh(&runtime.context, &queued, true).unwrap();
    pipe.expect_dispatch_token(token);
    let route = SharedReserved2Route::default();
    *route.lock().unwrap() = Reserved2Route::PendingHarness;
    pipe.share_reserved2_route(route.clone(), MintedEpoch::default());
    let deadline = crate::turn_ledger::now_ms() + 20;
    if expire { pipe.expect_dispatch_deadline(deadline); }
    let target = Target::Group(42);
    let wake = pipe.run_turn(&target, "unused DSH wake", "fixture CLI wake", &queued);
    tokio::pin!(wake);
    tokio::select! { biased;
        result = &mut wake => panic!("the wake passed the held conversation gate: {result:?}"),
        _ = std::future::ready(()) => {}
    }

    let mut current: Value = serde_json::from_slice(&std::fs::read(runtime.root.join("config/config.json")).unwrap()).unwrap();
    current["backends"] = json!({"sessions":{key:"missing-fixture-cli"}});
    runtime.context.write_atomic(RuntimeArea::Config, "config.json", current.to_string().as_bytes()).unwrap();
    if expire {
        let remaining = deadline.saturating_sub(crate::turn_ledger::now_ms()).max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(remaining + 1)).await;
    }
    drop(gate);

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut wake).await.unwrap();
    let trials = std::fs::read_to_string(runtime.root.join("data/logs/opencode-trial.jsonl")).unwrap_or_default();
    if expire {
        assert!(trials.is_empty(), "an expired admission must not start the pinned CLI");
        assert_eq!(result.unwrap_err(), RESERVED2_CAP_BEFORE_ADMISSION);
        assert!(*route.lock().unwrap() == Reserved2Route::ExpiredBeforeAdmission);
        assert!(!runtime.ledger().iter().any(|r| r["k"] == "begun" || r["k"] == "wake_claim"));
        let gate = tokio::time::timeout(std::time::Duration::from_secs(1),
            crate::engine_switch::turn_gate(&runtime.context, key)).await.expect("expired admission released its gate").unwrap();
        drop(gate);
        return;
    }
    assert!(trials.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["key"] == key && row["backend"] == "missing-fixture-cli"),
        "the wake must dispatch the CLI pin selected after the gate opens: {result:?}");
    assert!(result.is_err(), "the fixture's CLI backend does not exist");
    assert!(*route.lock().unwrap() == Reserved2Route::Cli, "hub settlement must use the admitted CLI route");
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
        .run_chat_turn("private:8808", "hello seam harness".into(), None, &cfg, &[], &[], "closed-agent", true)
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

/// #164: a harness chat turn carries the inbound image the pipeline acquired (the same media parts
/// DSH gets) after the prompt, and the voice note stays the prompt's `[语音]` label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket164_harness_chat_turn_sends_the_acquired_image_with_the_voice_label() {
    use base64::Engine as _;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    let model = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    model.set_nonblocking(true).unwrap();
    let onebot = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    onebot.set_nonblocking(true).unwrap();
    let mut runtime = SeamRuntime::new(
        "harness-media-164",
        "closed-agent",
        false,
        json!({"consoleToken":"fixture-token","ownerQQ":8808,
            "conversationEngines":{"private:8808":"harness"},"harness":{"model":"fake-chat"}}),
    );
    std::fs::write(runtime.root.join("config/harness-api-key"), "synthetic-model-key-164").unwrap();
    std::fs::write(
        runtime.root.join("dependencies.json"),
        json!({
            "dsh": "http://127.0.0.1:43201",
            "model": format!("http://{}", model.local_addr().unwrap()),
            "onebot": format!("http://{}", onebot.local_addr().unwrap()),
        })
        .to_string(),
    )
    .unwrap();
    runtime.reload();
    let cfg = runtime.cfg();
    // A 1x1 PNG is small enough to pass media normalization unchanged.
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let png = base64::engine::general_purpose::STANDARD.encode(png);
    // The gateway answers `get_image` with the picture and refuses every other call. It stops when
    // `StopGateway` drops, on success and on a failed assertion alike.
    struct StopGateway(std::sync::Arc<AtomicBool>);
    impl Drop for StopGateway {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let stop = StopGateway(std::sync::Arc::new(AtomicBool::new(false)));
    let (stopped, gateway_png) = (stop.0.clone(), png.clone());
    let gateway = std::thread::spawn(move || {
        let mut paths = Vec::new();
        while !stopped.load(Ordering::SeqCst) {
            let Some((mut stream, request)) = fake_dsh_accept(&onebot, std::time::Duration::from_millis(200)) else {
                continue;
            };
            if request.path == "/get_image" {
                let body = json!({"status":"ok","retcode":0,"data":{"base64":gateway_png}}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            } else {
                write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            paths.push(request.path);
        }
        paths
    });
    let server = std::thread::spawn(move || {
        let (mut stream, request) = fake_dsh_accept(&model, std::time::Duration::from_secs(10))
            .expect("the harness turn reached the model");
        let chunk = json!({"id":"media-164","object":"chat.completion.chunk","model":"fake-chat",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"[SILENT]"},"finish_reason":null}]});
        let end = json!({"id":"media-164","object":"chat.completion.chunk","model":"fake-chat",
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
    let mut pipe = Pipeline::new_without_dsh(&runtime.context, &cfg, true).unwrap();
    let media = [json!({"kind":"image","file":"fixture-164.png"})];
    let result = pipe
        .run_chat_turn("private:8808", "hello harness media [语音]".into(), None, &cfg, &[], &media, "closed-agent", true)
        .await;
    let request = server.join().unwrap();
    drop(stop);
    let paths = gateway.join().unwrap();
    assert!(result.is_ok(), "the harness turn must run: {result:?}");
    assert!(paths.iter().any(|path| path == "/get_image"), "the image came from the gateway: {paths:?}");
    let sent = request["messages"].as_array().unwrap().last().unwrap().clone();
    let content = sent["content"]
        .as_array()
        .unwrap_or_else(|| panic!("the user message must carry parts: {sent}"));
    assert_eq!(content.len(), 3, "{sent}");
    let prompt = content[0]["text"].as_str().unwrap();
    assert!(prompt.contains("hello harness media [语音]"), "the voice label stays in the prompt: {prompt}");
    // `[图片1]（图片获取失败）` here means the acquisition failed: the gateway reply, or the
    // process-wide four-permit media source gate (`media::admit_source`) was busy.
    assert_eq!(content[1], json!({"type":"text","text":"[图片1]"}), "{sent}");
    assert_eq!(
        content[2],
        json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{png}")}}),
        "the normalized image reaches the model as the DSH path sends it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_unbound_pipeline_asks_never_open_a_dsh_stream() {
    // A session mapping makes `ensure_session` return without an RPC, so the stream open is the
    // call that would reach DSH: it must refuse locally as well.
    let dsh = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    dsh.set_nonblocking(true).unwrap();
    let mut runtime = SeamRuntime::new("seam-unbound-ask", "closed-agent", false, SeamRuntime::scripted_config());
    std::fs::write(
        runtime.root.join("dependencies.json"),
        json!({
            "dsh": format!("http://{}", dsh.local_addr().unwrap()),
            "model": "http://127.0.0.1:43204",
            "onebot": "http://127.0.0.1:43203",
        })
        .to_string(),
    )
    .unwrap();
    runtime.reload();
    crate::session_map::bind(&runtime.context, "private:8808", "seam-session-62", None).unwrap();
    let cfg = runtime.cfg();
    let mut pipe = Pipeline::new_without_dsh(&runtime.context, &cfg, true).unwrap();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        pipe.ask("private:8808", "hello seam unbound", 5),
    )
    .await;
    match dsh.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(10035) => {}
        other => panic!("the unbound ask contacted DSH: {other:?}"),
    }
    assert_eq!(
        reply,
        Ok(Err(crate::dsh_credential::UNBOUND.to_string())),
        "an unbound pipeline refuses its ask locally"
    );
}

/// #197: `pipeline.rs` keeps the turn code every engine shares; the DSH adapter's own code (DSH
/// sessions and RPCs, model selection, the follow-stream collector, DSH cancellation and usage
/// rows) lives in `pipeline/dsh_adapter.rs`, reached only through its registration points.
#[test]
fn dsh197_pipeline_rs_names_no_dsh_turn_code() {
    let source = include_str!("pipeline.rs");
    let production = &source[..source.find("\nmod tests {").expect("pipeline.rs has its test module")];
    for token in [
        "FollowStream", "TurnUsage", "\"session/prompt\"", "\"session/create\"", "\"session/selectModel\"",
        "\"session/cancel\"", "\"session/rename\"", "\"workspace/create\"", "\"workspace/rename\"",
        "\"agentPresets/list\"", "\"settings/describe\"", "stop_session_work", "archive_session",
        "probe_prompt", "dsh_profile::", "dsh_health::", ".relogin(", "rpc.call", "fn ensure_session",
        "fn apply_model", "fn cancel_with_reason",
        "fn turn_records(", "fn prompt_error_needs_probe(", "fn definite_refusal(", "fn preset_for(",
        "fn ensure_workspace(", "fn ensure_session(", "fn access_epoch(", "fn discard_created_session(",
        "fn require_preset(", "fn closed_preset(", "fn prepare_title(", "fn flush_title(",
        "fn cancel_session(", "fn cancel(", "fn cancel_with_reason(", "fn cancel_reserved_epoch(",
        "fn cancel_reserved2_cap(", "fn cancel_with_dispatch(",
        "fn note_session(", "fn has_dsh(", "fn abandon_attempt(", "fn abandon_attempt_for_test(",
        "fn session_policy(", "fn stale_policy(", "fn replace_session(", "fn failover_state(",
        "fn apply_model(", "fn ask_with_failover(", "fn ask_with_failover_with_deadline(",
        "fn cli_fallback(", "fn should_audit_key(", "fn hide_if_sensitive(", "fn report_turn_error(",
        "fn ask_with_failover_inner(", "fn exhausted_chain(", "fn ask(", "fn record_turn_usage(",
        "fn reply(", "fn handle(", "fn handle_with_failover(", "fn run_dsh_turn(",
        "enum Submit", "struct AccessEpoch",
    ] {
        assert!(!production.contains(token), "pipeline.rs still names DSH turn code: {token}");
    }
}
