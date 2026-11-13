#[tokio::test]
async fn hf123_cli_protocol_uses_console_and_stops_on_refusal() {
    hf123_cli_protocol(0).await;
}

#[tokio::test]
async fn hf123_cli_protocol_revises_held_draft_without_backend_replay_on_refusal() {
    hf123_cli_protocol(1).await;
}

#[tokio::test]
async fn hf123_cli_protocol_retains_all_inputs_across_multiple_revisions() {
    hf123_cli_protocol(3).await;
}

#[tokio::test]
async fn hf123_fix2_cli_connected_console_preserves_hold_response_budget() {
    let started = std::time::Instant::now();
    hf123_cli_protocol_with_hold(1, std::time::Duration::from_millis(1500)).await;
    assert!(started.elapsed() >= std::time::Duration::from_secs(3));
}

#[tokio::test]
async fn hf123_fix2_cli_unavailable_console_fails_closed_within_bound() {
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    // A refused loopback connection and (on Unix) an exhausted listen backlog exercise both
    // immediate failure and a real connect timeout without any non-loopback traffic.
    for saturated in [true, false] {
        if saturated && !cfg!(unix) {
            continue;
        }
        let console = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = console.local_addr().unwrap();
        let mut pending = Vec::new();
        if saturated {
            for _ in 0..1024 {
                match TcpStream::connect_timeout(&address, Duration::from_millis(40)) {
                    Ok(stream) => pending.push(stream),
                    Err(_) => break,
                }
            }
            assert!(
                !pending.is_empty(),
                "loopback backlog must contain pending connections"
            );
        }
        let console = saturated.then_some(console);
        let onebot = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        onebot.set_nonblocking(true).unwrap();
        let (root, _) = isolated_fixture("hf123-fix2-console-down");
        write_selected_secret(&root, [0x44; 32]);
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            serde_json::json!({
                "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:2",
                "onebot":format!("http://127.0.0.1:{}", onebot.local_addr().unwrap().port())
            })
            .to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root)
            .unwrap()
            .with_console_port(address.port());
        let cfg = context.load_config().unwrap();
        let mut pipeline = Pipeline::new_with_runtime(&context, &cfg, true).unwrap();
        pipeline.begin_ledger_turn("group:42").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (stopping, requests) = (stop.clone(), seen.clone());
        let worker = std::thread::spawn(move || {
            use std::io::Write;
            while !stopping.load(Ordering::SeqCst) {
                if let Some((mut stream, request)) =
                    fake_dsh_accept(&onebot, Duration::from_millis(50))
                {
                    requests.lock().unwrap().push(request);
                    let body = r#"{"status":"ok","retcode":0,"data":{"message_id":123}}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            }
        });
        let mut calls = 0;
        let started = Instant::now();
        let result = pipeline
            .run_cli_turn(&Target::Group(42), "fixture", &cfg, "fixture", |_| {
                calls += 1;
                Ok((
                    "fixture".into(),
                    "private draft must never be logged or sent".into(),
                ))
            })
            .await;
        let elapsed = started.elapsed();
        pipeline.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        let ledger = crate::turn_ledger::read_records(&context).unwrap();
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        drop((pending, console));
        std::fs::remove_dir_all(root).unwrap();
        eprintln!("CLI unavailable-console fixture: saturated={saturated} elapsedMs={} providerCalls={calls} onebotRequests={}",
            elapsed.as_millis(), seen.lock().unwrap().len());
        assert!(
            seen.lock().unwrap().is_empty(),
            "unreachable console must never bypass the send guard via OneBot"
        );
        assert_eq!(
            calls, 1,
            "console failure ends this turn without another provider call"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "console connect failure took {elapsed:?}"
        );
        assert_eq!(
            result.unwrap_err(),
            "CLI reply send failed: companion console unavailable"
        );
        assert!(
            ledger.iter().any(|r| r["k"] == "ended"
                && r["outcome"] == "failed"
                && r["reason"] == "CLI reply send failed: companion console unavailable"),
            "{ledger:?}"
        );
    }
}

#[tokio::test]
async fn hf123_fix1_cli_sentinel_checks_canonical_paths_and_known_secrets() {
    use serde_json::json;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (root, _) = isolated_fixture("hf123-fix1-cli-sentinel");
    write_selected_secret(&root, [0x44; 32]);
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({
            "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:2",
            "onebot":format!("http://127.0.0.1:{port}")
        })
        .to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root)
        .unwrap()
        .with_console_port(port);
    let cfg = context.load_config().unwrap();
    let mut pipeline = Pipeline::new_with_runtime(&context, &cfg, true).unwrap();
    pipeline.secrets = vec!["opaque-fixture-value-Q7N4".into()];
    pipeline.begin_ledger_turn("group:42").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (stopping, requests) = (stop.clone(), seen.clone());
    let worker = std::thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            if let Some((mut stream, request)) =
                fake_dsh_accept(&listener, std::time::Duration::from_millis(50))
            {
                requests.lock().unwrap().push(request);
                let body =
                    json!({"ok":true,"sent":1,"status":"ok","retcode":0,"data":{"message_id":123}})
                        .to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        }
    });
    let mut results = Vec::new();
    for text in [
        "/ro[表情]ot/fixture.txt",
        "/ro[表情:摘要]ot/fixture.txt",
        "/ro[表情[表情]]ot/fixture.txt",
        "opaque-fixture-[表情]value-Q7N4",
        "opaque-fixture-[表情:摘要]value-Q7N4",
        "opaque-fixture-[表情[表情]]value-Q7N4",
    ] {
        results.push(
            pipeline
                .run_cli_turn(&Target::Group(42), "fixture", &cfg, "fixture", |_| {
                    Ok(("fixture".into(), text.into()))
                })
                .await,
        );
    }
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    pipeline.end_ledger_turn(Ok(()));
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        results
            .iter()
            .all(|r| r.as_ref().err().is_some_and(|e| e.contains("出站哨兵"))),
        "canonical paths and known secrets must be refused: {results:?}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "refused CLI text must reach neither console send nor OneBot"
    );
}

