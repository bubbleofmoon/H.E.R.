//! Ticket 67: plain chat through the harness, using only synthetic loopback services.
#![allow(dead_code)]
#[path = "fixtures/media.rs"]
mod media_fixtures;
include!("support/chat.rs");
include!("support/model.rs");

#[test]
fn dropped_stream_before_tools_retries_once_with_a_fresh_durable_attempt() {
    let model = FakeModel::script(vec![
        ModelReply::Truncated("discard this partial answer".into()),
        ModelReply::ToolCalls(vec![
            json!({"id":"after-retry","name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
        ]),
        ModelReply::Text("recovered answer".into()),
    ]);
    let mut rig = harness_rig("harness-safe-stream-retry", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "recover a dropped stream");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        vec![ACK.to_string(), "recovered answer".into()]
    );
    let requests = lock(&model.requests);
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0], requests[1],
        "retry changed the admitted request"
    );
    let facts = conversation_facts(&rig);
    let states: Vec<_> = facts["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["failed", "completed", "completed"]);
    assert_eq!(facts["turns"][0]["state"], "completed");
    assert!(!facts["events"]
        .to_string()
        .contains("discard this partial answer"));
}

fn harness_rig(name: &str, model: &FakeModel) -> Rig {
    let rig = Rig::new(
        name,
        Seed {
            config: json!({"conversationEngines": {GROUP_KEY: "harness"}, "harness": {"model": "fake-chat"}}),
            ..Seed::default()
        },
    );
    fs::write(
        rig.root.join("config/harness-api-key"),
        "synthetic-model-key-67",
    )
    .unwrap();
    let path = rig.root.join("dependencies.json");
    let mut deps: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    deps["model"] = json!(model.base);
    // Keep the selected DSH pointed at the recording fake, including login calls.
    fs::write(path, deps.to_string()).unwrap();
    rig
}

// OneBot sends have no notice kind. Match the fixed notice envelope and only the
// three conversations in this fixture; opening/recovery may split their notices.
fn restart_gap_notice_keys(text: &str) -> Option<Vec<&str>> {
    let (count, body) = text.strip_prefix("入站缺口 ")?.split_once(" 个：\n")?;
    let body = body.strip_suffix("\n详见管理端 /api/inbound/gaps")?;
    let mut keys = Vec::new();
    for line in body.lines() {
        let (key, detail) = line.split_once("：1 个，")?;
        let (interval, action) = detail.split_once('；')?;
        let expected = match key {
            OWNER_KEY => {
                "无法补收 1，私聊无历史，消息可能漏收，重要内容请重发，这是你与机器人的私聊"
            }
            "private:7707" => "无法补收 1，私聊无历史，消息可能漏收，重要内容请重发",
            GROUP_KEY => "待重试 1，退避到期后下次重连时重试",
            _ => return None,
        };
        if !interval.contains('–') || action != expected || keys.contains(&key) {
            return None;
        }
        keys.push(key);
    }
    (!keys.is_empty() && count.parse::<usize>().ok()? == keys.len()).then_some(keys)
}

#[test]
fn harness_group_chat_streams_one_reply_through_the_guarded_outbox_without_dsh() {
    let model = FakeModel::start(vec!["harness reply ✓"]);
    let mut rig = harness_rig("harness-roundtrip", &model);
    let deps: Value =
        serde_json::from_slice(&fs::read(rig.root.join("dependencies.json")).unwrap()).unwrap();
    let cfg: Value =
        serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    assert_eq!(
        deps["dsh"], cfg["dsh"]["baseUrl"],
        "no-DSH witness must be the DSH selected by this runtime"
    );
    rig.start();
    rig.push(group_event(
        67001,
        1001,
        "tester",
        "",
        vec![text_seg("hello harness")],
    ));
    assert!(
        wait_for(Duration::from_secs(8), || !lock(&model.requests).is_empty()),
        "chat never reached fake model"
    );
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 2),
        vec![ACK.to_string(), "harness reply ✓".to_string()]
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    assert_eq!(lock(&model.requests).len(), 1);
    let request = lock(&model.requests)[0].clone();
    assert_eq!(request["stream"], true);
    assert_eq!(request["model"], "fake-chat");
    assert_eq!(request["max_tokens"], 4096);
    assert!(request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.contains("tester：hello harness"))));
    let tools = request["tools"]
        .as_array()
        .expect("HER tools must be offered");
    assert!(tools
        .iter()
        .any(|t| t["function"]["name"] == "memory_index"));
    for forbidden in [
        "bash",
        "read",
        "plugin_manager",
        "memory_acl",
        "settings_set",
    ] {
        assert!(!tools.iter().any(|t| t["function"]["name"] == forbidden));
    }
    assert!(rig.dsh.prompts().is_empty());
    assert!(rig.dsh.calls("session/create").is_empty());
    assert!(rig.dsh.calls("workspace/create").is_empty());
    let unexpected: Vec<_> = lock(&rig.dsh.requests)
        .iter()
        .filter(|r| {
            // Runtime-wide health and the admin interaction subscription remain shared with DSH.
            r["body"]["method"] != "settings/describe" && r["body"]["endpoint"] != "$events"
        })
        .cloned()
        .collect();
    assert!(
        unexpected.is_empty(),
        "harness conversation called DSH (including login): {unexpected:?}"
    );
    assert!(
        rig.dsh.calls("agentPresets/list").is_empty(),
        "harness must not relogin"
    );
    assert!(rig.data_json("sessions.json")["sessions"][GROUP_KEY].is_null());
    let intents: Vec<_> = rig
        .data_lines("send-outbox.jsonl")
        .into_iter()
        .filter(|r| r["k"] == "intent" && r["purpose"] == "reply")
        .collect();
    assert_eq!(intents.len(), 1);
    // Re-delivery of the same inbound event must not create another turn or send.
    rig.push(group_event(
        67001,
        1001,
        "tester",
        "",
        vec![text_seg("hello harness")],
    ));
    thread::sleep(Duration::from_millis(700));
    assert_eq!(rig.texts_to(GROUP_KEY).len(), 2);
    assert_eq!(lock(&model.requests).len(), 1);
}

fn conversation_facts(rig: &Rig) -> Value {
    let path = rig.root.join("data/conversations.db");
    assert!(
        path.is_file(),
        "harness did not create its conversation database"
    );
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let rows = |sql: &str| -> Vec<Value> {
        let mut stmt = db.prepare(sql).unwrap();
        let values = stmt
            .query_map([], |r| {
                Ok(serde_json::from_str::<Value>(&r.get::<_, String>(0)?).unwrap())
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        values
    };
    json!({
        "sessions": rows("SELECT json_object('id',id,'conversation',conversation,'generation',generation) FROM sessions ORDER BY id"),
        "turns": rows("SELECT json_object('id',id,'state',state,'session',session_id) FROM turns ORDER BY begun_ms,id"),
        "attempts": rows("SELECT json_object('id',id,'turn',turn_id,'state',state,'model',model) FROM model_attempts ORDER BY rowid"),
        "events": rows("SELECT json_object('seq',seq,'kind',kind,'turn',turn_id,'attempt',attempt_id,'payload',json(payload)) FROM events ORDER BY seq"),
        "tools": rows("SELECT json_object('id',id,'turn',turn_id,'attempt',attempt_id,'call',call_id,'name',name,'state',state,'result',json(result)) FROM tool_runs ORDER BY rowid")
    })
}

#[test]
fn harness_conversation_facts_and_history_survive_a_bridge_and_console_restart() {
    let model = FakeModel::start(vec!["first harness reply", "second harness reply"]);
    let mut rig = harness_rig("harness-restart", &model);
    rig.start();
    rig.push(group_event(
        67002,
        1001,
        "tester",
        "",
        vec![text_seg("remember this turn")],
    ));
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "first harness reply");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    let before = conversation_facts(&rig);
    assert_eq!(before["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(before["turns"][0]["state"], "completed");
    assert_eq!(before["attempts"][0]["state"], "completed");
    assert_eq!(before["attempts"][0]["model"], "fake-chat");
    assert!(before["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant" && e["payload"]["text"] == "first harness reply"));
    let mut child = rig.candidate.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    rig.console.kill().unwrap();
    rig.console.wait().unwrap();
    fs::write(rig.root.join("candidate-out.log"), "").unwrap();
    fs::write(rig.root.join("candidate-err.log"), "").unwrap();
    rig.console = spawn_console(&rig.root, rig.console_port);
    assert!(wait_for(LONG, || http_call(
        rig.console_port,
        "GET",
        "/api/health",
        &[],
        None
    )
    .is_some()));
    rig.start();
    assert_eq!(
        conversation_facts(&rig),
        before,
        "restart must retain committed facts unchanged"
    );
    rig.push(group_event(
        67003,
        1001,
        "tester",
        "",
        vec![text_seg("continue our chat")],
    ));
    assert_eq!(rig.wait_texts(GROUP_KEY, 4)[3], "second harness reply");
    let second = lock(&model.requests)[1].clone();
    let messages = second["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| m["role"] == "user"
        && m["content"]
            .as_str()
            .is_some_and(|s| s.contains("remember this turn"))));
    assert!(messages
        .iter()
        .any(|m| m["role"] == "assistant" && m["content"] == "first harness reply"));
    assert!(messages.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .contains("continue our chat"));
    let after = conversation_facts(&rig);
    assert_eq!(after["sessions"], before["sessions"]);
    assert_eq!(after["turns"].as_array().unwrap().len(), 2);
    assert_eq!(after["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(
        &after["events"].as_array().unwrap()[..before["events"].as_array().unwrap().len()],
        before["events"].as_array().unwrap().as_slice()
    );
    assert!(rig.dsh.prompts().is_empty());
}

const MODEL_KEY: &str = "synthetic-model-key-67";

fn assert_model_key_is_private(rig: &Rig) {
    fn scan(root: &Path, path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(root, &path);
                continue;
            }
            if path == root.join("config/harness-api-key") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue, // rotating logs / SQLite sidecars
                Err(error) => panic!("cannot scan runtime artifact: {error}"),
            };
            assert!(
                !bytes
                    .windows(MODEL_KEY.len())
                    .any(|b| b == MODEL_KEY.as_bytes()),
                "model credential leaked into {}",
                path.strip_prefix(root).unwrap().display()
            );
        }
    }
    scan(&rig.root, &rig.root);
    #[cfg(target_os = "linux")]
    for pid in [rig.candidate.as_ref().unwrap().id(), rig.console.id()] {
        let argv = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(
            !argv
                .windows(MODEL_KEY.len())
                .any(|b| b == MODEL_KEY.as_bytes()),
            "model credential leaked into argv"
        );
    }
}

