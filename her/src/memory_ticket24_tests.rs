use super::*;

fn ticket24_memory_fixture() -> (FixtureRoot, RuntimeContext) {
    let (root, context) = fixture_runtime("ticket24 memory provenance", "fixture-console-token-17");
    let state = json!({"conversations":{
        "private:7001":{"agentToken":"owner-token","unread":[{"text":"管理员喜欢茶"}],"recentMessages":[
            {"messageId":"owner-note","userId":"7001","isSelf":false,"plain":"我喜欢热茶","time":11},
            {"messageId":"owner-correction","userId":"7001","isSelf":false,"plain":"更正，我喜欢冷茶","time":12}
        ]},
        "group:101":{"agentToken":"group-token","recentMessages":[
            {"messageId":"group-note","userId":"202","sender":"小林","isSelf":false,"plain":"管理员喜欢咖啡","time":13}
        ]}
    }});
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            state.to_string().as_bytes(),
        )
        .unwrap();
    (root, context)
}

fn ticket24_memory_request(port: u16, action: &str, token: &str, body: Value) -> (u16, Value) {
    http_json_with_agent(
        port,
        "POST",
        &format!("/api/memory/{action}"),
        "fixture-console-token-17",
        Some(token),
        Some(&body.to_string()),
    )
}

#[test]
fn ticket24_inference_and_verified_fact_round_trip() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 5);
    let owner = |action, mut body: Value| {
        body["key"] = json!("private:7001");
        ticket24_memory_request(port, action, "owner-token", body)
    };
    let (status, inferred) = owner(
        "write",
        json!({
            "kind":"fact", "content":"管理员喜欢茶", "source":"verified-message", "provenance":"fact"
        }),
    );
    assert_eq!(status, 200, "{inferred}");
    let (status, verified) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(status, 200, "{verified}");
    let (status, mismatch) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢冰水"}),
    );
    assert_eq!(status, 400, "{mismatch}");
    let (status, search) = owner("search", json!({"query":""}));
    let (preview_status, preview) = owner("preview", json!({}));
    server.join().unwrap();
    assert_eq!(status, 200, "{search}");
    let memories = search["memories"].as_array().unwrap();
    let find = |id: &Value| memories.iter().find(|m| &m["id"] == id).unwrap();
    assert_eq!(
        find(&inferred["id"])["provenance"],
        "inference",
        "unverified output must be inference"
    );
    let fact = find(&verified["id"]);
    assert_eq!(
        fact["provenance"], "fact",
        "owner quote must retain fact provenance"
    );
    assert_eq!(fact["message_id"], "owner-note");
    assert_eq!(preview_status, 200, "{preview}");
    assert!(
        preview["block"]
            .as_str()
            .unwrap()
            .contains("以当前对话为准"),
        "recall must give the live conversation precedence"
    );
}

#[test]
fn ticket24_group_owner_claim_is_attributed() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 2);
    let (status, written) = ticket24_memory_request(
        port,
        "write",
        "group-token",
        json!({
            "key":"group:101","messageId":"group-note","content":"管理员喜欢咖啡","kind":"fact"
        }),
    );
    assert_eq!(status, 200, "{written}");
    let (status, search) = ticket24_memory_request(
        port,
        "search",
        "owner-token",
        json!({
            "key":"private:7001","query":"管理员喜欢咖啡"
        }),
    );
    server.join().unwrap();
    assert_eq!(status, 200, "{search}");
    let row = &search["memories"][0];
    assert_eq!(row["id"], written["id"]);
    assert_eq!(
        row["content"], "小林说：管理员喜欢咖啡",
        "group claim must name the speaker"
    );
    assert_eq!(row["attributed_to"], "小林");
    assert_eq!(row["message_id"], "group-note");
    assert_eq!(row["provenance"], "inference");
    assert_eq!(row["owner_id"], "group:101");
}

#[test]
fn ticket24_correction_supersedes_without_recalling_old_record() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 7);
    let owner = |action, mut body: Value| {
        body["key"] = json!("private:7001");
        ticket24_memory_request(port, action, "owner-token", body)
    };
    let (status, first) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(status, 200, "{first}");
    let (status, denied) = ticket24_memory_request(
        port,
        "write",
        "group-token",
        json!({
            "key":"group:101","messageId":"group-note","content":"管理员喜欢咖啡","supersedes":first["id"]
        }),
    );
    assert_eq!(status, 409, "{denied}");
    let body = json!({"messageId":"owner-correction","content":"更正，我喜欢冷茶","supersedes":first["id"]});
    let (status, corrected) = owner("write", body.clone());
    assert_eq!(status, 200, "{corrected}");
    let (status, retried) = owner("write", body);
    assert_eq!(status, 200, "{retried}");
    assert_eq!(retried["id"], corrected["id"]);
    let (status, archived) = owner("archive", json!({"id":first["id"],"archived":false}));
    assert_eq!(status, 200, "{archived}");
    assert_eq!(archived["changed"], false);
    let (status, search) = owner("search", json!({"query":""}));
    assert_eq!(status, 200, "{search}");
    let (history_status, history) = owner("history", json!({"id":first["id"]}));
    server.join().unwrap();
    assert_eq!(
        search["count"], 1,
        "superseded row must be absent from current search"
    );
    assert_eq!(search["memories"][0]["id"], corrected["id"]);
    assert_eq!(history_status, 200, "{history}");
    assert_eq!(history["content"], "我喜欢热茶");
    assert_eq!(history["superseded_by"], corrected["id"]);
}
