// Ticket 24 console tests that compile against the integration tip as well (red evidence there).
use super::*;

fn raw_request(port: u16, head: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let request = format!("{head}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("status line");
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn ticket24_console_front_refuses_foreign_hosts_non_json_writes_and_cross_site_origins() {
    let token = "fixture-ticket24-front-token-abcdefghij";
    let (root, context) = fixture_runtime("票据24 front", token);
    let config_path = root.0.join("config/config.json");
    let original = std::fs::read(&config_path).unwrap();
    let (port, server) = start_http_fixture(context, 8);
    let ok_host = format!("127.0.0.1:{port}");

    // DNS rebinding: a foreign Host is refused before credentials are looked at.
    let (status, body) = raw_request(port, &format!("GET /api/status HTTP/1.1\r\nHost: evil.example:{port}\r\nx-console-token: {token}\r\n"), "");
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], format!("Host 头不被允许（evil.example:{port}）：控制台只接受本机回环访问"));
    let (status, body) = raw_request(port, &format!("GET /api/status HTTP/1.1\r\nx-console-token: {token}\r\n"), "");
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "Host 头不被允许（缺失）：控制台只接受本机回环访问");

    // A credentialed write must be declared JSON.
    let (status, body) = raw_request(port, &format!("POST /api/security HTTP/1.1\r\nHost: {ok_host}\r\nx-console-token: {token}\r\n"), r#"{"interceptNotify":false}"#);
    assert_eq!(status, 415, "{body}");
    assert_eq!(body["error"], "请求必须是 application/json");
    // A credentialed write from another site is refused.
    let (status, body) = raw_request(port, &format!("POST /api/security HTTP/1.1\r\nHost: {ok_host}\r\nx-console-token: {token}\r\nContent-Type: application/json\r\nOrigin: http://evil.example\r\n"), r#"{"interceptNotify":false}"#);
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "跨站请求被拒绝");
    // Without a credential the answer stays 401 (credentials precede the CSRF checks).
    let (status, _) = raw_request(port, &format!("POST /api/security HTTP/1.1\r\nHost: {ok_host}\r\n"), r#"{"interceptNotify":false}"#);
    assert_eq!(status, 401);
    assert_eq!(std::fs::read(&config_path).unwrap(), original, "a refused write changed the config");

    // The loopback page itself (any spelling of the loopback authority) is accepted.
    let (status, body) = raw_request(port, &format!("POST /api/security HTTP/1.1\r\nHost: LOCALHOST:{port}\r\nx-console-token: {token}\r\nContent-Type: application/json; charset=utf-8\r\nOrigin: http://localhost:{port}\r\n"), r#"{"interceptNotify":false}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["security"]["interceptNotify"], false);
    let (status, _) = raw_request(port, &format!("GET /api/security HTTP/1.1\r\nHost: [::1]:{port}\r\nx-console-token: {token}\r\n"), "");
    assert_eq!(status, 200);
    let (status, _) = raw_request(port, &format!("GET /api/health HTTP/1.1\r\nHost: probe.invalid\r\n"), "");
    assert_eq!(status, 200, "the health probe stays a token-free, host-free liveness check");
    server.join().unwrap();
}

#[test]
fn ticket24_safe_undo_refuses_to_overwrite_a_later_change() {
    let token = "fixture-ticket24-undo-token-abcdefghij";
    let (root, context) = fixture_runtime("票据24 safe undo", token);
    let (port, server) = start_http_fixture(context.clone(), 2);
    for model in ["first-model", "second-model"] {
        let body = json!({"key": "private:7001", "provider": "fixture", "model": model}).to_string();
        let (status, reply) = http_json(port, "POST", "/api/session-models", token, Some(&body));
        assert_eq!(status, 200, "{reply}");
    }
    server.join().unwrap();
    let log = std::fs::read_to_string(root.0.join("data/audit/audit.jsonl")).unwrap();
    let entries: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).filter(|e: &Value| e["path"] == "/api/session-models").collect();
    assert_eq!(entries.len(), 2);
    let after_second = std::fs::read(root.0.join("config/config.json")).unwrap();
    // Undoing the first change would silently drop the second one.
    let result = audit_undo(&context, &json!({"id": entries[0]["id"]}).to_string());
    assert_eq!(result.as_ref().map_err(|e| e.0).unwrap_err(), 409, "{result:?}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), after_second);
    // Undoing the latest change is safe, and then the earlier one is too.
    audit_undo(&context, &json!({"id": entries[1]["id"]}).to_string()).unwrap();
    let config: Value = serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(config["sessions"]["private:7001"]["model"], "first-model");
    audit_undo(&context, &json!({"id": entries[0]["id"]}).to_string()).unwrap();
    let config: Value = serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap();
    assert!(config.get("sessions").is_none(), "{config}");
}

#[test]
fn ticket24_retry_of_an_unresolved_send_turn_requires_confirmation() {
    let (_root, context) = fixture_runtime("票据24 retry confirm", "fixture-ticket24-retry-token-abcdefghij");
    let key = "private:7001";
    let unresolved = crate::turn_ledger::Recorder::begin(&context, key, "dsh", "atMention", "epoch", &[], None).unwrap();
    unresolved.unknown("send-unresolved", "batch b-1 has an unknown outcome").unwrap();
    let unreachable = crate::turn_ledger::Recorder::begin(&context, key, "dsh", "atMention", "epoch", &[], None).unwrap();
    unreachable.unknown("dsh-unreachable", "probe failed").unwrap();

    // A retry may repeat a message whose delivery is unknown: the administrator must confirm it.
    let refused = handle_turns_resolve(&context, &json!({"turnId": unresolved.id, "action": "retry"}).to_string());
    let (status, message) = refused.unwrap_err();
    assert_eq!(status, 409);
    assert!(message.contains("confirm"), "{message}");
    let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(&context).unwrap());
    assert_eq!(turns.iter().find(|t| t.id == unresolved.id).unwrap().state, crate::turn_ledger::State::Paused);
    let accepted = handle_turns_resolve(&context, &json!({"turnId": unresolved.id, "action": "retry", "confirm": true}).to_string()).unwrap();
    assert_eq!(accepted["action"], "retry");
    assert_eq!(accepted["confirmed"], true);
    // Other paused turns and abandoning need no confirmation.
    handle_turns_resolve(&context, &json!({"turnId": unreachable.id, "action": "retry"}).to_string()).unwrap();
    let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(&context).unwrap());
    for id in [&unresolved.id, &unreachable.id] {
        assert_eq!(turns.iter().find(|t| &t.id == id).unwrap().state, crate::turn_ledger::State::Resolved);
    }
}