#[test]
fn runtime_model_key_is_used_for_auth_but_never_persisted_logged_or_relayed_even_when_echoed() {
    let echoed = format!("provider echoed {MODEL_KEY} safely");
    let model = FakeModel::start(vec![&echoed]);
    let mut rig = harness_rig("harness-key-echo", &model);
    rig.start();
    rig.push(group_event(
        67004,
        1001,
        "tester",
        "",
        vec![text_seg("credential echo test")],
    ));
    let texts = rig.wait_texts(GROUP_KEY, 2);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_model_key_is_private(&rig);
    assert_eq!(texts[1], "provider echoed [REDACTED] safely");
    assert_eq!(*lock(&model.authorizations), vec![true]);
    let facts = conversation_facts(&rig);
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"
            && e["payload"]["text"] == "provider echoed [REDACTED] safely"));
}

#[test]
fn provider_error_bodies_cannot_leak_the_model_key_or_trigger_a_dsh_fallback() {
    let model = FakeModel::script(vec![ModelReply::Error {
        status: 400,
        message: format!("invalid request; echoed {MODEL_KEY}"),
    }]);
    let mut rig = harness_rig("harness-key-error", &model);
    rig.start();
    rig.push(group_event(
        67005,
        1001,
        "tester",
        "",
        vec![text_seg("provider error test")],
    ));
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
    assert_eq!(lock(&model.requests).len(), 1);
    assert_eq!(*lock(&model.authorizations), vec![true]);
    assert!(rig.dsh.prompts().is_empty());
    assert_model_key_is_private(&rig);
    assert_eq!(conversation_facts(&rig)["attempts"][0]["state"], "failed");
}

#[test]
fn revoking_a_harness_conversation_cancels_the_model_wait_and_fences_late_output() {
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![ModelReply::Delayed {
        text: "stale reply".into(),
        release: release.clone(),
    }]);
    let mut rig = harness_rig("harness-revoked", &model);
    rig.start();
    rig.push(group_event(
        67006,
        1001,
        "tester",
        "",
        vec![text_seg("wait for revocation")],
    ));
    assert!(wait_for(LONG, || lock(&model.requests).len() == 1));
    rig.wait_texts(GROUP_KEY, 1);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["allow"]["groups"] = json!([]);
    fs::write(path, cfg.to_string()).unwrap();
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"][0]
        ["state"]
        == "cancelled"));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
    let facts = conversation_facts(&rig);
    assert_eq!(facts["attempts"][0]["state"], "cancelled");
    assert!(!facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"));
    assert!(!rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "intent" && r["purpose"] == "reply"));
    assert!(rig.dsh.prompts().is_empty());
}

fn stop_bridge(rig: &mut Rig) {
    let mut child = rig.candidate.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn interrupted_harness_accepted_send_requires_confirmation_before_retry() {
    let model = FakeModel::start(vec!["accepted reply"]);
    let mut rig = harness_rig("harness-uncertain-send", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "send once");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "accepted reply");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    stop_bridge(&mut rig);
    // Reproduce the durable crash boundary: QQ accepted, but the local send result and
    // bridge turn end were never recorded. Keep the real intent and sending records.
    let turn = rig
        .ledger()
        .into_iter()
        .find(|r| r["k"] == "begun")
        .unwrap();
    let id = turn["turn"].as_str().unwrap();
    for (file, records) in [
        (
            "turn-ledger.jsonl",
            rig.ledger()
                .into_iter()
                .filter(|r| !(r["turn"] == id && r["k"] == "ended"))
                .collect::<Vec<_>>(),
        ),
        (
            "send-outbox.jsonl",
            rig.data_lines("send-outbox.jsonl")
                .into_iter()
                .filter(|r| !matches!(r["k"].as_str(), Some("confirmed")))
                .collect(),
        ),
    ] {
        let text = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        fs::write(rig.root.join("data").join(file), text).unwrap();
    }
    rig.start();
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1
        ["pausedCount"]
        == 1));
    let response = rig.console(
        "POST",
        "/api/turns/resolve",
        Some(&json!({"turnId":id,"action":"retry"})),
    );
    assert_eq!(
        response.0, 409,
        "unconfirmed retry must refuse a possibly delivered harness reply: {response:?}"
    );
    assert_eq!(
        rig.console("GET", "/api/turns", None).1["paused"][0]["reason"],
        "send-unresolved"
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        vec![ACK.to_string(), "accepted reply".into()]
    );
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn resolving_an_interrupted_harness_turn_unblocks_only_that_turn() {
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![
        ModelReply::Delayed {
            text: "interrupted".into(),
            release: release.clone(),
        },
        ModelReply::Text("after resolution".into()),
    ]);
    let mut rig = harness_rig("harness-resolve-running", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "interrupted input");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 1));
    let id = conversation_facts(&rig)["turns"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    stop_bridge(&mut rig);
    // A separate conversation's running turn must survive resolution of this one.
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES('other-session','group:999','other-generation')",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms) VALUES('other-turn','other-session','group:999','test','running',0)", []).unwrap();
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.start();
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1
        ["pausedCount"]
        == 1));
    let response = rig.console(
        "POST",
        "/api/turns/resolve",
        Some(&json!({"turnId":id,"action":"abandon"})),
    );
    assert_eq!(response.0, 200, "{response:?}");
    assert!(
        wait_for(LONG, || {
            db.query_row("SELECT state FROM turns WHERE id=?1", [&id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
                != "running"
        }),
        "administrator resolution left the harness conversation blocked"
    );
    assert_eq!(
        db.query_row("SELECT state FROM turns WHERE id='other-turn'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "running"
    );
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "new input after resolution",
    );
    assert!(
        wait_for(LONG, || rig
            .texts_to(GROUP_KEY)
            .iter()
            .any(|s| s == "after resolution")),
        "conversation must admit new work after administrator resolution"
    );
}

fn reset_harness_history(command: &str) {
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![
        ModelReply::Text("old history reply".into()),
        ModelReply::Delayed {
            text: "late pre-reset reply".into(),
            release: release.clone(),
        },
        ModelReply::Text("fresh history reply".into()),
    ]);
    let mut rig = harness_rig(&format!("harness-reset-{}", &command[1..]), &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "old history input");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "old history reply");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let before = rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["agentToken"].clone();
    says(&rig, GROUP_KEY, 1001, "tester", "in flight before reset");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    rig.wait_texts(GROUP_KEY, 3);
    assert!(rig.data_json("sessions.json")["sessions"][GROUP_KEY].is_null());
    says(&rig, GROUP_KEY, OWNER, "owner", command);
    assert!(
        wait_for(Duration::from_secs(8), || rig
            .texts_to(GROUP_KEY)
            .iter()
            .any(|s| s.contains("已重置会话"))),
        "{command} did not reset a harness-only conversation"
    );
    assert_ne!(
        rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["agentToken"],
        before
    );
    assert!(rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "reset" && r["key"] == GROUP_KEY));
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["state"] == "cancelled")));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    says(&rig, GROUP_KEY, 1001, "tester", "fresh history input");
    assert!(
        wait_for(LONG, || rig
            .texts_to(GROUP_KEY)
            .iter()
            .any(|s| s == "fresh history reply")),
        "new generation was blocked by old harness work"
    );
    let texts = rig.texts_to(GROUP_KEY);
    assert!(!texts.iter().any(|s| s == "late pre-reset reply"));
    let request = lock(&model.requests)[2].to_string();
    assert!(request.contains("fresh history input"));
    for old in [
        "old history input",
        "old history reply",
        "in flight before reset",
        "late pre-reset reply",
    ] {
        assert!(
            !request.contains(old),
            "{command} retained history: {request}"
        );
    }
    let facts = conversation_facts(&rig);
    assert_eq!(facts["sessions"].as_array().unwrap().len(), 2);
    assert!(!facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant" && e["payload"]["text"] == "late pre-reset reply"));
}

#[test]
fn harness_reset_rotates_generation_and_fences_inflight_reply_without_dsh_mapping() {
    reset_harness_history("/reset");
}

#[test]
fn harness_new_resets_generation_and_fences_inflight_reply_without_dsh_mapping() {
    reset_harness_history("/new");
}

#[test]
fn model_fixture_reads_fragmented_requests_on_nonblocking_accepted_sockets() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let task = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        // Reproduce Windows inheritance even on Linux.
        socket.set_nonblocking(true).unwrap();
        read_model_request(&mut socket).map(|r| (r.request_line, r.body))
    });
    let mut client = TcpStream::connect(address).unwrap();
    client
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n")
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let _ = client.write_all(b"\r\n{}");
    assert_eq!(
        task.join().unwrap(),
        Some(("POST /v1/chat/completions HTTP/1.1".into(), b"{}".to_vec())),
        "accepted model sockets must be blocking before request timeouts"
    );
}

#[test]
fn cancelling_a_harness_turn_during_an_active_sse_stream_never_sends_or_completes() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![ModelReply::Active {
        text: "streaming reply must stay unsent".into(),
        started: started.clone(),
        release: release.clone(),
    }]);
    let mut rig = harness_rig("harness-active-cancel", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "cancel this active stream");
    assert!(
        wait_for(Duration::from_secs(8), || started
            .load(std::sync::atomic::Ordering::SeqCst)),
        "fixture never sent headers and an SSE content delta before cancellation"
    );
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["allow"]["groups"] = json!([]);
    fs::write(path, cfg.to_string()).unwrap();
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"][0]
        ["state"]
        == "cancelled"));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
    assert_eq!(
        conversation_facts(&rig)["attempts"][0]["state"],
        "cancelled"
    );
    assert!(!conversation_facts(&rig)["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"));
    assert!(!rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "intent" && r["purpose"] == "reply"));
}

#[test]
fn a_truncated_harness_sse_stream_never_sends_or_completes_the_turn() {
    let model = FakeModel::script(vec![
        ModelReply::Truncated("partial answer".into()),
        ModelReply::Truncated("another partial answer".into()),
    ]);
    let mut rig = harness_rig("harness-truncated", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "stream will be truncated");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        vec![ACK.to_string()],
        "a truncated stream must not send captured partial text"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["attempts"][0]["state"], "failed");
    assert_eq!(facts["attempts"][1]["state"], "failed");
    assert_eq!(lock(&model.requests).len(), 2, "a second drop must not retry");
    assert!(!facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"));
    assert!(!rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "intent" && r["purpose"] == "reply"));
}

#[test]
fn harness_completion_updates_only_its_active_model_attempt() {
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![ModelReply::Delayed {
        text: "attempt reply".into(),
        release: release.clone(),
    }]);
    let mut rig = harness_rig("harness-attempt-scope", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "finish active attempt");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 1));
    let id = conversation_facts(&rig)["turns"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    // A prior terminal attempt is durable history, not part of the active attempt's completion.
    db.execute("INSERT INTO model_attempts(id,turn_id,model,endpoint,state,begun_ms,ended_ms) VALUES('prior-attempt',?1,'old-model','fixture','failed',0,1)", [&id]).unwrap();
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "attempt reply");
    let previous: (String, i64) = db
        .query_row(
            "SELECT state,ended_ms FROM model_attempts WHERE id='prior-attempt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        previous,
        ("failed".into(), 1),
        "completion rewrote a different attempt's terminal facts"
    );
    let active: String = db
        .query_row(
            "SELECT id FROM model_attempts WHERE turn_id=?1 AND state='completed'",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM events WHERE turn_id=?1 AND attempt_id=?2 AND kind IN ('model_attempt_started','assistant','model_attempt_ended')", rusqlite::params![id,active], |r| r.get::<_,i64>(0)).unwrap(), 3);
}

