// Ticket 24 console tests: access changes act immediately, audit and safe undo.
use super::*;

const GROUP: &str = "group:420042";

fn seeded(label: &str, token: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, _) = fixture_runtime(label, token);
    let mut config: Value = serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap();
    config["allow"]["groups"] = json!([420042]);
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    std::fs::write(
        root.0.join("data/social-v2.json"),
        json!({"conversations": {GROUP: {"agentToken": "old-group-token-abcdefghij", "bootstrapSent": true, "unread": [], "recentMessages": [],
            "wakeConfig": {"mode": "diving", "infinite": true, "confirmedAt": 1, "triggers": {"atMention": true}}}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {GROUP: "dsh-old-session"}, "sessionPolicies": {GROUP: "[\"reserved2\",\"qq-chat-v2\",null]"}}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root.0).unwrap();
    (root, context)
}

fn group_token(context: &RuntimeContext) -> Option<String> {
    conversation_token(context, GROUP).unwrap()
}

fn audit_entries(context: &RuntimeContext) -> Vec<Value> {
    let (status, recent) = admin_request(context, "GET", "/api/audit/recent?limit=50", None);
    assert_eq!(status, 200, "{recent}");
    recent["entries"].as_array().cloned().unwrap_or_default()
}

#[test]
fn ticket24_whitelist_revocation_is_immediate_and_undo_grants_only_a_new_epoch() {
    let (root, context) = seeded("票据24 revoke undo", "fixture-ticket24-revoke-token-abcdefghij");
    let open_turn = crate::turn_ledger::Recorder::begin(&context, GROUP, "dsh", "atMention", "epoch", &[], None).unwrap();
    let old = "old-group-token-abcdefghij";
    assert_eq!(admin_request(&context, "POST", "/api/authorize/read", Some(&json!({"key": GROUP, "token": old}))).0, 200);

    // Revoke: the response comes back only after the old epoch is gone.
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["allow"]["groups"], json!([]));
    let rotated = group_token(&context).expect("the record keeps a fresh token");
    assert_ne!(rotated, old, "revocation must rotate the conversation token");
    assert_eq!(context.read_social_state().unwrap()["conversations"][GROUP]["bootstrapSent"], false);
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the DSH mapping must be retired");
    let records = crate::turn_ledger::read_records(&context).unwrap();
    assert!(records.iter().any(|r| r["k"] == "reset" && r["key"] == GROUP && r["why"] == "access-revoked"), "{records:?}");
    let (turns, _) = crate::turn_ledger::fold(&records);
    let turn = turns.iter().find(|t| t.id == open_turn.id).unwrap();
    assert_eq!(turn.state, crate::turn_ledger::State::Cancelled);
    assert_eq!(turn.outcome_reason, "access-revoked");
    let (status, reply) = admin_request(&context, "POST", "/api/authorize/read", Some(&json!({"key": GROUP, "token": old})));
    assert_eq!((status, reply["error"].as_str()), (403, Some("agent token 无效")));

    // Undo restores the configuration and grants access again, under the new epoch only.
    let entries = audit_entries(&context);
    let revoke = entries.iter().find(|e| e["path"] == "/api/whitelist").expect("whitelist audited");
    assert_eq!(revoke["ok"], true);
    assert_eq!(revoke["files"][0]["file"], "config/config.json");
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": revoke["id"]})));
    assert_eq!(status, 200, "{reply}");
    let config = context.load_config().unwrap();
    assert!(crate::console::key_allowed(&config, GROUP), "undo must restore the allow list");
    assert_eq!(group_token(&context).as_deref(), Some(rotated.as_str()), "undo must not bring back the old token");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "undo must not bring back the old session");
    let (status, _) = admin_request(&context, "POST", "/api/authorize/read", Some(&json!({"key": GROUP, "token": old})));
    assert_eq!(status, 403, "the revoked token stays revoked after undo");
    let (status, _) = admin_request(&context, "POST", "/api/authorize/read", Some(&json!({"key": GROUP, "token": rotated})));
    assert_eq!(status, 200);
    let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(&context).unwrap());
    assert_eq!(turns.iter().find(|t| t.id == open_turn.id).unwrap().state, crate::turn_ledger::State::Cancelled, "a cancelled turn is not revived");

    // The undo is itself audited, and the revocation can no longer be undone over it.
    let entries = audit_entries(&context);
    assert_eq!(entries[0]["path"], "/api/audit/undo");
    assert_eq!(entries[0]["files"][0]["file"], "config/config.json");
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": revoke["id"]})));
    assert_eq!(status, 409, "{reply}");
    // Undoing the undo is an ordinary access change: it revokes again, with a newer epoch.
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entries[0]["id"]})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["revoked"], json!([GROUP]));
    let third = group_token(&context).unwrap();
    assert_ne!(third, rotated);
    drop(root);
}

#[test]
fn ticket24_closed_agent_owner_change_retires_every_mapping_and_rotates_tokens() {
    let (root, context) = seeded("票据24 owner change", "fixture-ticket24-owner-token-abcdefghij");
    let owner_key = "private:7001";
    std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    let mut state = context.read_social_state().unwrap();
    state["conversations"][owner_key] = json!({"agentToken": "old-owner-token-abcdefghij", "bootstrapSent": true});
    std::fs::write(root.0.join("data/social-v2.json"), state.to_string()).unwrap();
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {owner_key: "dsh-owner-session"}, "sessionPolicies": {owner_key: "[\"closed-agent\",\"\",7001]"}}).to_string(),
    )
    .unwrap();
    // Owner A -> B -> A between two ticks of the live process.
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"ownerQQ": "10001"})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["ownerQQ"], 10001);
    let (status, _) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"ownerQQ": 7001})));
    assert_eq!(status, 200);
    assert_eq!(crate::session_map::lookup(&context, owner_key).unwrap(), None);
    let token = conversation_token(&context, owner_key).unwrap().unwrap();
    assert_ne!(token, "old-owner-token-abcdefghij", "the owner session's epoch must end even when the owner comes back");
    let records = crate::turn_ledger::read_records(&context).unwrap();
    assert!(records.iter().filter(|r| r["k"] == "reset" && r["key"] == owner_key).count() >= 1);
    drop(root);
}

