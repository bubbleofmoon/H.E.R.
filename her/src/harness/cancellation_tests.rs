use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn respond_once(
    listener: Arc<TcpListener>,
    response: String,
    before_response: impl FnOnce(),
) -> Value {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("fixture received no request")
        .unwrap();
    let mut bytes = Vec::new();
    let (split, length) = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "truncated fixture request");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..split])
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse::<usize>()
                .unwrap();
            if bytes.len() >= split + 4 + length {
                break (split + 4, length);
            }
        }
    };
    let request = serde_json::from_slice(&bytes[split..split + length]).unwrap();
    before_response();
    socket.write_all(response.as_bytes()).await.unwrap();
    request
}

#[tokio::test(flavor = "current_thread")]
async fn config_read_error_during_an_admitted_turn_preserves_the_failure_reason() {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-93-config-{}", crate::turn_ledger::mint_id(""))),
    );
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("http://{}", model.local_addr().unwrap());
    let cfg = json!({
        "ownerQQ":8808, "consoleToken":"synthetic-console-token-93",
        "harness":{"model":"fake-chat"}
    });
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(
        root.join("config/harness-api-key"),
        "synthetic-model-key-93",
    )
    .unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root).unwrap();
    let token =
        crate::console::ensure_or_rotate_agent_token(&context, "private:8808", false).unwrap();
    let generation = crate::send_outbox::token_epoch(&token);
    let chunk = json!({"id":"fake-93-config","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{"role":"assistant","content":"must not be delivered"},"finish_reason":null}]});
    let end = json!({"id":"fake-93-config","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let config_path = root.join("config/config.json");
    // Only remove the config once the admitted turn has reached the model boundary.
    let model_task = tokio::spawn(respond_once(model, response, move || {
        std::fs::remove_file(config_path).unwrap();
    }));
    let harness = Harness::open(&context).unwrap();
    let mut turn = harness
        .admit(TurnRequest {
            id: "config-read-error".into(),
            conversation: "private:8808".into(),
            generation: generation.clone(),
            prompt: "reply".into(),
            profile: "closed-agent".into(),
            route: ModelRoute::from_runtime(&context, &cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: Tools::capture(&context, &cfg, "private:8808", &generation).unwrap(),
            limits: Limits::from_config(&cfg),
        })
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("config read failure did not end the turn");
    assert_eq!(model_task.await.unwrap()["stream"], true);
    assert_eq!(result, Err("runtime path is unavailable".into()));
    assert!(
        matches!(turn.status(), Status::Failed(reason) if reason == "runtime path is unavailable")
    );
    assert!(matches!(
        Harness::open(&context).unwrap().status("config-read-error").unwrap(),
        Some(Status::Failed(reason)) if reason == "runtime path is unavailable"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn revoked_authority_settles_a_submitted_send_without_starting_the_next_call() {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-93-cancel-{}", crate::turn_ledger::mint_id(""))),
    );
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let console = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("http://{}", model.local_addr().unwrap());
    let cfg = json!({
        "ownerQQ":8808, "consoleToken":"synthetic-console-token-93",
        "harness":{"model":"fake-chat"}
    });
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(
        root.join("config/harness-api-key"),
        "synthetic-model-key-93",
    )
    .unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root)
        .unwrap()
        .with_console_port(console.local_addr().unwrap().port());
    let token =
        crate::console::ensure_or_rotate_agent_token(&context, "private:8808", false).unwrap();
    let generation = crate::send_outbox::token_epoch(&token);
    let calls = ["submitted-send", "after-revocation"].map(|id| json!({
        "index":if id == "submitted-send" { 0 } else { 1 },
        "id":id,"type":"function",
        "function":{"name":"qq_send_private_message","arguments":json!({"userId":8808,"message":id}).to_string()}
    }));
    let chunk = json!({"id":"fake-93","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{"role":"assistant","tool_calls":calls},"finish_reason":null}]});
    let end = json!({"id":"fake-93","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
    let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let model_task = tokio::spawn(respond_once(model.clone(), response, || {}));
    let revoke_context = context.clone();
    // The real handler has submitted its HTTP send. Revoke before acknowledging it,
    // with no pipeline cancellation poll or Turn::cancel notification to win the race.
    let console_task = tokio::spawn(respond_once(
        console.clone(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}".into(),
        move || crate::console::revoke_agent_token(&revoke_context, "private:8808", &token).unwrap(),
    ));
    let harness = Harness::open(&context).unwrap();
    let mut turn = harness
        .admit(TurnRequest {
            id: "revoked-in-send".into(),
            conversation: "private:8808".into(),
            generation: generation.clone(),
            prompt: "submit two sends".into(),
            profile: "closed-agent".into(),
            route: ModelRoute::from_runtime(&context, &cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: Tools::capture(&context, &cfg, "private:8808", &generation).unwrap(),
            limits: Limits::from_config(&cfg),
        })
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("revoked turn did not settle");
    assert!(result.is_err());
    assert!(turn.sent_tool_reply(), "submitted send result was lost");
    assert_eq!(console_task.await.unwrap()["message"], "submitted-send");
    assert_eq!(model_task.await.unwrap()["stream"], true);
    let db = rusqlite::Connection::open(root.join("data/conversations.db")).unwrap();
    let runs: Vec<(String, String)> = db
        .prepare("SELECT call_id,state FROM tool_runs ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        runs,
        vec![("submitted-send".into(), "completed".into())],
        "another tool started after authority revocation"
    );
    assert!(matches!(turn.status(), Status::Cancelled));
    assert!(matches!(
        harness.status("revoked-in-send").unwrap(),
        Some(Status::Cancelled)
    ));
    let kinds: Vec<String> = db
        .prepare("SELECT kind FROM events ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(&kinds[kinds.len() - 2..], &["tool_result", "turn_ended"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), console.accept())
            .await
            .is_err(),
        "another send reached the handler"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), model.accept())
            .await
            .is_err(),
        "model continued after revocation"
    );
}