#[test]
fn streamed_memory_tool_result_is_returned_to_the_model_before_the_final_reply() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"memory-1","name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
        ]),
        ModelReply::Text("memory checked".into()),
    ]);
    let mut rig = harness_rig("harness-memory-loop", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "check our memory");
    assert!(
        wait_for(Duration::from_secs(8), || lock(&model.requests).len() == 2),
        "tool result never reached the model"
    );
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "memory checked");
    let request = lock(&model.requests)[1].clone();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "assistant" && m["tool_calls"][0]["id"] == "memory-1"));
    assert!(messages.iter().any(|m| m["role"] == "tool"
        && m["tool_call_id"] == "memory-1"
        && m["content"].as_str().unwrap().contains("记忆库还是空的")));
    assert_eq!(
        conversation_facts(&rig)["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn owner_qq_tool_sends_once_with_durable_intent_before_effect_and_result_before_continuation() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"send-68","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"one tool send"}}),
            json!({"id":"after-send-memory","name":"memory_index","arguments":{"key":OWNER_KEY,"token":""}}),
        ]),
        ModelReply::Text("final answer after tool send".into()),
    ]);
    let mut rig = harness_rig("harness-owner-send", &model);
    fs::write(
        rig.root.join("data/mode.json"),
        json!({"mode":"closed-agent"}).to_string(),
    )
    .unwrap();
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["conversationEngines"][OWNER_KEY] = json!("harness");
    fs::write(&path, cfg.to_string()).unwrap();
    let witnessed = Arc::new(Mutex::new(Vec::new()));
    let seen = witnessed.clone();
    let root = rig.root.clone();
    lock(&rig.gateway).before_send = Some(Arc::new(move |body| {
        let text: String = body["message"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["data"]["text"].as_str())
            .collect();
        if text != "one tool send" {
            return;
        }
        let db = rusqlite::Connection::open_with_flags(
            root.join("data/conversations.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let intent: (String, Option<String>) = db
            .query_row(
                "SELECT state,result FROM tool_runs WHERE call_id='send-68'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let payload: String = db
            .query_row(
                "SELECT payload FROM events WHERE kind='assistant' ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        let message: genai::chat::ChatMessage =
            serde_json::from_value(payload["message"].clone()).unwrap();
        lock(&seen).push(
            json!({"state":intent.0,"result":intent.1,"calls":message.content.tool_calls().len()}),
        );
    }));
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "please send through the QQ tool",
    );
    assert!(
        wait_for(Duration::from_secs(8), || lock(&model.requests).len() == 2),
        "owner QQ tool never continued to a final model answer"
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    assert_eq!(
        rig.texts_to(OWNER_KEY),
        vec![ACK.to_string(), "one tool send".into()]
    );
    assert_eq!(
        *lock(&witnessed),
        vec![json!({"state":"running","result":null,"calls":2})]
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"][0]["state"], "completed");
    assert!(facts["events"].as_array().unwrap().iter().any(
        |e| e["kind"] == "assistant" && e["payload"]["text"] == "final answer after tool send"
    ));
    let requests = lock(&model.requests);
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "send-68"));
    drop(requests);
    let starts: Vec<_> = facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "tool_started")
        .collect();
    let results: Vec<_> = facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "tool_result")
        .collect();
    assert_eq!((starts.len(), results.len()), (2, 2));
    for (start, result) in starts.iter().zip(&results) {
        assert_eq!(start["payload"]["id"], result["payload"]["id"]);
        assert!(start["seq"].as_i64().unwrap() < result["seq"].as_i64().unwrap());
    }
    let next = facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "model_attempt_started")
        .nth(1)
        .unwrap();
    assert!(results
        .iter()
        .all(|result| result["seq"].as_i64().unwrap() < next["seq"].as_i64().unwrap()));
    assert_ne!(facts["attempts"][0]["id"], facts["attempts"][1]["id"]);
    assert_eq!(facts["tools"][0]["attempt"], facts["attempts"][0]["id"]);
    assert_eq!(
        rig.data_lines("send-outbox.jsonl")
            .iter()
            .filter(|r| r["k"] == "intent" && r["messages"] == json!(["one tool send"]))
            .count(),
        1
    );
    assert!(rig.dsh.prompts().is_empty());
    assert!(rig.dsh.calls("agentPresets/list").is_empty());
}

fn set_harness_config(rig: &Rig, harness: Value, timeout_secs: u64) {
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["harness"]
        .as_object_mut()
        .unwrap()
        .extend(harness.as_object().unwrap().clone());
    cfg["socialV2"]["turnTimeoutSecs"] = json!(timeout_secs);
    fs::write(path, cfg.to_string()).unwrap();
}

fn owner_harness_rig(name: &str, model: &FakeModel) -> Rig {
    let rig = harness_rig(name, model);
    fs::write(
        rig.root.join("data/mode.json"),
        json!({"mode":"closed-agent"}).to_string(),
    )
    .unwrap();
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["conversationEngines"][OWNER_KEY] = json!("harness");
    fs::write(path, cfg.to_string()).unwrap();
    rig
}

#[test]
fn owner_tokenless_image_lookup_keeps_the_closed_agent_permission() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"owner-image","name":"qq_get_message_images","arguments":{"key":OWNER_KEY,"messageId":"no-media"}}),
        ]),
        ModelReply::Text("image lookup complete".into()),
    ]);
    let mut rig = owner_harness_rig("harness-owner-image-access", &model);
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "look up an older image");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"][0]["state"], "completed", "{facts}");
    assert!(facts["tools"][0]["result"]
        .to_string()
        .contains("没有可返回的图片"));
    assert_eq!(rig.wait_texts(OWNER_KEY, 2)[1], "image lookup complete");
}

#[test]
fn mcp_images_reach_model_vision_and_replay_as_persisted_image_parts() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"vision-image","name":"qq_get_message_images","arguments":{"key":OWNER_KEY,"messageId":"older-image"}}),
            json!({"id":"after-image","name":"qq_status","arguments":{}}),
        ]),
        ModelReply::Text("image understood".into()),
        ModelReply::Text("image remembered".into()),
    ]);
    let mut rig = owner_harness_rig("harness-model-image", &model);
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{OWNER_KEY:{"recentMessages":[{"messageId":"older-image","media":[{"kind":"image","file":"ticket19.gif"}]}]}}}).to_string()).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "inspect an older image");
    assert_eq!(rig.wait_texts(OWNER_KEY, 2)[1], "image understood");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let assert_vision = |request: &Value| {
        let messages = request["messages"].as_array().unwrap();
        let (image_index, image) = messages
            .iter()
            .enumerate()
            .find_map(|(i, m)| {
                m["content"]
                    .as_array()?
                    .iter()
                    .find(|p| p["type"] == "image_url")
                    .map(|p| (i, p))
            })
            .expect("MCP image was escaped as text instead of model vision content");
        assert!(image["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/"));
        let result_index = messages
            .iter()
            .position(|m| m["tool_call_id"] == "after-image")
            .unwrap();
        assert!(
            image_index > result_index,
            "images must follow the complete call/result group"
        );
        for m in messages.iter().filter(|m| m["role"] == "tool") {
            assert!(
                !m["content"].as_str().unwrap().contains("\"data\":"),
                "base64 must not reach model text"
            );
        }
    };
    assert_vision(&lock(&model.requests)[1]);
    let facts = conversation_facts(&rig);
    let image_fact = facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["kind"] == "tool_result"
                && e["payload"]["result"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|b| b["type"] == "image")
        })
        .unwrap();
    let message: genai::chat::ChatMessage =
        serde_json::from_value(image_fact["payload"]["message"].clone()).unwrap();
    assert!(
        !message.content.binaries().is_empty(),
        "canonical history lost image parts"
    );
    stop_bridge(&mut rig);
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "recall the image");
    assert!(wait_for(LONG, || rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s == "image remembered")));
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(texts.iter().filter(|s| s.as_str() == ACK).count(), 2);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "image understood")
            .count(),
        1
    );
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "image remembered")
            .count(),
        1
    );
    assert!(texts.iter().all(|s| s == ACK
        || s == "image understood"
        || s == "image remembered"
        || restart_gap_notice_keys(s).is_some()));
    assert!(rig.sends().iter().all(|(key, _)| key == OWNER_KEY));
    assert_vision(&lock(&model.requests)[2]);
}

#[test]
fn harness_loop_budget_records_the_step_limit_without_more_model_requests() {
    let replies = (0..10).map(|step| ModelReply::ToolCalls(vec![json!({"id":format!("loop-{step}"),"name":"bash","arguments":{"command":"unoffered"}})])).collect();
    let model = FakeModel::script(replies);
    let mut rig = harness_rig("harness-step-budget", &model);
    set_harness_config(&rig, json!({"maxSteps":2}), 10);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "bounded tool loop");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(
        lock(&model.requests).len(),
        2,
        "configured step budget was ignored"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "turn_ended"
            && e["payload"]["result"]
                .as_str()
                .is_some_and(|r| r.contains("step limit"))));
    assert_eq!(facts["tools"].as_array().unwrap().len(), 2);
    assert!(facts["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["state"] == "refused"));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
}

#[test]
fn harness_loop_budget_records_token_exhaustion_before_a_requested_effect() {
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![
        json!({"id":"over-token-budget","name":"memory_write","arguments":{"key":GROUP_KEY,"token":"","content":"must not be written after exhaustion"}}),
    ])]);
    let mut rig = harness_rig("harness-token-budget", &model);
    set_harness_config(&rig, json!({"maxTokens":10}), 10);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "bounded token use");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert!(
        facts["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "turn_ended"
                && e["payload"]["result"]
                    .as_str()
                    .is_some_and(|r| r.contains("token limit"))),
        "token exhaustion had no recorded reason: {facts}"
    );
    assert_eq!(
        facts["tools"][0]["state"], "refused",
        "a tool ran after token exhaustion"
    );
    assert_eq!(lock(&model.requests).len(), 1);
    assert_eq!(lock(&model.requests)[0]["max_tokens"], 10);
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
}

#[test]
fn unoffered_invalid_and_cross_conversation_calls_are_recorded_refusals_without_effects() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"owner-only","name":"settings_set","arguments":{"key":GROUP_KEY,"token":"","settingKey":"sendDelayMs","value":999}}),
            json!({"id":"invalid-schema","name":"qq_send_message","arguments":{"key":GROUP_KEY,"token":"","messages":42}}),
            json!({"id":"cross-session","name":"memory_write","arguments":{"key":OWNER_KEY,"token":"","content":"cross-session write must be refused"}}),
            json!({"id":"local-exec","name":"bash","arguments":{"command":"must never run"}}),
        ]),
        ModelReply::Text("refusals handled".into()),
    ]);
    let mut rig = harness_rig("harness-refusals", &model);
    let config_before = fs::read(rig.root.join("config/config.json")).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "try disallowed tools");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "refusals handled");
    assert_eq!(
        fs::read(rig.root.join("config/config.json")).unwrap(),
        config_before
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"].as_array().unwrap().len(), 4);
    assert!(facts["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["state"] == "refused" && t["result"]["isError"] == true));
    let requests = lock(&model.requests);
    let messages = requests[1]["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 4);
    let results: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(results.len(), 4);
    for result in results {
        let outcome: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(outcome["isError"], true);
    }
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| matches!(
            t["function"]["name"].as_str(),
            Some("settings_set" | "bash")
        )));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        vec![ACK.to_string(), "refusals handled".into()]
    );
}