#[test]
fn ticket24_audit_records_admin_actions_and_refusals_without_request_secrets() {
    let (root, context) = seeded("票据24 audit trail", "fixture-ticket24-audit-token-abcdefghij");
    let replacement = "fixture-ticket24-replacement-token-0123";
    let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&json!({"token": replacement})));
    assert_eq!(status, 200, "{reply}");
    let (status, reply) = admin_request(&context, "POST", "/api/socialV2/wake", Some(&json!({"key": "group:abc"})));
    assert_eq!(status, 400, "{reply}");
    let (status, _) = admin_request(&context, "POST", "/api/socialV2/wake", Some(&json!({"key": GROUP})));
    assert_eq!(status, 200);
    let (status, _) = admin_request(&context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_eq!(status, 200);
    let entries = audit_entries(&context);
    let summary: Vec<(String, u64, bool)> = entries
        .iter()
        .map(|e| (e["path"].as_str().unwrap().to_string(), e["status"].as_u64().unwrap(), e["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("/api/socialV2/reset".to_string(), 200, true),
            ("/api/socialV2/wake".to_string(), 200, true),
            ("/api/socialV2/wake".to_string(), 400, false),
            ("/api/console/token".to_string(), 200, true),
        ]
    );
    assert_eq!(entries[1]["key"], GROUP);
    assert_eq!(entries[2]["error"], crate::console::agent::KEY_FORMAT_ERROR);
    let log = std::fs::read_to_string(root.0.join("data/audit/audit.jsonl")).unwrap();
    assert!(!log.contains(replacement), "the audit log must not carry the new console token");
    assert!(audit_blob_texts(&root).iter().all(|b| !b.contains(replacement)), "an audit blob carries the new console token");
    // The token change keeps a blob of the previous configuration: it must not carry the previous
    // (manual) console token either.
    let blobs = audit_blob_texts(&root);
    assert!(!blobs.is_empty(), "the token change keeps a blob of the previous config");
    assert!(!log.contains("fixture-ticket24-audit-token-abcdefghij"));
    assert!(blobs.iter().all(|b| !b.contains("fixture-ticket24-audit-token-abcdefghij")), "an audit blob carries the previous manual console token");
    drop(root);
}

#[test]
fn ticket24_audit_and_access_routes_refuse_agents_and_missing_credentials() {
    let token = "fixture-ticket24-agent-token-abcdefghij";
    let (_root, context) = seeded("票据24 agent refusal", token);
    let (port, server) = start_http_fixture(context, 6);
    for (method, path, body) in [
        ("GET", "/api/audit/recent", None),
        ("POST", "/api/audit/undo", Some("{\"id\":\"x\"}")),
        ("POST", "/api/whitelist", Some("{}")),
        ("POST", "/api/console/token", Some("{}")),
        ("POST", "/api/socialV2/reset", Some("{}")),
    ] {
        let (status, reply) = http_json_with_agent(port, method, path, token, Some("old-group-token-abcdefghij"), body);
        assert_eq!((status, reply["error"].as_str()), (403, Some("该接口仅控制台可用")), "{method} {path}");
    }
    assert_eq!(http_status_without_token(port, "/api/audit/recent"), 401);
    server.join().unwrap();
}

// ── review fixes (ticket 24 fix round) ───────────────────────────────────────────────────────────

fn audit_blob_texts(root: &FixtureRoot) -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() { walk(&path, out) } else { out.push(std::fs::read_to_string(&path).unwrap_or_default()) }
        }
    }
    let mut out = Vec::new();
    walk(&root.0.join("data/audit/blobs"), &mut out);
    out
}

fn raw_config(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap()
}

/// One request with an explicit console token through `serve_request` (the TCP path).
fn request_with_token(context: &RuntimeContext, token: &str, method: &str, path: &str) -> (u16, Value) {
    let port = 3999;
    let headers = HashMap::from([
        ("x-console-token".to_string(), token.to_string()),
        ("host".to_string(), format!("127.0.0.1:{port}")),
        ("content-type".to_string(), "application/json".to_string()),
    ]);
    let req = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers,
        body: if method == "GET" { String::new() } else { "{\"triggerProbability\":0.25}".to_string() },
        body_too_large: false,
    };
    let cfg = context.load_config().unwrap_or_else(|_| json!({}));
    let (status, _, text) = serve_request(&req, &cfg, context, port);
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// S1: the generated console token is the hex of 24 bytes from the entropy source, nothing else.
#[test]
fn ticket24_generated_console_token_is_the_hex_of_os_entropy() {
    let mut fill = |bytes: &mut [u8]| {
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(1);
        }
        Ok(())
    };
    let token = generate_console_token_from(&mut fill).unwrap();
    let expected: String = (0..24u8).map(|i| format!("{:02x}", i.wrapping_mul(7).wrapping_add(1))).collect();
    assert_eq!(token, expected, "the token must be exactly the entropy bytes (not time or pid)");
    assert!(generate_console_token_from(&mut |_: &mut [u8]| Err("no entropy".to_string())).is_err(), "no entropy, no token");
    let (a, b) = (generate_console_token().unwrap(), generate_console_token().unwrap());
    assert_ne!(a, b);
    for token in [&a, &b] {
        assert_eq!(token.len(), 48);
        assert!(token.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "{token}");
    }
}

