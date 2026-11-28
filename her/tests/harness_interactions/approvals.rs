use super::*;

fn propose_group_send() -> ModelReply {
    ModelReply::ToolCalls(vec![json!({
        "id":"proposal", "name":"qq_send_group_message",
        "arguments":{"groupId":GROUP_ID,"message":"approved group text"}
    })])
}

fn request_approval(rig: &mut Rig) -> String {
    rig.start();
    says(rig, OWNER_KEY, OWNER, "owner", "ask before posting");
    let id = pending_approval(rig);
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    id
}

fn pending_approval(rig: &Rig) -> String {
    let item = owner_questions::pending(rig, "approval");
    assert_eq!(item["target"], GROUP_KEY);
    assert_eq!(item["message"], "approved group text");
    let id = item["requestId"].as_str().unwrap();
    assert!(wait_for(LONG, || rig.texts_to(OWNER_KEY).iter().any(
        |text| { text.contains(id) && text.contains("approved group text") }
    )));
    id.into()
}

#[test]
fn unrecognised_approval_reply_reprompts_with_both_exact_commands() {
    let model = FakeModel::script(vec![propose_group_send()]);
    let mut rig = owner_harness_rig("owner-approval-reprompt", &model);
    let id = request_approval(&mut rig);
    let expected = format!("回复 /approve {id} 或 /deny {id}");
    for reply in ["/approve", "/approve wrong extra", "/approval-help"] {
        let before = rig
            .texts_to(OWNER_KEY)
            .iter()
            .filter(|text| *text == &expected)
            .count();
        says(&rig, OWNER_KEY, OWNER, "owner", reply);
        assert!(
            wait_for(LONG, || {
                rig.texts_to(OWNER_KEY)
                    .iter()
                    .filter(|text| *text == &expected)
                    .count()
                    > before
            }),
            "an unrecognised approval reply did not display both exact commands"
        );
    }
    assert_eq!(lock(&model.requests).len(), 1);
    assert!(rig.texts_to(GROUP_KEY).is_empty());
}

#[test]
fn ordinary_chat_during_approval_reaches_a_normal_turn() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("handled approval".into()),
        ModelReply::Text("handled ordinary chat".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-chat", &model);
    let id = request_approval(&mut rig);
    let ordinary = "an unrelated chat message";
    says(&rig, OWNER_KEY, OWNER, "owner", ordinary);
    let expected = format!("回复 /approve {id} 或 /deny {id}");
    assert!(wait_for(LONG, || rig
        .texts_to(OWNER_KEY)
        .contains(&expected)));
    says(&rig, OWNER_KEY, OWNER, "owner", &format!("/approve {id}"));
    assert!(
        wait_for(LONG, || lock(&model.requests).len() == 3),
        "ordinary chat did not start its own turn"
    );
    assert!(lock(&model.requests)[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(ordinary))
        }));
    assert_eq!(
        conversation_facts(&rig)["turns"].as_array().unwrap().len(),
        2
    );
    assert_eq!(rig.texts_to(GROUP_KEY), ["approved group text"]);
}

#[test]
fn stray_approval_commands_report_nothing_pending() {
    let model = FakeModel::script(vec![]);
    let mut rig = owner_harness_rig("owner-approval-stray", &model);
    rig.start();
    for (index, command) in ["/approve missing", "/deny missing"]
        .into_iter()
        .enumerate()
    {
        says(&rig, OWNER_KEY, OWNER, "owner", command);
        assert!(
            wait_for(LONG, || {
                rig.texts_to(OWNER_KEY)
                    .iter()
                    .filter(|text| text.as_str() == "没有待处理的请求。")
                    .count()
                    > index
            }),
            "a stray approval command did not report that nothing was pending"
        );
    }
    assert!(lock(&model.requests).is_empty());
}

#[test]
fn matching_approval_commands_from_group_members_and_the_owner_in_a_group_are_ignored() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("handled private approval".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-group-ingress", &model);
    let id = request_approval(&mut rig);
    for (user, nick) in [(STRANGER, "member"), (OWNER, "owner")] {
        for command in ["/approve", "/deny"] {
            says(&rig, GROUP_KEY, user, nick, &format!("{command} {id}"));
        }
    }
    // The bridge processes frames in order. A private re-prompt proves the group frames passed
    // through ingress and the same approval is still live.
    says(&rig, OWNER_KEY, OWNER, "owner", "/approval-help");
    let expected = format!("回复 /approve {id} 或 /deny {id}");
    assert!(
        wait_for(LONG, || rig.texts_to(OWNER_KEY).contains(&expected)),
        "a group command consumed the private approval"
    );
    assert_eq!(lock(&model.requests).len(), 1);
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "running");
    assert_eq!(pending_approval(&rig), id);
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    says(&rig, OWNER_KEY, OWNER, "owner", &format!("/approve {id}"));
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert_eq!(rig.texts_to(GROUP_KEY), ["approved group text"]);
}

