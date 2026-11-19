use super::*;

pub(super) fn pending(rig: &Rig, kind: &str) -> Value {
    let mut result = Value::Null;
    assert!(
        wait_for(LONG, || {
            let (status, view) = rig.console("GET", "/api/pending", None);
            result = view["pending"][0].clone();
            status == 200 && result["kind"] == kind
        }),
        "owner interaction never became visible"
    );
    result
}

fn ask() -> ModelReply {
    ModelReply::ToolCalls(vec![json!({
        "id":"pick-colour", "name":"ask_owner",
        "arguments":{"question":"Which colour?","options":["Blue","Green"]}
    })])
}

fn request_question(rig: &mut Rig) -> Value {
    rig.start();
    says(rig, OWNER_KEY, OWNER, "owner", "ask what to use");
    let item = pending(rig, "question");
    assert!(wait_for(LONG, || rig.texts_to(OWNER_KEY).iter().any(|text| text.contains("Which colour?"))));
    item
}

#[test]
fn owner_answer_resumes_the_same_tool_call_without_a_second_user_turn() {
    let model = FakeModel::script(vec![ask(), ModelReply::Text("recorded your choice".into())]);
    let mut rig = owner_harness_rig("owner-question-answer", &model);
    let item = request_question(&mut rig);
    let facts = conversation_facts(&rig);
    assert_eq!(item["sessionId"], facts["turns"][0]["session"]);
    assert_eq!(facts["tools"][0]["state"], "running");
    assert_eq!(lock(&model.requests).len(), 1);
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2), "answer never resumed the turn");
    let request = lock(&model.requests)[1].clone();
    assert!(request["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "tool"
            && message["tool_call_id"] == "pick-colour"
            && message["content"].as_str().is_some_and(|text| text.contains("Blue"))
    }), "the original option label never reached the model");
    assert_eq!(conversation_facts(&rig)["turns"].as_array().unwrap().len(), 1);
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})));
    assert!(rig.dsh.calls("$events/result").is_empty());
}

#[test]
fn unanswered_question_expires_without_a_model_continuation() {
    let model = FakeModel::script(vec![ask()]);
    let mut rig = owner_harness_rig("owner-question-expiry", &model);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["questionTimeoutMs"] = json!(1_500);
    fs::write(path, cfg.to_string()).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "ask and wait");
    assert!(wait_for(Duration::from_secs(7), || rig.texts_to(OWNER_KEY).iter().any(|text| text.contains("等待回答超时"))), "question did not use its configured expiry");
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"][0]["state"] == "failed"));
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "refused");
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})));
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn the_turn_deadline_tells_the_owner_the_question_is_closed() {
    let model = FakeModel::script(vec![ask()]);
    let mut rig = owner_harness_rig("owner-question-turn-deadline", &model);
    set_harness_config(&rig, json!({}), 5);
    request_question(&mut rig);
    assert!(wait_for(LONG, || rig.texts_to(OWNER_KEY).iter().any(|text| text == "⏰ 等待回答超时，已取消该请求。")), "the turn deadline did not send the question closure notice");
    assert!(wait_for(LONG, || conversation_facts(&rig)["turns"][0]["state"] == "failed"));
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "refused");
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})));
    assert_eq!(lock(&model.requests).len(), 1);
}

#[test]
fn qq_reset_and_new_end_a_pending_question_without_answering_it() {
    for (tag, command) in [("reset", "/reset"), ("new", "/new")] {
        let model = FakeModel::script(vec![ask()]);
        let mut rig = owner_harness_rig(&format!("owner-question-{tag}"), &model);
        request_question(&mut rig);
        says(&rig, OWNER_KEY, OWNER, "owner", command);
        assert!(wait_for(Duration::from_secs(7), || conversation_facts(&rig)["turns"][0]["state"] == "cancelled"), "{command} did not cancel the suspended turn");
        assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})));
        assert_eq!(lock(&model.requests).len(), 1);
    }
}

#[test]
fn bridge_restart_does_not_restore_or_resend_a_pending_question() {
    let model = FakeModel::script(vec![ask()]);
    let mut rig = owner_harness_rig("owner-question-restart", &model);
    request_question(&mut rig);
    stop_bridge(&mut rig);
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})), "the console exposed a question whose turn no longer has an owner");
    rig.start();
    assert!(wait_for(LONG, || rig.console("GET", "/api/turns", None).1["pausedCount"] == 1));
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[]})));
    assert_eq!(lock(&model.requests).len(), 1);
    assert_eq!(rig.texts_to(OWNER_KEY).iter().filter(|text| text.contains("Which colour?")).count(), 1);
}