/// S4: a conversation with no records anywhere (allowed, first turn not bound yet) still loses
/// its epoch on revocation, and an undo does not bring the old epoch back.
#[test]
fn ticket24_revoking_a_conversation_without_records_ends_its_epoch() {
    let (root, context) = seeded("票据24 revoke no records", "fixture-ticket24-norecord-token-abcdefghij");
    let fresh = "group:420099";
    let mut config = raw_config(&root);
    config["allow"]["groups"] = json!([420042, 420099]);
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let before = crate::turn_ledger::reset_count(&context, fresh).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}})));
    assert_eq!(status, 200, "{reply}");
    let after = crate::turn_ledger::reset_count(&context, fresh).unwrap();
    assert!(after > before, "a queued job of {fresh} would still run: no reset record");
    let revoke = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/whitelist").unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": revoke["id"]})));
    assert_eq!(status, 200, "{reply}");
    assert!(crate::console::key_allowed(&context.load_config().unwrap(), fresh));
    assert_eq!(crate::turn_ledger::reset_count(&context, fresh).unwrap(), after, "the undo grants a new epoch only");
    // Allow-all: a conversation never listed and without records anywhere is still reached when
    // the policy narrows (Codex 6).
    let unknown = "group:420088";
    let mut config = raw_config(&root);
    config["allow"]["groups"] = json!([]);
    config["allowAllWhenEmpty"] = json!(true);
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    assert!(crate::console::access_allowed(&context.load_config().unwrap(), "reserved2", unknown));
    let before = crate::turn_ledger::reset_count(&context, unknown).unwrap();
    let kept_before = crate::turn_ledger::reset_count(&context, GROUP).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}})));
    assert_eq!(status, 200, "{reply}");
    assert!(!crate::console::access_allowed(&context.load_config().unwrap(), "reserved2", unknown));
    assert!(crate::turn_ledger::reset_count(&context, unknown).unwrap() > before, "a queued job of {unknown} (allow-all, no records) would survive the narrowing");
    assert_eq!(crate::turn_ledger::reset_count(&context, GROUP).unwrap(), kept_before, "a conversation that stays allowed keeps its queued work");
    drop(root);
}

/// S4: a key known only from an open ledger turn (chat's first turn before its session binds) is
/// revoked when the allow-all policy narrows.
#[test]
fn ticket24_revocation_reaches_a_key_known_only_from_an_open_ledger_turn() {
    let (root, context) = seeded("票据24 revoke ledger only", "fixture-ticket24-ledgeronly-token-abcdefghij");
    let only_turn = "group:420077";
    let mut config = raw_config(&root);
    config["allow"]["groups"] = json!([]);
    config["allowAllWhenEmpty"] = json!(true);
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let turn = crate::turn_ledger::Recorder::begin(&context, only_turn, "dsh", "chat", "epoch", &[], None).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}})));
    assert_eq!(status, 200, "{reply}");
    let records = crate::turn_ledger::read_records(&context).unwrap();
    assert!(records.iter().any(|r| r["k"] == "reset" && r["key"] == only_turn), "no reset record for {only_turn}: {records:?}");
    let (turns, _) = crate::turn_ledger::fold(&records);
    assert_eq!(turns.iter().find(|t| t.id == turn.id).unwrap().state, crate::turn_ledger::State::Cancelled, "its open turn must be cancelled");
    drop(root);
}

/// S5: a revocation that cannot be completed is reported, and the steps that can still run (token
/// rotation, mapping retirement) run anyway.
#[test]
fn ticket24_a_failed_revocation_is_reported_and_still_rotates_what_it_can() {
    let (root, context) = seeded("票据24 revoke failure", "fixture-ticket24-failure-token-abcdefghij");
    std::fs::create_dir_all(root.0.join("data/turn-ledger.jsonl")).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}})));
    assert_ne!(status, 200, "a revocation without its ledger record must not answer success: {reply}");
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap_or("").contains(GROUP), "{reply}");
    assert_ne!(group_token(&context).as_deref(), Some("old-group-token-abcdefghij"), "the token must still be rotated");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the mapping must still be retired");
    drop(root);
}

/// S5: the interaction gate is taken before the configuration changes; if it cannot be taken
/// nothing is written.
#[test]
fn ticket24_whitelist_refuses_without_the_interaction_gate_and_writes_nothing() {
    let (root, context) = seeded("票据24 gate busy", "fixture-ticket24-gatebusy-token-abcdefghij");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let held = crate::closed_agent::lock(&context).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}})));
    drop(held);
    assert_eq!(status, 503, "{reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing may be written");
    assert_eq!(group_token(&context).as_deref(), Some("old-group-token-abcdefghij"));
    drop(root);
}

/// S5: an undo whose restored configuration cannot be loaded is refused before anything is written
/// (its access reconciliation could not run).
#[test]
fn ticket24_undo_refuses_a_backup_the_runtime_cannot_load() {
    let (root, context) = seeded("票据24 undo invalid", "fixture-ticket24-undoinvalid-token-abcdefghij");
    let (status, _) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042, 420043]}})));
    assert_eq!(status, 200);
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/whitelist").unwrap();
    let backup = entry["files"][0]["backup"].as_str().unwrap().to_string();
    std::fs::write(root.0.join("data").join(&backup), "[]").unwrap();
    let current = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    assert_ne!(status, 200, "{reply}");
    assert_eq!(reply["ok"], false);
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), current, "nothing may be written");
    drop(root);
}