#[test]
fn auxiliary_her_tools_are_offered_and_dispatched_in_process() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"web-search","name":"web_search","arguments":{"query":"  \n  "}}),
            json!({"id":"web-fetch","name":"web_fetch","arguments":{"url":"http://127.0.0.1/blocked-by-existing-fetch-policy"}}),
            json!({"id":"host-status","name":"snowluma_status","arguments":{}}),
        ]),
        ModelReply::Text("auxiliary tools checked".into()),
    ]);
    let mut rig = harness_rig("harness-aux-tools", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "use safe auxiliary tools");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "auxiliary tools checked");
    let requests = lock(&model.requests);
    for name in ["web_search", "web_fetch", "snowluma_status"] {
        assert!(
            requests[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == name),
            "{name} was not offered"
        );
    }
    let results: Vec<Value> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(results[0]["isError"], true); // sanitized empty search: no external request
    assert!(results[0].to_string().contains("查询词为空"));
    assert_eq!(results[1]["isError"], true); // existing private-IP fetch policy
    assert_ne!(results[2]["isError"], true);
    assert!(results[2].to_string().contains("reachable"));
    assert!(rig.dsh.prompts().is_empty());
}

#[test]
fn harness_loop_wall_deadline_covers_tool_continuation_and_records_why_it_stopped() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"before-timeout","name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
        ]),
        ModelReply::Active {
            text: "late continuation must not send".into(),
            started: started.clone(),
            release: release.clone(),
        },
    ]);
    let mut rig = harness_rig("harness-wall-budget", &model);
    set_harness_config(&rig, json!({}), 3);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "bound the whole tool turn");
    assert!(wait_for(Duration::from_secs(8), || started
        .load(std::sync::atomic::Ordering::SeqCst)));
    assert!(wait_for(Duration::from_secs(8), || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["attempts"][0]["state"], "completed");
    assert_eq!(facts["attempts"][1]["state"], "failed");
    assert_eq!(facts["tools"][0]["state"], "completed");
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "turn_ended"
            && e["payload"]["result"]
                .as_str()
                .is_some_and(|r| r.contains("deadline exceeded"))));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
}

#[test]
fn canonical_tool_rounds_and_reasoning_replay_after_restart_with_attempt_scoped_call_ids() {
    let call = json!({"id":"provider-call-0","name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}});
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![call.clone()]),
        ModelReply::ToolCalls(vec![call]),
        ModelReply::Text("tool rounds complete".into()),
        ModelReply::Text("history replayed".into()),
    ]);
    let mut rig = harness_rig("harness-canonical-replay", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "remember our tool rounds");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2)[1], "tool rounds complete");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let before = conversation_facts(&rig);
    assert_eq!(before["tools"].as_array().unwrap().len(), 2);
    assert_ne!(before["tools"][0]["attempt"], before["tools"][1]["attempt"]);
    stop_bridge(&mut rig);
    rig.start();
    assert_eq!(conversation_facts(&rig), before);
    says(&rig, GROUP_KEY, 1001, "tester", "replay our history");
    assert_eq!(rig.wait_texts(GROUP_KEY, 4)[3], "history replayed");
    let request = lock(&model.requests)[3].clone();
    assert!(request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "memory_index"));
    let rounds: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool" || m["tool_calls"].is_array())
        .collect();
    assert_eq!(
        rounds.len(),
        4,
        "canonical call/result groups were lost on reopen"
    );
    for pair in rounds.chunks_exact(2) {
        assert_eq!(pair[0]["role"], "assistant");
        assert_eq!(pair[0]["tool_calls"][0]["id"], "provider-call-0");
        assert_eq!(pair[0]["reasoning_content"], "tool plan");
        assert_eq!(pair[1]["role"], "tool");
        assert_eq!(pair[1]["tool_call_id"], "provider-call-0");
        assert!(pair[1]["content"]
            .as_str()
            .unwrap()
            .contains("记忆库还是空的"));
    }
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        vec![
            ACK.to_string(),
            "tool rounds complete".into(),
            ACK.to_string(),
            "history replayed".into()
        ],
        "reasoning must never reach QQ"
    );
}

#[test]
fn truncated_streamed_tool_arguments_never_create_a_dispatch_intent_or_effect() {
    let call = json!({"id":"partial-tool","name":"memory_write","arguments":{"key":GROUP_KEY,"token":"","content":"partial call must not run"}});
    let model = FakeModel::script(vec![
        ModelReply::TruncatedTools(vec![call.clone()]),
        ModelReply::TruncatedTools(vec![call]),
    ]);
    let mut rig = harness_rig("harness-truncated-tools", &model);
    rig.start();
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "a tool stream will end early",
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert!(facts["tools"].as_array().unwrap().is_empty());
    assert!(!facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
    assert_eq!(lock(&model.requests).len(), 2);
}

#[test]
fn dropped_stream_after_a_tool_send_does_not_retry_or_repeat_the_effect() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"before-drop","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"sent before dropped stream"}}),
        ]),
        ModelReply::Truncated("uncommitted final answer".into()),
        ModelReply::Text("must not retry after the send".into()),
    ]);
    let mut rig = owner_harness_rig("harness-unsafe-stream-retry", &model);
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "send before a stream drop");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(lock(&model.requests).len(), 2);
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["tools"].as_array().unwrap().len(), 1);
    assert_eq!(facts["tools"][0]["state"], "completed");
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "sent before dropped stream")
            .count(),
        1
    );
    assert!(!texts
        .iter()
        .any(|s| s.contains("uncommitted final answer")
            || s.contains("must not retry after the send")));
    stop_bridge(&mut rig);
    rig.start();
    assert_eq!(lock(&model.requests).len(), 2);
    assert_eq!(conversation_facts(&rig)["tools"], facts["tools"]);
}

#[test]
fn dropped_stream_after_a_memory_mutation_does_not_retry_or_repeat_the_effect() {
    let call = json!({"id":"memory-before-drop","name":"memory_write","arguments":{"key":OWNER_KEY,"token":"","content":"mutationbeforestreamdrop"}});
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![call.clone()]),
        ModelReply::Truncated("uncommitted mutation continuation".into()),
        ModelReply::ToolCalls(vec![call]),
        ModelReply::Text("must not retry after mutation".into()),
    ]);
    let mut rig = owner_harness_rig("harness-memory-stream-drop", &model);
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "remember before a stream drop",
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(
        lock(&model.requests).len(),
        2,
        "continuation retried after a memory mutation"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["tools"].as_array().unwrap().len(), 1);
    assert_eq!(facts["tools"][0]["state"], "completed");
    let response = tool_wire_result(&lock(&model.requests)[1], "memory-before-drop");
    assert_ne!(response["isError"], true);
    assert!(response.to_string().contains("已记住"));
    let token = rig.data_json("social-v2.json")["conversations"][OWNER_KEY]["agentToken"].clone();
    let (status, memory) = http_call(
        rig.console_port,
        "GET",
        &format!("/api/memory/search?key={OWNER_KEY}&query=mutationbeforestreamdrop"),
        &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", token.as_str().unwrap())],
        None,
    ).expect("read the mutation through the authenticated memory route");
    assert_eq!(status, 200);
    assert_eq!(memory["count"], 1);
    assert_eq!(memory["memories"][0]["content"], "mutationbeforestreamdrop");
    assert!(!rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s.contains("uncommitted mutation continuation")
            || s.contains("must not retry after mutation")));
    stop_bridge(&mut rig);
    rig.start();
    assert_eq!(lock(&model.requests).len(), 2);
    assert_eq!(conversation_facts(&rig)["tools"], facts["tools"]);
}

#[test]
fn dropped_stream_after_a_dispatched_handler_error_does_not_retry() {
    // Schema-valid whitespace reaches the handler, which refuses the sanitized empty query.
    let call = json!({"id":"error-before-drop","name":"web_search","arguments":{"query":"  \n  "}});
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![call.clone()]),
        ModelReply::Truncated("uncommitted error continuation".into()),
        ModelReply::ToolCalls(vec![call]),
        ModelReply::Text("must not retry after handler error".into()),
    ]);
    let mut rig = owner_harness_rig("harness-handler-error-stream-drop", &model);
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "dispatch a handler that returns an error",
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(
        lock(&model.requests).len(),
        2,
        "continuation retried after a dispatched handler error"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["tools"].as_array().unwrap().len(), 1);
    assert_eq!(facts["tools"][0]["state"], "refused");
    let response = tool_wire_result(&lock(&model.requests)[1], "error-before-drop");
    assert_eq!(response["isError"], true);
    assert!(
        response.to_string().contains("查询词为空"),
        "handler was not reached: {response}"
    );
    assert!(!rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s.contains("uncommitted error continuation")
            || s.contains("must not retry after handler error")));
    stop_bridge(&mut rig);
    rig.start();
    assert_eq!(lock(&model.requests).len(), 2);
    assert_eq!(conversation_facts(&rig)["tools"], facts["tools"]);
}

#[test]
fn interrupted_tool_intent_without_an_outbox_send_requires_confirmation() {
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![json!({
        "id":"pre-outbox-send","name":"qq_send_private_message",
        "arguments":{"userId":OWNER,"message":"unfinished quoted send","replyToMessageId":"6969"}
    })])]);
    let mut rig = owner_harness_rig("harness-intent-before-outbox", &model);
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _release_on_failure = ReleaseTool(release.clone());
    let (signal, gate) = (started.clone(), release.clone());
    lock(&rig.gateway).intercept = Some(Arc::new(move |action, body| {
        if action != "get_msg" || body["message_id"] != 6969 {
            return None;
        }
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(30), || gate
            .load(std::sync::atomic::Ordering::SeqCst)));
        Some(
            json!({"status":"ok","retcode":0,"data":{"message_id":6969,"user_id":OWNER,"message":[text_seg("quoted owner message")]}}),
        )
    }));
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "interrupt before outbox creation",
    );
    assert!(wait_for(LONG, || started.load(std::sync::atomic::Ordering::SeqCst)));
    let before = conversation_facts(&rig);
    let id = before["turns"][0]["id"].as_str().unwrap();
    assert_eq!(before["tools"][0]["state"], "running");
    assert!(!rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "intent" && r["messages"] == json!(["unfinished quoted send"])));
    stop_bridge(&mut rig);
    rig.console.kill().unwrap();
    rig.console.wait().unwrap();
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.console = spawn_console(&rig.root, rig.console_port);
    assert!(wait_for(LONG, || http_call(
        rig.console_port,
        "GET",
        "/api/health",
        &[],
        None
    )
    .is_some()));
    rig.start();
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1
        ["pausedCount"]
        == 1));
    let paused = rig.console("GET", "/api/turns", None).1;
    assert_eq!(paused["paused"][0]["reason"], "send-unresolved");
    assert_eq!(
        rig.console(
            "POST",
            "/api/turns/resolve",
            Some(&json!({"turnId":id,"action":"retry"}))
        )
        .0,
        409
    );
    assert_eq!(lock(&model.requests).len(), 1);
    assert!(!rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s == "unfinished quoted send"));
    assert_eq!(conversation_facts(&rig)["tools"], before["tools"]);
    assert_eq!(
        rig.console(
            "POST",
            "/api/turns/resolve",
            Some(&json!({"turnId":id,"action":"abandon"}))
        )
        .0,
        200
    );
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"][0]
        ["state"]
        == "cancelled"));
    assert_eq!(
        conversation_facts(&rig)["tools"],
        before["tools"],
        "resolution fabricated a tool result"
    );
}