#[tokio::test]
async fn hf123_cli_protocol_empty_initial_draft_keeps_fallback_but_silent_ends() {
    for (text, expected) in [("", None), ("[SILENT]", Some(0))] {
        let (root, context) = isolated_fixture("hf123-cli-empty");
        write_selected_secret(&root, [0x44; 32]);
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        let cfg = context.load_config().unwrap();
        let mut pipeline = Pipeline::new_with_runtime(&context, &cfg, true).unwrap();
        pipeline.begin_ledger_turn("group:42").unwrap();
        let mut calls = 0;
        let result = pipeline
            .run_cli_turn(&Target::Group(42), "fixture", &cfg, "fixture", |_| {
                calls += 1;
                Ok(("fixture".to_owned(), text.to_owned()))
            })
            .await;
        pipeline.end_ledger_turn(Ok(()));
        std::fs::remove_dir_all(root).expect("remove the completed empty CLI fixture");
        assert_eq!(calls, 1);
        assert_eq!(
            result.unwrap(),
            expected,
            "only an empty initial response may fall back"
        );
    }
}

async fn hf123_cli_protocol(revisions: usize) {
    hf123_cli_protocol_with_hold(revisions, std::time::Duration::ZERO).await;
}

async fn hf123_cli_protocol_with_hold(revisions: usize, response_delay: std::time::Duration) {
    hf123_cli_protocol_with_outcome(revisions, response_delay, None).await;
}

#[tokio::test]
async fn hf123_cli_protocol_console_failure_is_terminal_and_content_free() {
    for status in [500, 503, 200] {
        hf123_cli_protocol_with_outcome(0, std::time::Duration::ZERO, Some(status)).await;
    }
}