/// S6: the console token write and the undo take the social-state gate: a held gate refuses them
/// and nothing is written.
#[test]
fn ticket24_token_and_undo_wait_for_the_config_gate() {
    let (root, context) = seeded("票据24 config gate", "fixture-ticket24-configgate-token-abcdefghij");
    let (status, _) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042, 420043]}})));
    assert_eq!(status, 200);
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/whitelist").unwrap();
    let current = std::fs::read(root.0.join("config/config.json")).unwrap();
    let held = acquire_social_state_gate(&context).unwrap();
    let (token_status, token_reply) =
        admin_request(&context, "POST", "/api/console/token", Some(&json!({"token": "fixture-aaaaaaaaaaaaaaaaaaaaaaaa"})));
    let (undo_status, undo_reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    drop(held);
    assert_eq!(token_status, 503, "{token_reply}");
    assert_eq!(undo_status, 503, "{undo_reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), current, "nothing may be written while the gate is held");
    assert!(!root.0.join("data/console-token").exists());
    drop(root);
}

/// S6: if the token file cannot be written, the configuration keeps the token the operator has.
#[test]
fn ticket24_token_change_is_not_half_written_when_the_token_file_fails() {
    let old = "fixture-ticket24-halfwrite-token-abcdefghij";
    let (root, context) = seeded("票据24 token half write", old);
    std::fs::create_dir_all(root.0.join("data/console-token")).unwrap();
    for body in [json!({"token": "fixture-ticket24-halfwrite-replacement"}), json!({})] {
        let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&body));
        assert_eq!(status, 500, "{reply}");
        assert_eq!(raw_config(&root)["consoleToken"], old, "the operator would be locked out: {body}");
        assert_eq!(context.load_config().unwrap()["consoleToken"], old);
    }
    std::fs::remove_dir_all(root.0.join("data/console-token")).unwrap();
    // Post-write failure: the token file is written, then config.json cannot be replaced (its
    // directory is immutable). The token file must be rolled back so the old token still works.
    #[cfg(target_os = "linux")]
    {
        let previous = "previous-file-console-token-abcdefghij";
        std::fs::write(root.0.join("data/console-token"), previous).unwrap();
        struct UnwritableDir {
            path: std::path::PathBuf,
            original_permissions: Option<std::fs::Permissions>,
        }
        impl Drop for UnwritableDir {
            fn drop(&mut self) {
                if let Some(permissions) = self.original_permissions.take() {
                    let _ = std::fs::set_permissions(&self.path, permissions);
                } else {
                    let _ = std::process::Command::new("chattr").arg("-i").arg(&self.path).status();
                }
            }
        }
        let config_dir = root.0.join("config");
        let is_root = std::process::Command::new("id").arg("-u").output().map(|out| out.stdout == b"0\n").unwrap_or(false);
        let original_permissions = if is_root {
            assert!(std::process::Command::new("chattr").arg("+i").arg(&config_dir).status().unwrap().success(), "chattr +i failed");
            None
        } else {
            let original = std::fs::metadata(&config_dir).unwrap().permissions();
            let mut readonly = original.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&config_dir, readonly).unwrap();
            Some(original)
        };
        let guard = UnwritableDir { path: config_dir, original_permissions };
        let before = std::fs::read(root.0.join("config/config.json")).unwrap();
        let mut outcomes = Vec::new();
        for body in [json!({"token": "fixture-ticket24-halfwrite-replacement"}), json!({})] {
            let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&body));
            outcomes.push((status, reply, std::fs::read_to_string(root.0.join("data/console-token")).unwrap_or_default()));
        }
        drop(guard);
        assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before);
        for (status, reply, file) in outcomes {
            assert_ne!(status, 200, "{reply}");
            assert_eq!(file, previous, "the token file kept a token the operator never received: {reply}");
        }
        assert_eq!(raw_config(&root)["consoleToken"], old);
    }
    drop(root);
}

/// S7: a console-token record is not undoable (it would bring back the previous token).
#[test]
fn ticket24_undo_of_a_console_token_change_is_refused() {
    let (root, context) = seeded("票据24 undo token", "fixture-ticket24-undotoken-token-abcdefghij");
    let replacement = "fixture-ticket24-undotoken-replacement";
    let (status, _) = admin_request(&context, "POST", "/api/console/token", Some(&json!({"token": replacement})));
    assert_eq!(status, 200);
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/console/token").unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    assert_ne!(status, 200, "{reply}");
    assert_eq!(reply["ok"], false);
    assert_eq!(raw_config(&root)["consoleToken"], replacement, "the previous console token must not come back");
    drop(root);
}

/// AC6: undoing a record that changed no file says that nothing was restored (not success).
#[test]
fn ticket24_undo_of_a_record_without_files_is_not_a_success() {
    let (root, context) = seeded("票据24 undo nothing", "fixture-ticket24-undonothing-token-abcdefghij");
    let (status, _) = admin_request(&context, "POST", "/api/socialV2/wake", Some(&json!({"key": GROUP})));
    assert_eq!(status, 200);
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/socialV2/wake").unwrap();
    assert_eq!(entry["files"], json!([]));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    assert_ne!(status, 200, "{reply}");
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().is_some_and(|e| !e.is_empty()), "{reply}");
    drop(root);
}

/// Decision B: a generated console token is kept only in the hardened `data/console-token`;
/// `config.json` gets "" (JS), and neither the configuration nor the audit trail carries it.
#[test]
fn ticket24_generated_console_token_lives_only_in_the_hardened_token_file() {
    let (root, context) = seeded("票据24 generated token", "fixture-ticket24-generated-token-abcdefghij");
    let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    let token = reply["token"].as_str().unwrap().to_string();
    assert_eq!(reply["generated"], true);
    assert_eq!(raw_config(&root)["consoleToken"], "", "JS writes \"\" to config.json for a generated token");
    assert_eq!(std::fs::read_to_string(root.0.join("data/console-token")).unwrap(), token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.0.join("data/console-token")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the token file must be private: {mode:o}");
    }
    assert_eq!(request_with_token(&context, &token, "GET", "/api/whitelist").0, 200, "the console authenticates with the file token");
    // An ordinary configuration change afterwards keeps "" in config.json.
    let (status, reply) = request_with_token(&context, &token, "POST", "/api/social");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(raw_config(&root)["consoleToken"], "", "a later config write put the generated token into config.json");
    assert!(!std::fs::read_to_string(root.0.join("config/config.json")).unwrap().contains(&token));
    assert!(!std::fs::read_to_string(root.0.join("data/audit/audit.jsonl")).unwrap().contains(&token));
    let blobs = audit_blob_texts(&root);
    assert!(!blobs.is_empty(), "the token change keeps a blob of the previous config");
    assert!(blobs.iter().all(|b| !b.contains(&token)), "an audit blob carries the generated token");
    // A later rotation keeps the previous generated token out of the blobs too.
    let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    assert!(audit_blob_texts(&root).iter().all(|b| !b.contains(&token)));
    drop(root);
}

