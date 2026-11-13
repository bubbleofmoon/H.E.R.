// Attempt classification and cap interleavings through the real collector and fake DSH.
struct CompletionDsh {
    stop: Arc<std::sync::atomic::AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
    prompts: Arc<std::sync::atomic::AtomicUsize>,
    port: u16,
}

impl CompletionDsh {
    fn start(failover: bool) -> Self {
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stopping = stop.clone();
        let count = prompts.clone();
        let server = std::thread::spawn(move || {
            let pending: Arc<Mutex<std::collections::VecDeque<std::sync::mpsc::Sender<usize>>>> =
                Default::default();
            let mut clients = vec![];
            while !stopping.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                let pending = pending.clone();
                let count = count.clone();
                clients.push(std::thread::spawn(move || {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                    let mut head = [0; 4];
                    stream.peek(&mut head).unwrap();
                    if &head[..3] == b"GET" {
                        use tokio_tungstenite::tungstenite::{accept, Message};
                        let mut ws = accept(stream).unwrap();
                        let open: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                        let id = &open["streamId"];
                        let (tx, rx) = std::sync::mpsc::channel();
                        pending.lock().unwrap().push_back(tx);
                        ws.send(Message::Text(json!({"type":"item","streamId":id,"value":{"type":"snapshot","records":[],"hasMore":false}}).to_string())).unwrap();
                        let attempt = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
                        let end = if failover && attempt == 2 { "aborted" } else { "completed" };
                        let text = if attempt == 1 { "Error: provider unavailable" } else { "" };
                        for (i, (kind, data)) in [
                            ("turn/start", json!({"turn":attempt})),
                            ("assistant/message", json!({"turn":attempt,"message":{"content":[{"type":"text","text":text}]}})),
                            ("turn/end", json!({"turn":attempt,"reason":{"kind":end}})),
                        ].into_iter().enumerate() {
                            ws.send(Message::Text(json!({"type":"item","streamId":id,"value":{"type":"event","event":{"seq":i+1,"type":kind,"data":data}}}).to_string())).unwrap();
                        }
                        let _ = ws.read(); // Collector owns closure; keep the fake alive until then.
                    } else {
                        use std::io::Read;
                        let mut stream = stream;
                        let mut bytes = vec![];
                        let (path, body) = loop {
                            let mut chunk = [0; 4096];
                            let n = stream.read(&mut chunk).unwrap();
                            assert!(n > 0, "incomplete fake DSH request");
                            bytes.extend_from_slice(&chunk[..n]);
                            let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                            let head = String::from_utf8_lossy(&bytes[..end]);
                            let len = head.lines().find_map(|line| {
                                let (k,v) = line.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().unwrap())
                            }).unwrap_or(0);
                            if bytes.len() >= end + 4 + len {
                                break (head.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_string(),
                                    String::from_utf8_lossy(&bytes[end + 4..end + 4 + len]).to_string());
                            }
                        };
                        let value = if path == "/api/session/prompt" {
                            let attempt = count.fetch_add(1, Ordering::SeqCst) + 1;
                            pending.lock().unwrap().pop_front().unwrap().send(attempt).unwrap();
                            json!({"accepted":true})
                        } else { json!({"values":[]}) };
                        fake_dsh_respond(&mut stream, &FakeDshRequest { path: path.clone(), cookie: None, body }, 200, value);
                        drop(stream);
                    }
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            stop,
            server: Some(server),
            prompts,
            port,
        }
    }
}
impl Drop for CompletionDsh {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
    }
}

/// Every turn whose pipeline has ended must have exactly one durable terminal and admit wakes.
fn assert_completion_terminal(
    context: &RuntimeContext,
    id: &str,
    outcome: &str,
    reason: Option<&str>,
) {
    let records = crate::turn_ledger::read_records(context).unwrap();
    let ended: Vec<_> = records
        .iter()
        .filter(|r| r["turn"] == id && r["k"] == "ended")
        .collect();
    assert_eq!(
        ended.len(),
        1,
        "an ended pipeline turn must have exactly one terminal: {records:?}"
    );
    assert_eq!(ended[0]["outcome"], outcome);
    if let Some(reason) = reason {
        assert_eq!(ended[0]["reason"], reason);
    }
    let turns = crate::turn_ledger::fold(&records).0;
    assert!(
        crate::turn_ledger::blocking_turn(&turns, "group:42").is_none(),
        "later wakes must proceed"
    );
}

fn rejected_completion_cap_case(failover: bool, sleep: bool) {
    let fake = CompletionDsh::start(failover);
    let root = fake_runtime_root("hf136-fix2-completion");
    write_selected_secret(&root, [0x36; 32]);
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"chat"}"#).unwrap();
    let (context, mut pipe) = pipeline_for_fake_dsh(&root, fake.port);
    let key = "group:42";
    crate::session_map::bind(&context, key, "sid-136", None).unwrap();
    pipe.chat = Some(ChatRun {
        engine: crate::harness::Engine::Dsh,
        reserved: None,
        mode: "chat".into(),
        silent: true,
        media_parts: vec![],
        ack: None,
        delay_ms: 0,
        owner_started: false,
    });
    pipe.begin_ledger_turn(key).unwrap();
    let id = pipe.cur_turn.as_ref().unwrap().id.clone();
    let mut control = Pipeline::new_with_runtime(&context, &json!({}), true).unwrap();
    let cap_context = context.clone();
    let cap_id = id.clone();
    let mut attempts = 0;
    pipe.completion_observed_hook = Some(Box::new(move || {
        attempts += 1;
        if attempts != if failover { 2 } else { 1 } {
            return;
        }
        if sleep {
            let turn =
                crate::turn_ledger::Recorder::attach(&cap_context, &cap_id, key).without_retry();
            turn.verified("cancel-requested", "observationSleep")
                .unwrap();
            crate::console::ensure_or_rotate_agent_token(&cap_context, key, true).unwrap();
            turn.end_cancellation("completed", "observationSleep")
                .unwrap();
        } else {
            control.cancel_with_reason(key, "turnTimeout").unwrap();
        }
    }));
    let cfg = json!({"dsh":{"provider":"first","model":"first","fallbacks":[{"provider":"second","model":"second"}]}});
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(pipe.ask_with_failover(key, "synthetic completion test", 10, &cfg));
    pipe.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
    let submitted = fake.prompts.load(std::sync::atomic::Ordering::SeqCst);
    drop(fake);
    assert_eq!(submitted, if failover { 2 } else { 1 });
    if sleep {
        assert!(result.is_ok(), "{result:?}");
    } else {
        assert!(result.is_err(), "{result:?}");
    }
    assert_completion_terminal(
        &context,
        &id,
        if sleep { "completed" } else { "cancelled" },
        Some(if sleep {
            "observationSleep"
        } else {
            "turnTimeout"
        }),
    );
    pipe.begin_ledger_turn(key)
        .expect("the next wake is admitted without restart");
    let next_id = pipe.cur_turn.as_ref().unwrap().id.clone();
    pipe.end_ledger_turn(Ok(()));
    assert_completion_terminal(&context, &next_id, "completed", Some("ok"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hf136_fix2_rejected_completion_during_cap_rotation_closes_and_admits_next_wake() {
    rejected_completion_cap_case(false, false);
    rejected_completion_cap_case(false, true);
}

#[test]
fn hf136_fix2_failover_releases_rejected_completion_for_the_next_attempt_cap() {
    rejected_completion_cap_case(true, false);
    rejected_completion_cap_case(true, true);
}