async fn hf123_cli_protocol_with_outcome(
    revisions: usize,
    response_delay: std::time::Duration,
    failure_status: Option<u16>,
) {
    use serde_json::{json, Value};
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = fake_runtime_root("hf123-cli-protocol");
    write_selected_secret(&root, [0x44; 32]);
    let cfg = json!({"consoleToken":"fixture-token","allow":{"groups":[42]},
        "socialV2":{"sticker":{"enabled":false}}});
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({
        "dsh":format!("http://127.0.0.1:{port}"),"model":"http://127.0.0.1:1",
        "onebot":format!("http://127.0.0.1:{port}")})
        .to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root)
        .unwrap()
        .with_console_port(port);
    let mut pipeline = Pipeline::new_with_runtime(&context, &cfg, true).unwrap();
    pipeline.begin_ledger_turn("group:42").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (stopping, requests) = (stop.clone(), seen.clone());
    let worker = std::thread::spawn(move || {
        let mut calls = 0;
        while !stopping.load(Ordering::SeqCst) {
            if let Some((mut stream, request)) =
                fake_dsh_accept(&listener, std::time::Duration::from_millis(50))
            {
                let (status, body) = if request.path == "/api/socialV2/send-message" {
                    calls += 1;
                    if let Some(status) = failure_status {
                        (
                            status,
                            json!({"ok":false,"error":"PRIVATE_RELAY_FAILURE_MUST_NOT_ESCAPE"}),
                        )
                    } else if calls <= revisions {
                        (
                            200,
                            json!({"ok":true,"held":true,"reviseRequired":true,
                        "newMessages":[{"messageId":format!("new-123-{calls}"),"seq":calls+1,"text":format!("new-question-{calls}")}],
                        "readThroughSeq":2,"partial":false,"replyRevisionsUsed":1}),
                        )
                    } else if revisions > 0 {
                        (200, json!({"ok":true,"sent":1}))
                    } else {
                        (429, json!({"ok":false,"error":"send guard refused"}))
                    }
                } else {
                    // The old CLI bypass talks directly to OneBot. Let it complete so the
                    // assertions identify the wrong route rather than a fixture failure.
                    (
                        200,
                        json!({"status":"ok","retcode":0,"data":{"message_id":123}}),
                    )
                };
                requests.lock().unwrap().push(request);
                std::thread::sleep(response_delay);
                let body = body.to_string();
                write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        }
    });
    let mut prompts = Vec::new();
    let result = pipeline
        .run_cli_turn(
            &Target::Group(42),
            "original prompt",
            &cfg,
            "fixture",
            |prompt| {
                prompts.push(prompt.to_string());
                Ok((
                    "fixture".to_owned(),
                    if prompts.len() == 1 {
                        "obsolete[表情14]"
                    } else {
                        "revised[表情28]"
                    }
                    .to_owned(),
                ))
            },
        )
        .await;
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    let requests = seen.lock().unwrap();
    let paths: Vec<_> = requests.iter().map(|r| r.path.as_str()).collect();
    let payloads: Vec<Value> = requests
        .iter()
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    pipeline.end_ledger_turn(if result.is_ok() {
        Ok(())
    } else {
        Err("fixture failure")
    });
    std::fs::remove_dir_all(&root).expect("remove the completed CLI protocol fixture");
    if failure_status.is_none() {
        assert!(result.is_ok(), "{result:?}");
    }
    assert!(
        paths.iter().all(|p| *p == "/api/socialV2/send-message"),
        "CLI must share the authenticated tool route: {paths:?}"
    );
    if let Some(status) = failure_status {
        assert_eq!(
            result.unwrap_err(),
            format!("CLI reply send failed: HTTP {status}")
        );
        assert_eq!(
            prompts.len(),
            1,
            "console failures must not replay the provider"
        );
        assert_eq!(
            paths.len(),
            1,
            "console failures must not fall back to OneBot"
        );
    } else if revisions > 0 {
        assert_eq!(
            prompts.len(),
            revisions + 1,
            "each discarded draft needs exactly one new model response"
        );
        for prior in 1..=revisions {
            assert!(
                prompts[revisions].contains(&format!("new-question-{prior}")),
                "the final draft must retain every held input: missing revision {prior}"
            );
        }
        assert_eq!(payloads[1]["messages"], json!(["revised[表情28]"]));
        assert!(
            pipeline
                .carried_messages
                .iter()
                .any(|r| r["messageId"] == "new-123-1"),
            "new delivered input must retain refusal provenance"
        );
    } else {
        assert_eq!(
            result.unwrap(),
            Some(0),
            "a refusal ends the turn without another send or model fallback"
        );
        assert_eq!(prompts.len(), 1);
    }
}