/// Decision C: the whole reset keeps only in-flight sticker liabilities; every other field of the
/// record and every other top-level field is gone (a kept record is a fresh one).
#[test]
fn ticket24_whole_reset_keeps_only_sticker_liabilities() {
    let (root, context) = seeded("票据24 reset liabilities", "fixture-ticket24-liability-token-abcdefghij");
    let mut state = context.read_social_state().unwrap();
    let liabilities = json!({
        "stickerSendPending": [{"id": "send-1", "time": 1}],
        "stickerCollectPending": [{"id": "collect-1", "time": 1}],
        "stickerSendProtected": [{"id": "protected-1", "time": 1}],
    });
    let record = state["conversations"][GROUP].as_object_mut().unwrap();
    for (field, value) in liabilities.as_object().unwrap() {
        record.insert(field.clone(), value.clone());
    }
    for (field, value) in [
        ("unread", json!([{"seq": 1}])), ("recentMessages", json!([{"seq": 1}])), ("sendTimes", json!([5])),
        ("stickerCollectTimes", json!([5])), ("wakeTimes", json!([5])), ("lastAiReplyAt", json!(5)), ("lastActionAt", json!(5)),
        ("lastWakeReason", json!("atMention")), ("wakeConfig", json!({"mode": "active", "operatorMarker": true})), ("adminWakes", json!([{"reason": "admin"}])), ("lastUnreadSeq", json!(9)), ("custom", json!(1)),
    ] {
        record.insert(field.to_string(), value);
    }
    state["conversations"]["group:420043"] = json!({"agentToken": "other-token-abcdefghij", "unread": [{"seq": 1}]});
    state["paused"] = json!(true);
    state["stray"] = json!({"x": 1});
    std::fs::write(root.0.join("data/social-v2.json"), state.to_string()).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    let after = context.read_social_state().unwrap();
    let mut top: Vec<&String> = after.as_object().unwrap().keys().collect();
    top.sort();
    assert_eq!(top, vec!["conversations", "resetEpoch"]);
    let conversations = after["conversations"].as_object().unwrap();
    assert_eq!(conversations.keys().collect::<Vec<_>>(), vec![GROUP], "only the conversation with liabilities keeps a record");
    let kept = &conversations[GROUP];
    let fresh = new_conversation(&context);
    let mut expected_fields: Vec<String> = fresh.as_object().unwrap().keys().cloned().collect();
    expected_fields.extend(liabilities.as_object().unwrap().keys().cloned());
    expected_fields.sort();
    let mut fields: Vec<String> = kept.as_object().unwrap().keys().cloned().collect();
    fields.sort();
    assert_eq!(fields, expected_fields, "a field other than the liabilities survived the reset");
    for (field, value) in fresh.as_object().unwrap() {
        if matches!(field.as_str(), "agentToken" | "wakeConfig") {
            continue;
        }
        assert_eq!(&kept[field], value, "{field} kept a pre-reset value");
    }
    assert_ne!(kept["agentToken"], "old-group-token-abcdefghij");
    assert_eq!(kept["wakeConfig"].get("operatorMarker"), None, "the wake configuration is a fresh default");
    assert_eq!(kept["wakeConfig"]["mode"], fresh["wakeConfig"]["mode"]);
    for field in ["stickerSendPending", "stickerCollectPending"] {
        assert_eq!(kept[field][0]["id"], liabilities[field][0]["id"]);
        assert_eq!(kept[field][0]["revoked"], true, "a pending reservation cannot authorize a later submission");
    }
    assert_eq!(kept["stickerSendProtected"], liabilities["stickerSendProtected"]);
    drop(root);
}

// ── second fix round (ticket 24) ─────────────────────────────────────────────────────────────────

const OWNER_KEY: &str = "private:7001";
const OWNER_AGENT_TOKEN: &str = "owner-agent-token-abcdefghij";

fn with_owner_session(root: &FixtureRoot, context: &RuntimeContext) {
    let mut state = context.read_social_state().unwrap();
    state["conversations"][OWNER_KEY] = json!({"agentToken": OWNER_AGENT_TOKEN, "bootstrapSent": true});
    std::fs::write(root.0.join("data/social-v2.json"), state.to_string()).unwrap();
}

fn write_raw_config(root: &FixtureRoot, config: &Value) {
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(config).unwrap()).unwrap();
}

fn reply_of(result: Result<Value, (u16, String)>) -> (u16, Value) {
    match result {
        Ok(value) => (200, value),
        Err((status, error)) => (status, json!({"ok": false, "error": error})),
    }
}