#[test]
fn a_crash_during_a_tool_pauses_the_interrupted_turn_without_replaying_the_send() {
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![
        json!({"id":"crash-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"interrupted tool send"}}),
    ])]);
    let mut rig = harness_rig("harness-mid-tool-crash", &model);
    fs::write(
        rig.root.join("data/mode.json"),
        json!({"mode":"closed-agent"}).to_string(),
    )
    .unwrap();
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["conversationEngines"][OWNER_KEY] = json!("harness");
    fs::write(path, cfg.to_string()).unwrap();
    let at_effect = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Release(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let unblock_on_failure = Release(release.clone());
    let (signal, gate) = (at_effect.clone(), release.clone());
    lock(&rig.gateway).before_send = Some(Arc::new(move |body| {
        let text: String = body["message"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["data"]["text"].as_str())
            .collect();
        if text == "interrupted tool send" {
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
            wait_for(Duration::from_secs(15), || {
                gate.load(std::sync::atomic::Ordering::SeqCst)
            });
        }
    }));
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "interrupt this tool turn");
    assert!(
        wait_for(Duration::from_secs(8), || at_effect
            .load(std::sync::atomic::Ordering::SeqCst)),
        "send handler never reached the fake QQ boundary"
    );
    let before = conversation_facts(&rig);
    assert_eq!(before["tools"][0]["state"], "running");
    assert_eq!(before["tools"][0]["result"], Value::Null);
    assert_eq!(before["attempts"][0]["state"], "completed");
    stop_bridge(&mut rig);
    drop(unblock_on_failure);
    assert_eq!(rig.wait_texts(OWNER_KEY, 2)[1], "interrupted tool send");
    rig.start();
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1
        ["pausedCount"]
        == 1));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(lock(&model.requests).len(), 1);
    let texts = rig.texts_to(OWNER_KEY);
    // The owner also receives existing ingress/gap recovery alerts on restart.
    // Account for those exact alert classes while pinning the conversation sends.
    let mut conversation = Vec::new();
    let mut interrupted_alerts = 0;
    let mut gap_conversations = Vec::new();
    for text in &texts {
        if text.starts_with(&format!("上次运行中断的事件 1 条：\n{OWNER_KEY} "))
            && text.ends_with("\n若已获回复，无需操作；否则请重发。详见管理端 /api/inbound/gaps")
        {
            interrupted_alerts += 1;
        } else if let Some(keys) = restart_gap_notice_keys(text) {
            for key in keys {
                assert!(
                    !gap_conversations.contains(&key),
                    "duplicate gap notice for {key}: {texts:?}"
                );
                gap_conversations.push(key);
            }
        } else {
            conversation.push(text.clone());
        }
    }
    assert!(interrupted_alerts <= 1);
    assert!(rig.sends().iter().all(|(key, _)| key == OWNER_KEY));
    assert_eq!(
        conversation,
        vec![ACK.to_string(), "interrupted tool send".into()]
    );
    assert_eq!(
        rig.data_lines("send-outbox.jsonl")
            .iter()
            .filter(|r| r["k"] == "intent" && r["messages"] == json!(["interrupted tool send"]))
            .count(),
        1
    );
    let after = conversation_facts(&rig);
    assert_eq!(after["tools"], before["tools"]);
    assert_eq!(after["turns"][0]["state"], "running");
    assert_eq!(rig.console("POST", "/api/session/reset", Some(&json!({"key":OWNER_KEY}))).0, 200);
    let reset = conversation_facts(&rig);
    assert_eq!(reset["turns"][0]["state"], "cancelled");
    assert_eq!(reset["tools"], before["tools"], "reset fabricated a crashed handler result");
    assert!(rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "cancelled"));
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn version_one_plain_chat_history_migrates_without_rewriting_its_append_only_facts() {
    use sha2::Digest;
    let model = FakeModel::start(vec!["reply after schema migration"]);
    let mut rig = harness_rig("harness-v1-migration", &model);
    let token = "fixture-aaaaaaaaaaaaaaaaaa";
    let generation: String = sha2::Sha256::digest(token.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    fs::write(
        rig.root.join("data/social-v2.json"),
        json!({"conversations":{GROUP_KEY:{"agentToken":token}}}).to_string(),
    )
    .unwrap();
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    db.execute_batch(include_str!("fixtures/conversations-v1.sql"))
        .unwrap();
    db.execute(
        "INSERT INTO sessions(id,conversation,generation) VALUES('legacy-session',?1,?2)",
        rusqlite::params![GROUP_KEY, generation],
    )
    .unwrap();
    db.execute("INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms,ended_ms,result) VALUES('legacy-turn','legacy-session',?1,'legacy','completed',0,1,'legacy reply')", [GROUP_KEY]).unwrap();
    let old_user = json!({"text":"legacy input before version two"}).to_string();
    let old_assistant =
        json!({"text":"legacy reply","raw":{"captured_reasoning_content":"legacy reasoning"}})
            .to_string();
    for (kind, payload) in [("user", &old_user), ("assistant", &old_assistant)] {
        db.execute("INSERT INTO events(session_id,turn_id,kind,payload,created_ms) VALUES('legacy-session','legacy-turn',?1,?2,0)", rusqlite::params![kind,payload]).unwrap();
    }
    rig.start();
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "continue a version-one session",
    );
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 2)[1],
        "reply after schema migration"
    );
    let request = lock(&model.requests)[0].clone();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "user" && m["content"] == "legacy input before version two"));
    assert!(messages.iter().any(|m| m["role"] == "assistant"
        && m["content"] == "legacy reply"
        && m["reasoning_content"] == "legacy reasoning"));
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let old_payloads: Vec<String> = db
        .prepare("SELECT payload FROM events WHERE turn_id='legacy-turn' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(old_payloads, vec![old_user, old_assistant]);
    assert_eq!(
        conversation_facts(&rig)["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn reported_token_exhaustion_records_the_completed_model_fact_without_sending_final_text() {
    let model = FakeModel::start(vec!["over-budget final text"]); // reported total usage: 15
    let mut rig = harness_rig("harness-reported-token-budget", &model);
    set_harness_config(&rig, json!({"maxTokens":10}), 10);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "reported usage budget");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert_eq!(facts["attempts"][0]["state"], "completed");
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "assistant"
            && e["payload"]["raw"]["captured_usage"]["total_tokens"] == 15));
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "turn_ended"
            && e["payload"]["result"] == "harness token limit exceeded"));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
    assert_eq!(lock(&model.requests).len(), 1);
}

struct ReleaseTool(Arc<std::sync::atomic::AtomicBool>);
impl Drop for ReleaseTool {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn deadline_during_quote_lookup_revokes_authority_and_settles_the_dispatched_tool() {
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![
        json!({"id":"late-quote-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"must never send after deadline","replyToMessageId":"6868"}}),
    ])]);
    let mut rig = owner_harness_rig("harness-deadline-quote", &model);
    set_harness_config(&rig, json!({}), 3);
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _release_on_failure = ReleaseTool(release.clone());
    let (signal, gate) = (started.clone(), release.clone());
    lock(&rig.gateway).intercept = Some(Arc::new(move |action, body| {
        if action != "get_msg" || body["message_id"] != 6868 {
            return None;
        }
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(15), || gate
            .load(std::sync::atomic::Ordering::SeqCst)));
        Some(
            json!({"status":"ok","retcode":0,"data":{"message_id":6868,"user_id":OWNER,"message":[text_seg("quoted owner message")]}}),
        )
    }));
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "send with a slow quote lookup",
    );
    assert!(wait_for(LONG, || started.load(std::sync::atomic::Ordering::SeqCst)));
    let token = rig.data_json("social-v2.json")["conversations"][OWNER_KEY]["agentToken"].clone();
    assert!(
        wait_for(Duration::from_secs(7), || rig.data_json("social-v2.json")
            ["conversations"][OWNER_KEY]["agentToken"]
            != token),
        "deadline left dispatched tool authority live"
    );
    let pending = conversation_facts(&rig);
    assert_eq!(pending["tools"][0]["state"], "running");
    assert_eq!(
        pending["turns"][0]["state"], "running",
        "conversation must block admission while the handler is settling"
    );
    assert!(!rig.ledger().iter().any(|r| r["k"] == "ended"));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert_eq!(
        facts["tools"][0]["state"], "refused",
        "dispatched handler result was abandoned: {facts}"
    );
    assert_eq!(facts["tools"][0]["result"]["isError"], true);
    assert_eq!(facts["turns"][0]["state"], "failed");
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "turn_ended"
            && e["payload"]["result"]
                .as_str()
                .unwrap()
                .contains("deadline exceeded")));
    let texts = rig.wait_texts(OWNER_KEY, 2);
    assert_eq!(texts.iter().filter(|s| s.as_str() == ACK).count(), 1);
    assert_eq!(texts.iter().filter(|s| s.contains("backend=harness") && s.contains("class=timeout")).count(), 1);
    assert!(texts.iter().all(|s| s == ACK || (s.contains("backend=harness") && s.contains("class=timeout"))));
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn unknown_qq_delivery_pauses_before_another_effect_or_automatic_reply() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"unknown-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"unknown tool send"}}),
            json!({"id":"after-unknown-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"must not follow an unknown send"}}),
        ]),
        ModelReply::Text("must not relay after unknown send".into()),
    ]);
    let mut rig = owner_harness_rig("harness-unknown-send", &model);
    lock(&rig.gateway).lose_text = Some("unknown tool send".into());
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "send a message with a lost response",
    );
    assert!(
        wait_for(Duration::from_secs(12), || rig
            .console("GET", "/api/turns", None)
            .1["pausedCount"]
            == 1),
        "unknown delivery was returned to the model as a retryable refusal"
    );
    let paused = rig.console("GET", "/api/turns", None).1;
    assert_eq!(paused["paused"][0]["reason"], "send-unresolved");
    let id = paused["paused"][0]["turnId"].as_str().unwrap();
    assert_eq!(
        rig.console(
            "POST",
            "/api/turns/resolve",
            Some(&json!({"turnId":id,"action":"retry"}))
        )
        .0,
        409
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "paused");
    assert_eq!(facts["tools"].as_array().unwrap().len(), 1);
    assert_eq!(facts["tools"][0]["state"], "uncertain");
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(texts.iter().filter(|s| s.as_str() == ACK).count(), 1);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "unknown tool send")
            .count(),
        1
    );
    assert!(texts.iter().all(|s| s == ACK || s == "unknown tool send"
        || s == "[private:8808] 回合失败：harness turn paused (send-unresolved): QQ send delivery could not be confirmed; administrator confirmation required before retry"));
    assert_eq!(lock(&model.requests).len(), 1);
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "new input must remain blocked",
    );
    thread::sleep(Duration::from_millis(700));
    assert_eq!(lock(&model.requests).len(), 1);
    assert_eq!(
        conversation_facts(&rig)["turns"].as_array().unwrap().len(),
        1
    );
    assert_eq!(rig.console("POST", "/api/session/reset", Some(&json!({"key":OWNER_KEY}))).0, 200);
    let reset = conversation_facts(&rig);
    assert_eq!(reset["turns"][0]["state"], "cancelled");
    assert_eq!(reset["tools"], facts["tools"], "reset rewrote uncertain delivery");
    assert!(rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "cancelled"));
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn malformed_json_arguments_are_a_recorded_refusal_without_effects() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"broken-json","name":"memory_write","raw_arguments":"{\"key\":\"group:420042\",\"token\":\"\",\"content\":\"must not persist\""}),
        ]),
        ModelReply::ToolCalls(vec![
            json!({"id":"check-after-broken-json","name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
        ]),
        ModelReply::Text("malformed arguments refused".into()),
    ]);
    let mut rig = harness_rig("harness-malformed-json", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "try malformed JSON");
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 2)[1],
        "malformed arguments refused"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"].as_array().unwrap().len(), 2);
    assert_eq!(facts["tools"][0]["state"], "refused");
    assert_eq!(facts["tools"][0]["result"]["isError"], true);
    let request = lock(&model.requests)[1].clone();
    let response = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tool_call_id"] == "broken-json")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(response["content"].as_str().unwrap()).unwrap()["isError"],
        true
    );
    assert!(facts["tools"][1]["result"]
        .to_string()
        .contains("记忆库还是空的"));
}