#[test]
fn abandoning_a_paused_send_unresolved_turn_revokes_the_turn_agent_token() {
    let (_root, context) = fixture_runtime("gap abandon token", "fixture-gap-abandon-token-abcdefghij");
    let key = "private:7001";
    let token = |c: &RuntimeContext| c.read_social_state().unwrap()["conversations"][key]["agentToken"].as_str().unwrap_or("").to_string();
    let issued = ensure_or_rotate_agent_token(&context, key, false).unwrap();
    let paused = |reason: &str| {
        let turn = crate::turn_ledger::Recorder::begin(&context, key, "dsh", "atMention", "epoch", &[], None).unwrap();
        turn.unknown(reason, "batch b-1 has an unknown outcome").unwrap();
        turn
    };
    // Another pause reason keeps the token: only a send of unknown delivery is revoked.
    let other = paused("dsh-unreachable");
    handle_turns_resolve(&context, &json!({"turnId": other.id, "action": "abandon"}).to_string()).unwrap();
    assert_eq!(token(&context), issued, "abandoning a dsh-unreachable turn must not touch the token");
    // The decision to retry keeps it too (the turn may run again under the same epoch).
    let unresolved = paused("send-unresolved");
    handle_turns_resolve(&context, &json!({"turnId": unresolved.id, "action": "retry", "confirm": true}).to_string()).unwrap();
    assert_eq!(token(&context), issued, "a confirmed retry keeps the token");
    let unresolved = paused("send-unresolved");
    let answer = handle_turns_resolve(&context, &json!({"turnId": unresolved.id, "action": "abandon"}).to_string()).unwrap();
    assert_eq!(answer["action"], "abandon");
    let after = token(&context);
    assert!(!after.is_empty() && after != issued, "abandoning a send-unresolved turn must issue a new token, got {after:?}");
    assert!(!agent_token_ok_for(&context, key, &issued).unwrap(), "the old token must no longer authenticate");
}