/// F2 / Codex 1: a generated console token never reaches `config.json` through a later config
/// write, nor an audit blob, nor `settings-get`; the memory console check compares with the
/// configured token only (JS `cfg.consoleToken`, "" after generation).
#[test]
fn ticket24_fix2_a_generated_token_stays_out_of_config_settings_and_audit() {
    let (root, context) = seeded("票据24 fix2 generated", "fixture-ticket24-fix2-manual-token-abcdefghij");
    with_owner_session(&root, &context);
    let (status, reply) = admin_request(&context, "POST", "/api/console/token", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    let generated = reply["token"].as_str().unwrap().to_string();
    let writes = [
        ("/api/social", json!({"triggerProbability": 0.25})),
        ("/api/socialV2/config", json!({"send": {"maxSendPerMinute": 7}})),
        ("/api/session-models", json!({"key": GROUP, "model": "fixture-model-b"})),
        ("/api/roles/limit", json!({"maxInjectChars": 5000})),
        ("/api/slang/config", json!({"injectMax": 5})),
        ("/api/security", json!({"interceptNotify": false})),
        ("/api/whitelist", json!({"allow": {"groups": [420042, 420043]}})),
    ];
    for (path, body) in writes {
        let (status, reply) = admin_request(&context, "POST", path, Some(&body));
        assert_eq!(status, 200, "{path}: {reply}");
        let text = std::fs::read_to_string(root.0.join("config/config.json")).unwrap();
        assert_eq!(raw_config(&root)["consoleToken"], "", "{path} wrote a console token into config.json");
        assert!(!text.contains(&generated), "{path} wrote the generated token into config.json");
    }
    for (name, result) in [
        ("effort", handle_dsh_effort(&context, r#"{"reasoningEffort":"low"}"#)),
        ("model-chain", handle_model_chain_post(&context, r#"{"chain":[{"provider":"p","model":"m"}]}"#)),
    ] {
        result.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(raw_config(&root)["consoleToken"], "", "{name} wrote a console token into config.json");
    }
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(raw_config(&root)["consoleToken"], "");
    let args = json!({"key": OWNER_KEY, "token": OWNER_AGENT_TOKEN});
    let all = memory_tool_action(&context, "settings-get", &args).unwrap();
    assert!(!all.to_string().contains(&generated), "settings-get returned the generated token: {all}");
    let mut named = args.clone();
    named["settingKey"] = json!("consoleToken");
    let one = memory_tool_action(&context, "settings-get", &named).unwrap();
    assert!(!one.to_string().contains(&generated), "settings-get returned the generated token: {one}");
    let cfg = context.load_config().unwrap();
    let headers = HashMap::from([("x-console-token".to_string(), generated.clone())]);
    let acl = handle_memory_action("acl", &headers, &HashMap::new(), "{}", &cfg, &context);
    assert_eq!(acl.map(|_| 200).map_err(|e| e.0), Err(403), "JS compares the memory console check with cfg.consoleToken (\"\")");
    let log = std::fs::read_to_string(root.0.join("data/audit/audit.jsonl")).unwrap();
    assert!(!log.contains(&generated), "the audit log carries the generated token");
    let blobs = audit_blob_texts(&root);
    assert!(blobs.len() >= 8, "every config change keeps a blob: {}", blobs.len());
    assert!(blobs.iter().all(|b| !b.contains(&generated)), "an audit blob carries the generated token");
    assert_eq!(admin_request(&context, "GET", "/api/whitelist", None).0, 200, "the console still authenticates with the file token");
    drop(root);
}

/// Codex 3: every config read-modify-write waits for the social-state gate and reads the file
/// again inside it, so a revocation written meanwhile is never overwritten with the allow list the
/// writer would otherwise have read before the gate.
#[test]
fn ticket24_fix2_every_config_writer_waits_for_the_gate_and_rereads_inside_it() {
    let (root, context) = seeded("票据24 fix2 config gate", "fixture-ticket24-fix2-gate-token-abcdefghij");
    type Call = Box<dyn FnOnce(RuntimeContext) -> (u16, Value) + Send>;
    let post = |path: &'static str, body: Value| -> Call { Box::new(move |c: RuntimeContext| admin_request(&c, "POST", path, Some(&body))) };
    let writers: Vec<(&str, Call, fn(&Value) -> bool)> = vec![
        ("session-models", post("/api/session-models", json!({"key": GROUP, "model": "gate-model"})), |c| c["sessions"][GROUP]["model"] == "gate-model"),
        ("roles-limit", post("/api/roles/limit", json!({"maxInjectChars": 5001})), |c| c["role"]["maxInjectChars"] == 5001),
        ("social-v1", post("/api/social", json!({"triggerProbability": 0.25})), |c| c["social"]["triggerProbability"].as_f64() == Some(0.25)),
        ("socialV2-config", post("/api/socialV2/config", json!({"send": {"maxSendPerMinute": 9}})), |c| c["socialV2"]["send"]["maxSendPerMinute"] == 9),
        ("effort", Box::new(|c: RuntimeContext| reply_of(handle_dsh_effort(&c, r#"{"reasoningEffort":"low"}"#))), |c| c["dsh"]["reasoningEffort"] == "low"),
        (
            "model-chain",
            Box::new(|c: RuntimeContext| reply_of(handle_model_chain_post(&c, r#"{"chain":[{"provider":"gate-p","model":"gate-chain-model"}]}"#))),
            |c| c["dsh"]["model"] == "gate-chain-model",
        ),
    ];
    for (name, call, landed) in writers {
        let held = acquire_social_state_gate(&context).unwrap();
        let worker_context = context.clone();
        let worker = std::thread::spawn(move || call(worker_context));
        std::thread::sleep(std::time::Duration::from_millis(400));
        let while_held = raw_config(&root);
        // A revocation is written meanwhile, the way the whitelist handler writes it (under the gate).
        let mut revoked = while_held.clone();
        revoked["allow"]["groups"] = json!([]);
        write_raw_config(&root, &revoked);
        drop(held);
        let (status, reply) = worker.join().unwrap();
        assert_eq!(status, 200, "{name}: {reply}");
        assert!(!landed(&while_held), "{name} wrote config.json while the gate was held");
        let after = raw_config(&root);
        assert!(landed(&after), "{name}: its change was lost: {after}");
        assert_eq!(after["allow"]["groups"], json!([]), "{name} overwrote the revocation with the allow list it read before the gate");
        let mut restored = after.clone();
        restored["allow"]["groups"] = json!([420042]);
        write_raw_config(&root, &restored);
    }
    drop(root);
}

/// Codex 2: an audit record holds only the change its own request made. A console-token change
/// that lands while a security change waits for the gate is not attributed to the security change,
/// and undoing the security change never brings back the replaced token.
#[test]
fn ticket24_fix2_an_audit_record_holds_only_its_own_change() {
    let (root, context) = seeded("票据24 fix2 attribution", "fixture-ticket24-fix2-attr-token-abcdefghij");
    let held = acquire_social_state_gate(&context).unwrap();
    let worker_context = context.clone();
    let worker = std::thread::spawn(move || admin_request(&worker_context, "POST", "/api/security", Some(&json!({"interceptNotify": false}))));
    std::thread::sleep(std::time::Duration::from_millis(400));
    // Another administrator replaces the console token meanwhile (as the token handler writes it).
    let rotated = "fixture-ticket24-fix2-attr-rotated-abcdefghij";
    let mut cfg = raw_config(&root);
    cfg["consoleToken"] = json!(rotated);
    write_raw_config(&root, &cfg);
    drop(held);
    let (status, reply) = worker.join().unwrap();
    assert_eq!(status, 200, "{reply}");
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/security").unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    assert_eq!(status, 200, "{reply}");
    let after = raw_config(&root);
    assert_eq!(after["consoleToken"], rotated, "undoing the security change brought back the replaced console token");
    assert_ne!(after.pointer("/security/interceptNotify"), Some(&json!(false)), "the security change must be undone: {after}");
    drop(root);
}

/// Codex 13: an administrative change whose audit record or undo backup cannot be written keeps its
/// result but says so.
#[test]
fn ticket24_fix2_an_unwritable_audit_trail_is_reported_in_the_response() {
    let (root, context) = seeded("票据24 fix2 audit failure", "fixture-ticket24-fix2-auditfail-token-abcdefghij");
    std::fs::create_dir_all(root.0.join("data/audit/audit.jsonl")).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042, 420043]}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["audit"]["ok"], false, "an unwritable audit log must be reported: {reply}");
    assert!(reply["audit"]["error"].as_str().is_some_and(|e| !e.is_empty()), "{reply}");
    std::fs::remove_dir_all(root.0.join("data/audit")).unwrap();
    std::fs::create_dir_all(root.0.join("data/audit")).unwrap();
    std::fs::write(root.0.join("data/audit/blobs"), "not a directory").unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["audit"]["ok"], false, "an unwritable undo backup must be reported: {reply}");
    drop(root);
}

/// Codex 13: audit blobs never carry a manual console token, and an undo keeps the current token.
#[test]
fn ticket24_fix2_audit_blobs_never_carry_a_manual_console_token() {
    let manual = "fixture-ticket24-fix2-blob-manual-abcdefghij";
    let (root, context) = seeded("票据24 fix2 blob redaction", manual);
    let (status, _) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042, 420043]}})));
    assert_eq!(status, 200);
    let blobs = audit_blob_texts(&root);
    assert!(!blobs.is_empty());
    assert!(blobs.iter().all(|b| !b.contains(manual)), "an audit blob carries the manual console token");
    let entry = audit_entries(&context).into_iter().find(|e| e["path"] == "/api/whitelist").unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry["id"]})));
    assert_eq!(status, 200, "{reply}");
    let after = raw_config(&root);
    assert_eq!(after["consoleToken"], manual, "undo must keep the current console token");
    assert_eq!(after["allow"]["groups"], json!([420042]), "undo restores the rest of the configuration");
    drop(root);
}