#[test]
fn refusing_an_approval_creates_no_target_send_intent() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("understood".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-denied", &model);
    let id = request_approval(&mut rig);
    says(&rig, OWNER_KEY, OWNER, "owner", &format!("/deny {id}"));
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert!(
        rig.texts_to(GROUP_KEY).is_empty(),
        "a refused approval sent to the group"
    );
    assert!(rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .all(|entry| entry["key"] != GROUP_KEY));
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "refused");
}

#[test]
fn only_the_current_approval_id_can_send_and_it_sends_once() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("understood".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-once", &model);
    let id = request_approval(&mut rig);
    says(&rig, OWNER_KEY, OWNER, "owner", "/approve obsolete-request");
    assert!(
        !wait_for(Duration::from_secs(3), || lock(&model.requests).len() > 1),
        "a mismatched request ID resumed the approval"
    );
    let approval = format!("/approve {id}");
    says(&rig, OWNER_KEY, OWNER, "owner", &approval);
    says(&rig, OWNER_KEY, OWNER, "owner", &approval);
    assert!(wait_for(LONG, || lock(&model.requests).len() == 2));
    assert_eq!(rig.texts_to(GROUP_KEY), ["approved group text"]);
    assert_eq!(conversation_facts(&rig)["tools"][0]["state"], "completed");
    assert_eq!(
        rig.console("GET", "/api/pending", None),
        (200, json!({"pending":[]}))
    );
}

#[test]
fn ambiguous_approved_delivery_pauses_and_does_not_continue_or_resend() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("must not run".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-unknown", &model);
    lock(&rig.gateway).lose_text = Some("approved group text".into());
    let id = request_approval(&mut rig);
    says(&rig, OWNER_KEY, OWNER, "owner", &format!("/approve {id}"));
    assert!(wait_for(LONG, || lock(&model.requests).len() > 1
        || rig.console("GET", "/api/turns", None).1["pausedCount"] == 1));
    assert_eq!(
        lock(&model.requests).len(),
        1,
        "ambiguous approved delivery was returned to the model"
    );
    let facts = conversation_facts(&rig);
    assert_eq!(facts["tools"][0]["state"], "uncertain");
    assert_eq!(rig.texts_to(GROUP_KEY), ["approved group text"]);
    let turn = facts["turns"][0]["id"].as_str().unwrap();
    let (status, _) = rig.console(
        "POST",
        "/api/turns/resolve",
        Some(&json!({"turnId":turn,"action":"retry"})),
    );
    assert_eq!(status, 409);
}

#[test]
fn the_group_send_cap_counts_each_approving_turn() {
    let model = FakeModel::script(vec![
        propose_group_send(),
        ModelReply::Text("first done".into()),
        propose_group_send(),
        ModelReply::Text("second done".into()),
    ]);
    let mut rig = owner_harness_rig("owner-approval-turn-cap", &model);
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["socialV2"]["send"]["maxPerTurn"] = json!(1);
    fs::write(path, cfg.to_string()).unwrap();
    let first = request_approval(&mut rig);
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        &format!("/approve {first}"),
    );
    assert!(wait_for(LONG, || {
        rig.ledger()
            .iter()
            .any(|row| row["k"] == "ended" && row["outcome"] == "completed")
    }));
    assert_eq!(rig.texts_to(GROUP_KEY), ["approved group text"]);
    says(&rig, OWNER_KEY, OWNER, "owner", "ask to post again");
    let second = pending_approval(&rig);
    assert_ne!(second, first);
    says(
        &rig,
        OWNER_KEY,
        OWNER,
        "owner",
        &format!("/approve {second}"),
    );
    assert!(wait_for(LONG, || lock(&model.requests).len() == 4));
    assert_eq!(
        rig.texts_to(GROUP_KEY).len(),
        2,
        "a previous approving turn spent the current turn's group send cap"
    );
}