#[test]
fn revocation_between_effectful_calls_keeps_the_first_send_and_refuses_the_next() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _release_on_failure = ReleaseTool(release.clone());
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"before-revoke","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"sent before revocation"}}),
        ]),
        ModelReply::HeldTools {
            calls: vec![
                json!({"id":"after-revoke","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"must not send after revocation"}}),
            ],
            started: started.clone(),
            release: release.clone(),
        },
    ]);
    let mut rig = owner_harness_rig("harness-between-effects", &model);
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "send twice with revocation between calls",
    );
    assert!(wait_for(LONG, || started.load(std::sync::atomic::Ordering::SeqCst)));
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "completed");
    assert_eq!(
        rig.console("POST", "/api/socialV2/reset", Some(&json!({})))
            .0,
        200
    );
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert_eq!(lock(&model.requests).len(), 2);
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "sent before revocation")
            .count(),
        1
    );
    assert!(!texts.iter().any(|s| s == "must not send after revocation"));
    let facts = conversation_facts(&rig);
    assert!(facts["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["call"] == "after-revoke")
        .all(|t| t["state"] == "refused"));
}

fn bound_during_send(bound: &str) {
    let cancel = bound != "deadline";
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![
        json!({"id":"in-flight-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"submitted before bound"}}),
        json!({"id":"send-after-bound","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"must not follow a bound"}}),
    ])]);
    let mut rig = owner_harness_rig(&format!("harness-{bound}-in-send"), &model);
    set_harness_config(&rig, json!({}), if cancel { 30 } else { 3 });
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _release_on_failure = ReleaseTool(release.clone());
    let (signal, gate) = (started.clone(), release.clone());
    lock(&rig.gateway).intercept = Some(Arc::new(move |action, body| {
        if action != "send_private_msg" || !body.to_string().contains("submitted before bound") {
            return None;
        }
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(15), || gate
            .load(std::sync::atomic::Ordering::SeqCst)));
        Some(json!({"status":"ok","retcode":0,"data":{"message_id":6869}}))
    }));
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "bound an already submitted send",
    );
    assert!(wait_for(LONG, || started.load(std::sync::atomic::Ordering::SeqCst)));
    let token = rig.data_json("social-v2.json")["conversations"][OWNER_KEY]["agentToken"].clone();
    if bound == "reset" {
        assert_eq!(
            rig.console(
                "POST",
                "/api/session/reset",
                Some(&json!({"key":OWNER_KEY}))
            )
            .0,
            200
        );
    } else if cancel {
        assert_eq!(
            rig.console("POST", "/api/mode", Some(&json!({"mode":"chat"})))
                .0,
            200
        );
    }
    assert!(wait_for(Duration::from_secs(7), || rig
        .data_json("social-v2.json")["conversations"][OWNER_KEY]
        ["agentToken"]
        != token));
    let pending = conversation_facts(&rig);
    assert_eq!(pending["tools"][0]["state"], "running");
    assert_eq!(pending["turns"][0]["state"], "running");
    assert!(
        !rig.ledger().iter().any(|r| r["k"] == "ended"),
        "turn ended before handler settlement"
    );
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert_eq!(
        facts["tools"][0]["state"], "completed",
        "submitted send result was lost: {facts}"
    );
    assert_ne!(facts["tools"][0]["result"]["isError"], true);
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "tool_result"));
    assert!(rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == if cancel { "cancelled" } else { "failed" }));
    assert_eq!(
        facts["tools"].as_array().unwrap().len(),
        1,
        "another tool dispatched after the bound"
    );
    assert_eq!(
        facts["turns"][0]["state"],
        if cancel { "cancelled" } else { "failed" }
    );
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "submitted before bound")
            .count(),
        1
    );
    assert!(!texts.iter().any(|s| s == "must not follow a bound"));
    assert_eq!(lock(&model.requests).len(), 1);
    let late = http_call(
        rig.console_port,
        "POST",
        "/api/socialV2/send-message",
        &[
            ("x-console-token", CONSOLE_TOKEN),
            ("x-agent-token", token.as_str().unwrap()),
        ],
        Some(&json!({"key":OWNER_KEY,"messages":["late old-token call"]})),
    )
    .expect("console answered late tool call");
    assert!(
        matches!(late.0, 401 | 403),
        "old token retained send authority: {late:?}"
    );
    assert!(!rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s == "late old-token call"));
    if bound == "reset" {
        stop_bridge(&mut rig);
        rig.start();
        assert_eq!(lock(&model.requests).len(), 1);
        assert_eq!(conversation_facts(&rig)["tools"], facts["tools"]);
        assert_eq!(rig.texts_to(OWNER_KEY).iter().filter(|s| s.as_str() == "submitted before bound").count(), 1);
    }
}

#[test]
fn deadline_during_a_submitted_send_records_its_result_before_ending() {
    bound_during_send("deadline");
}

#[test]
fn cancellation_during_a_submitted_send_records_its_result_before_ending() {
    bound_during_send("cancel");
}

#[test]
fn reset_during_a_submitted_send_records_its_result_before_ending() {
    bound_during_send("reset");
}

#[test]
fn a_completed_send_followed_by_budget_exhaustion_is_not_repeated_after_restart() {
    let model = FakeModel::script(vec![
        ModelReply::UsedTools {
            calls: vec![
                json!({"id":"send-before-budget","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"one send before exhausted budget"}}),
            ],
            total_tokens: 990,
        },
        ModelReply::Text("final text exceeds remaining token budget".into()),
    ]);
    let mut rig = owner_harness_rig("harness-sent-then-budget", &model);
    // The send consumes 990 tokens, then the final model step consumes 15 of the remaining 10.
    set_harness_config(&rig, json!({"maxTokens":1000}), 10);
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "send once then exhaust the token budget",
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"][0]["state"], "completed");
    assert!(facts["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "turn_ended"
            && e["payload"]["result"]
                .as_str()
                .unwrap()
                .contains("token limit")));
    stop_bridge(&mut rig);
    rig.start();
    thread::sleep(Duration::from_millis(700));
    assert_eq!(lock(&model.requests).len(), 2);
    assert_eq!(lock(&model.requests)[1]["max_tokens"], 10);
    assert!(!rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s == "final text exceeds remaining token budget"));
    assert_eq!(
        rig.texts_to(OWNER_KEY)
            .iter()
            .filter(|s| s.as_str() == "one send before exhausted budget")
            .count(),
        1
    );
    assert_eq!(
        rig.data_lines("send-outbox.jsonl")
            .iter()
            .filter(|r| r["k"] == "intent"
                && r["messages"] == json!(["one send before exhausted budget"]))
            .count(),
        1
    );
}

#[test]
fn definite_qq_refusals_return_to_the_model_without_pausing() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"definite-send-refusal","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"definitely refused send"}}),
        ]),
        ModelReply::Text("definite refusal understood".into()),
    ]);
    let mut rig = owner_harness_rig("harness-definite-refusal", &model);
    lock(&rig.gateway).reject_text = Some("definitely refused send".into());
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "try a definitely refused send",
    );
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert!(wait_for(LONG, || rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|s| s == "definite refusal understood")));
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "refused");
    assert_eq!(rig.console("GET", "/api/turns", None).1["pausedCount"], 0);
}