/// Codex 9: an owner change is reconciled against the owner the runtime authorizes with (the DSH
/// owner overlay in closed-agent mode); a list-only change keeps that effective owner (JS writes
/// its in-memory `cfg.ownerQQ`).
#[test]
fn ticket24_fix2_owner_changes_follow_the_effective_owner() {
    let (root, context) = seeded("票据24 fix2 owner overlay", "fixture-ticket24-fix2-owner-token-abcdefghij");
    std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    std::fs::write(root.0.join("data/closed-agent-owner.json"), r#"{"ownerQQ":7001}"#).unwrap();
    let mut cfg = raw_config(&root);
    cfg["ownerQQ"] = json!(10001);
    write_raw_config(&root, &cfg);
    let mut state = context.read_social_state().unwrap();
    state["conversations"][OWNER_KEY] = json!({"agentToken": "owner-agent-token-abcdefghij", "bootstrapSent": true});
    std::fs::write(root.0.join("data/social-v2.json"), state.to_string()).unwrap();
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {OWNER_KEY: "dsh-owner-session"}, "sessionPolicies": {OWNER_KEY: "[\"closed-agent\",\"\",7001]"}}).to_string(),
    )
    .unwrap();
    assert_eq!(context.load_config().unwrap()["ownerQQ"], 7001, "the overlay names the effective owner");
    // The raw owner changes (10001 -> 10002) but the runtime still authorizes 7001: the
    // owner's session keeps its epoch.
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"ownerQQ": 10002})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(context.load_config().unwrap()["ownerQQ"], 7001);
    assert_eq!(crate::session_map::lookup(&context, OWNER_KEY).unwrap().as_deref(), Some("dsh-owner-session"), "an unchanged effective owner lost its session");
    assert_eq!(conversation_token(&context, OWNER_KEY).unwrap().as_deref(), Some("owner-agent-token-abcdefghij"));
    // A list-only change writes the effective owner (JS `cfg.ownerQQ ?? null`).
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042, 420043]}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(raw_config(&root)["ownerQQ"], 7001, "a list-only change must keep the effective owner");
    drop(root);
}

/// Codex 10 (JS identical for a request that names a list): a change that names `allow.groups`
/// rewrites `allow` from the canonical keys and drops the imported alias `allow.group`
/// (bridge.js 4170-4172), so a group the new list omits loses access; the revocation is reconciled
/// at once.
#[test]
fn ticket24_fix2_an_owner_only_change_revokes_an_aliased_group_immediately() {
    let (root, context) = seeded("票据24 fix2 alias", "fixture-ticket24-fix2-alias-token-abcdefghij");
    let mut cfg = raw_config(&root);
    cfg["allow"] = json!({"group": [420042], "private": ["7001"]});
    write_raw_config(&root, &cfg);
    assert!(crate::console::key_allowed(&context.load_config().unwrap(), GROUP), "the alias grants access");
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420043]}})));
    assert_eq!(status, 200, "{reply}");
    assert!(!crate::console::key_allowed(&context.load_config().unwrap(), GROUP));
    assert_ne!(group_token(&context).as_deref(), Some("old-group-token-abcdefghij"), "the lost access must be reconciled at once");
    drop(root);
}