#[test]
fn an_unrecordable_tool_result_pauses_with_the_uncertain_effect_confirmation_guard() {
    let model = FakeModel::script(vec![ModelReply::ToolCalls(vec![
        json!({"id":"unrecordable-send","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"sent before result storage failure"}}),
        json!({"id":"after-storage-failure","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"must not follow lost result"}}),
    ])]);
    let mut rig = owner_harness_rig("harness-unrecordable-result", &model);
    let root = rig.root.clone();
    lock(&rig.gateway).before_send = Some(Arc::new(move |body| {
        if !body
            .to_string()
            .contains("sent before result storage failure")
        {
            return;
        }
        let db = rusqlite::Connection::open(root.join("data/conversations.db")).unwrap();
        db.execute_batch("CREATE TRIGGER fixture_fail_result BEFORE UPDATE ON tool_runs BEGIN SELECT RAISE(ABORT,'synthetic result storage failure'); END;").unwrap();
    }));
    rig.start();
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        "lose the local result record",
    );
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1
        ["pausedCount"]
        == 1));
    let paused = rig.console("GET", "/api/turns", None).1;
    assert_eq!(paused["paused"][0]["reason"], "send-unresolved");
    let id = paused["paused"][0]["turnId"].as_str().unwrap();
    assert_eq!(
        rig.console(
            "POST",
            "/api/turns/resolve",
            Some(&json!({"turnId":id,"action":"retry"}))
        )
        .0,
        409
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["turns"][0]["state"], "paused");
    assert_eq!(facts["tools"].as_array().unwrap().len(), 1);
    assert_eq!(facts["tools"][0]["state"], "running");
    assert_eq!(lock(&model.requests).len(), 1);
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(
        texts
            .iter()
            .filter(|s| s.as_str() == "sent before result storage failure")
            .count(),
        1
    );
    assert!(!texts.iter().any(|s| s == "must not follow lost result"));
}

#[test]
fn hotfix82_harness_filters_current_and_replayed_chat_only_in_model_copies() {
    let model = FakeModel::start(vec!["original 奸 WORD83", "second reply"]);
    let mut rig = harness_rig("hotfix82-harness", &model);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["socialV2"]["context"]["stripFromModelText"] = json!(["奸"]);
    fs::write(&path, cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(82801,1001,"奸nickname","",vec![text_seg("奸 WORD83 first")]));
    rig.wait_texts(GROUP_KEY,2);
    assert_eq!(rig.texts_to(GROUP_KEY)[1],"original 奸 WORD83", "outbound text keeps the original");
    fs::write(rig.root.join("config/model-text-blacklist.json"),r#"{"strip":["WORD83"]}"#).unwrap();
    rig.push(group_event(82802,1001,"奸nickname","",vec![text_seg("奸 WORD83 second")]));
    rig.wait_texts(GROUP_KEY,4);
    let requests=lock(&model.requests);
    assert!(!requests[0]["messages"].to_string().contains('奸'));
    assert!(requests[0]["messages"].to_string().contains("WORD83"));
    assert!(!requests[1]["messages"].to_string().contains('奸'));
    assert!(!requests[1]["messages"].to_string().contains("WORD83"), "replayed history uses the new blacklist too");
    drop(requests);
    assert!(conversation_facts(&rig)["events"].to_string().contains("original 奸 WORD83"));
    assert!(conversation_facts(&rig)["events"].to_string().contains("奸 WORD83 first"), "canonical user input keeps the original chat text");
}

#[test]
fn hotfix81_harness_failures_are_failed_turns_with_content_free_owner_alerts() {
    for (name, reply, class, refusal) in [
        ("policy",ModelReply::Refusal,"refusal",true),
        ("error-event",ModelReply::ErrorEvent,"refusal",true),
        ("policy-http",ModelReply::Error{status:403,message:"safety block CHAT_SECRET".into()},"refusal",true),
        ("timeout",ModelReply::Delayed{text:"late reply".into(),release:Arc::new(std::sync::atomic::AtomicBool::new(false))},"timeout",false),
        ("prose",ModelReply::Text("The prompt could not be submitted: Prohibited Use Policy CHAT_SECRET".into()),"refusal",true),
        ("quota",ModelReply::Error{status:429,message:"CHAT_SECRET quota".into()},"quota",false),
        ("upstream",ModelReply::Error{status:503,message:"CHAT_SECRET service".into()},"backend",false),
        ("empty",ModelReply::Text(String::new()),"empty-response",false),
        ("exit-zero-shape",ModelReply::Text("Error: failed CHAT_SECRET".into()),"backend",false),
    ] {
        let model=FakeModel::script(vec![reply]);
        let mut rig=harness_rig(&format!("hotfix81-harness-{name}"),&model);
        if name == "timeout" {
            let path=rig.root.join("config/config.json");
            let mut cfg:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            cfg["socialV2"]["turnTimeoutSecs"]=json!(1);
            fs::write(path,cfg.to_string()).unwrap();
        }
        rig.start();
        rig.push(group_event(81801,1001,"tester","",vec![text_seg("CHAT_SECRET incoming")]));
        assert!(wait_for(LONG,||rig.ledger().iter().any(|r|r["k"]=="ended"&&r["outcome"]=="failed")),"{name}: {:?}",rig.ledger());
        rig.wait_texts(OWNER_KEY,1);
        let alerts=rig.texts_to(OWNER_KEY);
        assert_eq!(alerts.len(),1,"{name}: {alerts:?}");
        assert!(alerts[0].contains("backend=harness")&&alerts[0].contains("model=fake-chat")&&alerts[0].contains(&format!("class={class}")),"{name}: {alerts:?}");
        assert!(!alerts[0].contains("CHAT_SECRET"));
        assert_eq!(rig.texts_to(GROUP_KEY),vec![ACK.to_string()],"{name}: no failed reply");
        let records=rig.data_lines("logs/model-refusals.jsonl");
        assert_eq!(records.len(),usize::from(refusal),"{name}");
        if refusal {
            assert!(alerts[0].contains("blacklist search will run"));
            assert_eq!(records[0]["backend"],"harness");
            assert!(records[0]["messages"].as_array().unwrap().iter().any(|m|m["messageId"]=="81801"));
            assert!(!records[0].to_string().contains("CHAT_SECRET"));
        }
        assert!(rig.dsh.prompts().is_empty());
    }
}

#[test]
fn hotfix82_harness_memory_filter_preserves_generated_ids() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![json!({"id":"write-82","name":"memory_write","arguments":{"key":OWNER_KEY,"token":"","content":"1奸WORD82","subject":"奸topic"}})]),
        ModelReply::ToolCalls(vec![json!({"id":"read-82","name":"memory_search","arguments":{"key":OWNER_KEY,"token":"","query":"WORD82"}})]),
        ModelReply::ToolCalls(vec![json!({"id":"index-82","name":"memory_index","arguments":{"key":OWNER_KEY,"token":""}})]),
        ModelReply::Text("memory done".into()),
        ModelReply::Text("memory reviewed".into()),
    ]);
    let mut rig = owner_harness_rig("harness-memory-strip-ids", &model);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["socialV2"]["context"]["stripFromModelText"] = json!(["奸","1"]);
    fs::write(path,cfg.to_string()).unwrap();
    rig.start();
    says(&rig,OWNER_KEY,OWNER,"owner","remember and search WORD82");
    assert_eq!(rig.wait_texts(OWNER_KEY,2)[1],"memory done");
    let requests=lock(&model.requests);
    assert_eq!(requests.len(),4);
    let tool_text = |step: usize, id: &str| requests[step]["messages"].as_array().unwrap().iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == id).unwrap()["content"].as_str().unwrap().to_string();
    let written=tool_text(1,"write-82");
    assert!(written.contains("#1"),"generated write ID is metadata: {written}");
    let found=tool_text(2,"read-82");
    assert!(found.contains("1. [#1 fact"),"generated search ordinal and ID are metadata: {found}");
    assert!(found.contains("WORD82")&&!found.contains('奸'),"chat fields are filtered: {found}");
    let index=tool_text(3,"index-82");
    assert!(index.contains("共 1 条")&&!index.contains('奸'),"generated count is metadata: {index}");
    drop(requests);
    fs::write(rig.root.join("config/model-text-blacklist.json"),r#"{"strip":["WORD82"]}"#).unwrap();
    says(&rig,OWNER_KEY,OWNER,"owner","review memory");
    assert_eq!(rig.wait_texts(OWNER_KEY,4)[3],"memory reviewed");
    let requests=lock(&model.requests);
    assert_eq!(requests.len(),5);
    let replay=requests[4]["messages"].to_string();
    assert!(!replay.contains("WORD82"),"replayed chat/query/results use the new blacklist: {replay}");
    assert!(replay.contains("#1 fact"),"replayed generated memory ID remains intact: {replay}");
}

#[test]
fn streamed_refusal_delta_with_stop_records_refusal_and_blacklist_notice() {
    let model = FakeModel::script(vec![ModelReply::StreamedRefusal]);
    let mut rig = harness_rig("harness-streamed-refusal", &model);
    rig.start();
    rig.push(group_event(
        81804,
        1001,
        "tester",
        "",
        vec![text_seg("CHAT_SECRET incoming")],
    ));
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
    let alerts = rig.wait_texts(OWNER_KEY, 1);
    assert!(alerts[0].contains("class=refusal"), "{alerts:?}");
    assert!(alerts[0].contains("blacklist search will run"));
    assert!(!alerts[0].contains("CHAT_SECRET"));
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["errorClass"], "refusal");
    assert_eq!(records[0]["errorCode"], "MODEL_REFUSAL");
    assert!(!records[0].to_string().contains("CHAT_SECRET"));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()]);
}

fn tool_wire_result(request: &Value, call: &str) -> Value {
    let message = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == call)
        .unwrap();
    serde_json::from_str(message["content"].as_str().unwrap()).unwrap()
}

fn tool_wire_packet(request: &Value, call: &str) -> Value {
    let outcome = tool_wire_result(request, call);
    serde_json::from_str(outcome["content"][0]["text"].as_str().unwrap())
        .unwrap_or_else(|error| panic!("{call}: {error}: {outcome}"))
}

#[test]
fn qq_tool_history_restores_originals_after_blacklist_removal_on_replay() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"history-restore","name":"qq_get_group_history","arguments":{"groupId":GROUP_ID}}),
        ]),
        ModelReply::ToolCalls(vec![
            json!({"id":"send-original","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"WORD original outgoing"}}),
        ]),
        ModelReply::Text("history read".into()),
        ModelReply::Text("history still filtered".into()),
        ModelReply::Text("history replayed".into()),
    ]);
    let mut rig = owner_harness_rig("harness-qq-history-restore", &model);
    let original = json!({"messages":[{"message_id":81803,"sender":{"nickname":"WORD nickname"},"raw_message":"WORD historical chat"}]});
    let reply = original.clone();
    lock(&rig.gateway).intercept = Some(Arc::new(move |action, _| {
        (action == "get_group_msg_history").then(|| json!({"status":"ok","retcode":0,"data":reply}))
    }));
    let blacklist = rig.root.join("config/model-text-blacklist.json");
    fs::write(&blacklist, r#"{"strip":["WORD"]}"#).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "read group history");
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    {
        let requests = lock(&model.requests);
        let filtered = tool_wire_packet(&requests[1], "history-restore");
        assert_eq!(filtered["messages"][0]["raw_message"], " historical chat");
        assert_eq!(filtered["messages"][0]["sender"]["nickname"], " nickname");
        assert_eq!(filtered["messages"][0]["message_id"], 81803);
    }
    assert!(rig
        .texts_to(OWNER_KEY)
        .iter()
        .any(|text| text == "WORD original outgoing"));
    says(&rig, OWNER_KEY, OWNER, "owner", "replay while blacklisted");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 4));
    assert_eq!(
        tool_wire_packet(&lock(&model.requests)[3], "history-restore")["messages"][0]
            ["raw_message"],
        " historical chat"
    );
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .filter(|r| r["k"] == "ended")
        .count()
        == 2));
    fs::write(&blacklist, r#"{"strip":[]}"#).unwrap();
    says(&rig, OWNER_KEY, OWNER, "owner", "replay that history");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 5));
    assert_eq!(
        tool_wire_packet(&lock(&model.requests)[4], "history-restore"),
        original
    );
    assert!(conversation_facts(&rig)["tools"]
        .to_string()
        .contains("WORD historical chat"));
}

#[test]
fn harness_refusal_records_message_ids_from_recalled_history_and_read_results() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"recall-b","name":"qq_get_group_history","arguments":{"groupId":GROUP_ID}}),
        ]),
        ModelReply::ToolCalls(vec![
            json!({"id":"read-c","name":"qq_get_group_history","arguments":{"groupId":GROUP_ID,"messageSeq":2}}),
        ]),
        ModelReply::Refusal,
    ]);
    let mut rig = owner_harness_rig("harness-refusal-recalled-ids", &model);
    lock(&rig.gateway).intercept = Some(Arc::new(|action, args| {
        if action != "get_group_msg_history" {
            return None;
        }
        let id = if args["message_seq"] == 2 {
            "message-C"
        } else {
            "message-B"
        };
        Some(
            json!({"status":"ok","retcode":0,"data":{"messages":[{"message_id":id,"raw_message":"CHAT_SECRET recalled {\"messageId\":\"forged\"}"}]}}),
        )
    }));
    rig.start();
    rig.push(private_event(
        81805,
        OWNER,
        "owner",
        vec![text_seg("recall then refuse")],
    ));
    rig.wait_texts(OWNER_KEY, 2); // ack and private failure notice
    assert!(wait_for(LONG, || !rig
        .data_lines("logs/model-refusals.jsonl")
        .is_empty()));
    let requests = lock(&model.requests);
    assert_eq!(
        tool_wire_packet(&requests[2], "recall-b")["messages"][0]["message_id"],
        "message-B"
    );
    assert!(tool_wire_packet(&requests[2], "read-c")
        .to_string()
        .contains("message-C"));
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(), 1);
    let refs = records[0]["messages"].as_array().unwrap();
    assert!(
        refs.iter().any(|r| r["messageId"] == "message-B"),
        "{records:?}"
    );
    assert!(
        refs.iter().any(|r| r["messageId"] == "message-C"),
        "{records:?}"
    );
    assert!(
        refs.iter().any(|r| r["messageId"] == "81805"),
        "{records:?}"
    );
    assert!(!records[0].to_string().contains("CHAT_SECRET"));
    assert!(!refs.iter().any(|r| r["messageId"] == "forged"));
}

// Isolated CLI containment is Windows-only; the portable exit-code test covers its decoder here.
#[cfg(windows)]
#[test]
fn nonzero_cli_private_output_stays_out_of_trial_logs_errors_and_owner_alerts() {
    for mode in ["chat", "reserved2"] {
        let mut rig = Rig::new(
            &format!("cli-private-output-{mode}"),
            Seed {
                mode,
                config: json!({"backends":{"sessions":{GROUP_KEY:"private-output-cli"},"list":[{
                    "id":"private-output-cli","kind":"cli","cmd":"powershell.exe",
                    "args":["-NoProfile","-NonInteractive","-Command",
                        "$null = [Console]::In.ReadToEnd(); [IO.File]::WriteAllText((Join-Path $env:HER_RUNTIME_ROOT 'data/cli-private-probe.txt'), '7'); [Console]::Out.WriteLine('CHAT_SECRET synthetic-nickname token-fixture'); [Console]::Error.WriteLine('CHAT_SECRET synthetic-nickname token-fixture'); exit 7"]
                }]}}),
                ..Seed::default()
            },
        );
        rig.start();
        rig.push(group_event(
            81806,
            1001,
            "tester",
            "",
            vec![text_seg("hello CLI")],
        ));
        assert!(wait_for(LONG, || rig
            .ledger()
            .iter()
            .any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
        assert_eq!(fs::read_to_string(rig.root.join("data/cli-private-probe.txt")).unwrap(), "7",
            "the synthetic CLI must actually execute before privacy is checked");
        let alerts = rig.wait_texts(OWNER_KEY, 1);
        let trials = rig.data_lines("logs/opencode-trial.jsonl");
        assert_eq!(trials.len(), 1, "{mode}");
        assert_eq!(trials[0]["ok"], false);
        assert!(alerts[0].contains("backend=private-output-cli"));
        for private in ["CHAT_SECRET", "synthetic-nickname", "token-fixture"] {
            assert!(
                !trials[0].to_string().contains(private),
                "{mode}: {trials:?}"
            );
            assert!(!alerts[0].contains(private), "{mode}: {alerts:?}");
            assert!(!serde_json::to_string(&rig.ledger())
                .unwrap()
                .contains(private));
            assert!(!fs::read_to_string(rig.root.join("candidate-err.log"))
                .unwrap_or_default()
                .contains(private));
        }
        assert!(rig.texts_to(GROUP_KEY).iter().all(|s| s == ACK));
        assert!(rig.dsh.prompts().is_empty());
    }
}

#[test]
fn hotfix128_harness_records_mask_known_wrapped_and_shaped_tokens_without_changing_delivery() {
    let token = "K128sessionVALUExyz";
    let shape = "0123456789abcdef0123456789abcdef0123456789abcdef";
    let reply = format!("ordinary {token} wrapped K128session\r\nVALUExyz shape {shape}");
    let model = FakeModel::start(vec![reply.as_str()]);
    let mut rig = harness_rig("harness-token-128", &model);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["security"] = json!({"outboundGuard":false});
    fs::write(path, cfg.to_string()).unwrap();
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{
        GROUP_KEY:{"agentToken":token,"unread":[],"recentMessages":[]}
    }}).to_string()).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 record a synthetic reply");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")));
    let facts = conversation_facts(&rig);
    let records = facts["events"].to_string();
    assert!(records.contains("ordinary") && records.contains("***"), "{records}");
    assert!(!records.contains("K128session") && !records.contains("VALUExyz") && !records.contains(shape), "record leaked: {records}");
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    let response: String = db.query_row("SELECT response FROM model_attempts WHERE response IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert!(!response.contains("K128session") && !response.contains("VALUExyz") && !response.contains(shape), "attempt leaked: {response}");
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string(), reply]);
}

#[test]
fn hotfix128_harness_tool_record_copies_are_masked_while_wire_call_ids_stay_original() {
    let shape = "0123456789abcdef0123456789abcdef0123456789abcdef";
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![json!({"id":shape,"name":"memory_index","arguments":{"key":GROUP_KEY,"token":"","probe":"K128session\nVALUExyz"}})]),
        ModelReply::Text("ordinary final reply".into()),
    ]);
    let mut rig = harness_rig("harness-tool-token-128", &model);
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{
        GROUP_KEY:{"agentToken":"K128sessionVALUExyz","unread":[],"recentMessages":[]}
    }}).to_string()).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 record a synthetic tool call");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")));
    let db = rusqlite::Connection::open(rig.root.join("data/conversations.db")).unwrap();
    let (call, arguments): (String, String) = db.query_row("SELECT call_id,arguments FROM tool_runs", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_ne!(call, shape, "recorded protocol references must not retain token-shaped IDs");
    assert!(!arguments.contains("K128session") && !arguments.contains("VALUExyz"));
    let facts = conversation_facts(&rig)["events"].to_string();
    assert!(!facts.contains(shape) && !facts.contains("K128session") && !facts.contains("VALUExyz"), "{facts}");
    let requests = lock(&model.requests);
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == shape));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string(), "ordinary final reply".to_string()]);
}

#[test]
fn hotfix128_recorded_tool_id_masks_keep_distinct_calls_paired_on_restart_replay() {
    let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let reserved = "her_call_l9qsDumZjfytbJwJcNpcpBHIYjOpRMJbR1ZvanvB3dU";
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":first,"name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
            json!({"id":second,"name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
            json!({"id":reserved,"name":"memory_index","arguments":{"key":GROUP_KEY,"token":""}}),
        ]),
        ModelReply::Text("first final".into()),
        ModelReply::Text("second final".into()),
    ]);
    let mut rig = harness_rig("harness-id-replay-128", &model);
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 read twice");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")));
    let mut child = rig.candidate.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 continue after restart");
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r| r["k"] == "ended").count() == 2));
    let ids = |request: &Value| {
        let messages = request["messages"].as_array().unwrap();
        let calls: Vec<String> = messages.iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .map(|call| call["id"].as_str().unwrap().to_string()).collect();
        let replies: Vec<String> = messages.iter().filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().unwrap().to_string()).collect();
        (calls, replies)
    };
    let requests = lock(&model.requests);
    assert_eq!(requests.len(), 3, "stored facts: {}", conversation_facts(&rig));
    let original = vec![first.to_string(), second.to_string(), reserved.to_string()];
    assert_eq!(ids(&requests[1]), (original.clone(), original), "live tool calls retain original identities");
    let (calls, replies) = ids(&requests[2]);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.iter().collect::<std::collections::BTreeSet<_>>().len(), 3, "masked canonical calls must remain distinct: {calls:?}");
    assert_eq!(calls, replies, "every replayed result must retain its call association");
    assert!(calls.iter().all(|id| id != first && id != second));
    let facts = conversation_facts(&rig)["events"].to_string();
    assert!(!facts.contains(first) && !facts.contains(second), "records leaked IDs: {facts}");
}

#[test]
fn hf123_harness_qq_tools_share_face_normalization_and_repeat_admission() {
    let model = FakeModel::script(vec![
        ModelReply::ToolCalls(vec![
            json!({"id":"face-1","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"[表情028]"}}),
            json!({"id":"face-2","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"[表情28]"}}),
            json!({"id":"face-3","name":"qq_send_private_message","arguments":{"userId":OWNER,"message":"[表情0[表情]28]"}}),
        ]),
        ModelReply::Text("final answer stays internal after the send tools".into()),
    ]);
    let mut rig = harness_rig("hf123-harness-guard", &model);
    fs::write(rig.root.join("data/mode.json"), json!({"mode":"closed-agent"}).to_string()).unwrap();
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["conversationEngines"][OWNER_KEY] = json!("harness");
    fs::write(path, cfg.to_string()).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "send native QQ faces");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    rig.wait_texts(OWNER_KEY, 4);
    let calls = lock(&rig.gateway).calls.clone();
    let faces: Vec<_> = calls.iter().filter(|(action, body)| action == "send_private_msg"
        && body["message"].as_array().is_some_and(|items| items.iter().any(|item| item["type"] == "face")))
        .map(|(_,body)|body["message"].clone()).collect();
    assert_eq!(faces, vec![json!([{"type":"face","data":{"id":"28"}}]); 2]);
    let rows = rig.data_lines("send-outbox.jsonl");
    assert_eq!(rows.iter().filter(|r|r["k"] == "intent" && r["messages"] == json!(["[表情28]"])).count(), 2);
    assert_eq!(rows.iter().filter(|r|r["k"] == "send_guard_notice").count(), 1);
    let texts = rig.texts_to(OWNER_KEY);
    assert_eq!(texts.iter().filter(|text| text.contains("[发送保护]")).count(), 1);
    assert!(!texts.iter().any(|text| text.contains("final answer stays internal")));
    let requests = lock(&model.requests);
    assert_eq!(requests.len(), 2);
    let refused = requests[1]["messages"].as_array().unwrap().iter()
        .find(|m|m["role"] == "tool" && m["tool_call_id"] == "face-3").unwrap();
    assert!(refused["content"].as_str().unwrap().contains("重复"), "{refused}");
}