/// Accepted deviation 20 (user decision 2026-10-30, defect fix): an owner-only change keeps the
/// existing allow lists as configured, alias spellings included; JS drops them (bridge.js 4170-4172).
#[test]
fn ticket24_fix2_an_owner_only_change_keeps_an_aliased_allow_list() {
    let (root, context) = seeded("票据24 fix2 alias keep", "fixture-ticket24-fix2-aliaskeep-token-abcdefghij");
    let mut cfg = raw_config(&root);
    cfg["allow"] = json!({"group": [420042], "private": ["7001"]});
    write_raw_config(&root, &cfg);
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"ownerQQ": 7001})));
    assert_eq!(status, 200, "{reply}");
    assert!(crate::console::key_allowed(&context.load_config().unwrap(), GROUP), "an owner-only change revoked the aliased group");
    assert_eq!(raw_config(&root)["allow"], json!({"group": [420042], "private": ["7001"]}), "the configured allow list must stay byte-for-byte as configured");
    drop(root);
}

/// Accepted deviation 19 (user decision 2026-10-30, security): settings-get never returns a
/// console token, manual or generated; JS returns `cfg.consoleToken` (bridge.js readSettingPath 10704).
#[test]
fn ticket24_fix2_settings_get_never_returns_a_manual_console_token() {
    let manual = "fixture-ticket24-fix2-settings-manual-abcdefghij";
    let (root, context) = seeded("票据24 fix2 settings manual", manual);
    with_owner_session(&root, &context);
    let named = json!({"key": OWNER_KEY, "token": OWNER_AGENT_TOKEN, "settingKey": "consoleToken"});
    let one = memory_tool_action(&context, "settings-get", &named).unwrap();
    assert!(!one.to_string().contains(manual), "{one}");
    drop(root);
}

/// Codex 11: a whole reset whose inbound journal cannot be settled reports it (restart before the
/// next tick would replay those events).
#[test]
fn ticket24_fix2_a_whole_reset_reports_an_unsettled_inbound_journal() {
    let (root, context) = seeded("票据24 fix2 reset journal", "fixture-ticket24-fix2-journal-token-abcdefghij");
    std::fs::create_dir_all(root.0.join("data/inbound.db")).unwrap();
    let (status, reply) = admin_request(&context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_ne!(status, 200, "a reset that could not settle the queued ingress answered success: {reply}");
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap_or("").contains("入站"), "{reply}");
    assert_eq!(context.read_social_state().unwrap()["conversations"], json!({}), "the reset itself still ran");
    drop(root);
}

/// Codex 14: a manual wake re-validates mode and access inside the state gate; a revocation that
/// lands while the wake waits for the gate refuses it.
#[test]
fn ticket24_fix2_a_manual_wake_revalidates_inside_the_gate() {
    let (root, context) = seeded("票据24 fix2 wake gate", "fixture-ticket24-fix2-wake-token-abcdefghij");
    let held = acquire_social_state_gate(&context).unwrap();
    let worker_context = context.clone();
    let worker = std::thread::spawn(move || admin_request(&worker_context, "POST", "/api/socialV2/wake", Some(&json!({"key": GROUP}))));
    std::thread::sleep(std::time::Duration::from_millis(400));
    // The revocation lands while the wake waits: list narrowed, token rotated (under the gate).
    let mut cfg = raw_config(&root);
    cfg["allow"]["groups"] = json!([]);
    write_raw_config(&root, &cfg);
    let mut state = context.read_social_state().unwrap();
    state["conversations"][GROUP]["agentToken"] = json!("rotated-group-token-abcdefghij");
    std::fs::write(root.0.join("data/social-v2.json"), state.to_string()).unwrap();
    drop(held);
    let (status, reply) = worker.join().unwrap();
    assert_eq!(status, 403, "a wake validated before the revocation was queued after it: {reply}");
    assert_eq!(context.read_social_state().unwrap()["conversations"][GROUP].get("adminWakes"), None);
    drop(root);
}

#[test]
fn gap_ledger_a_torn_ledger_tail_leaves_the_turns_view_answering() {
    let (root, context) = seeded("gap-ledger torn turns view", "fixture-gap-ledger-turns-token-abcdefghij");
    let turn = crate::turn_ledger::Recorder::begin(&context, GROUP, "dsh", "atMention", "epoch", &[], None).unwrap();
    crate::send_outbox::append_record(&context, &json!({"batch": "b1", "k": "intent", "key": GROUP, "epoch": "epoch", "messages": ["a"], "delays": [], "t": 1})).unwrap();
    for file in ["turn-ledger.jsonl", "send-outbox.jsonl"] {
        let mut bytes = std::fs::read(root.0.join("data").join(file)).unwrap();
        bytes.extend_from_slice(br#"{"k":"att"#);
        std::fs::write(root.0.join("data").join(file), bytes).unwrap();
    }
    let (status, reply) = admin_request(&context, "GET", "/api/turns", None);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["turns"][0]["turnId"], json!(turn.id), "{reply}");
    // The next appends terminate the torn fragments; they stay skipped and the new records are read.
    crate::send_outbox::append_record(&context, &json!({"batch": "b1", "k": "attempt", "idx": 0, "t": 2})).unwrap();
    turn.verified("note", "after the torn tail").unwrap();
    let (status, reply) = admin_request(&context, "GET", "/api/turns", None);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["turns"][0]["turnId"], json!(turn.id), "{reply}");
}
