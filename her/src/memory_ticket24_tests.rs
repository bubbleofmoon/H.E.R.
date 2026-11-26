use super::*;

fn ticket24_memory_fixture() -> (FixtureRoot, RuntimeContext) {
    let (root, context) = fixture_runtime("ticket24 memory provenance", "fixture-console-token-17");
    let state = json!({"conversations":{
        "private:7001":{"agentToken":"owner-token","unread":[{"text":"管理员喜欢茶"}],"recentMessages":[
            {"messageId":"owner-note","userId":"7001","isSelf":false,"plain":"我喜欢热茶","time":11},
            {"messageId":"owner-correction","userId":"7001","isSelf":false,"plain":"更正，我喜欢冷茶","time":12}
        ]},
        "group:101":{"agentToken":"group-token","recentMessages":[
            {"messageId":"group-note","userId":"202","sender":"小林","isSelf":false,"plain":"管理员喜欢咖啡","time":13},
            {"messageId":"impersonating-note","userId":"303","sender":"管理员","isSelf":false,"plain":"我喜欢乌龙茶","time":14}
        ]},
        "private:202":{"agentToken":"peer-token","recentMessages":[
            {"messageId":"peer-note","userId":"202","sender":"小林","isSelf":false,"plain":"我喜欢菊花茶","time":15}
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
    let block = preview["block"].as_str().unwrap();
    assert!(
        block.contains("以当前对话为准"),
        "recall must give the live conversation precedence"
    );
    assert!(block.contains("源于本地记忆库，并非当下新发言"));
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
        row["content"], "202说：管理员喜欢咖啡",
        "group claim must identify the speaker"
    );
    assert_eq!(row["attributed_to"], "202");
    assert_eq!(row["message_id"], "group-note");
    assert_eq!(row["provenance"], "inference");
    assert_eq!(row["owner_id"], "group:101");
}

#[test]
fn ticket24_impersonating_card_cannot_attribute_a_claim_to_the_owner() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 2);
    let (write_status, written) = ticket24_memory_request(
        port,
        "write",
        "group-token",
        json!({"key":"group:101","messageId":"impersonating-note","content":"我喜欢乌龙茶"}),
    );
    let (search_status, search) = ticket24_memory_request(
        port,
        "search",
        "owner-token",
        json!({"key":"private:7001","query":"乌龙茶"}),
    );
    server.join().unwrap();
    assert_eq!(write_status, 200, "{written}");
    assert_eq!(search_status, 200, "{search}");
    let row = &search["memories"][0];
    assert_eq!(row["id"], written["id"]);
    assert_eq!(
        row["content"], "303说：我喜欢乌龙茶",
        "an impersonating card must not appear as the recalled speaker"
    );
    assert_eq!(row["attributed_to"], "303");
    assert_eq!(row["provenance"], "inference");
}

#[test]
fn ticket24_non_owner_private_message_is_attributed() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 2);
    let (write_status, written) = ticket24_memory_request(
        port,
        "write",
        "peer-token",
        json!({"key":"private:202","messageId":"peer-note","content":"我喜欢菊花茶"}),
    );
    let (search_status, search) = ticket24_memory_request(
        port,
        "search",
        "peer-token",
        json!({"key":"private:202","query":"菊花茶"}),
    );
    server.join().unwrap();
    assert_eq!(write_status, 200, "{written}");
    assert_eq!(search_status, 200, "{search}");
    let row = &search["memories"][0];
    assert_eq!(row["id"], written["id"]);
    assert_eq!(row["content"], "202说：我喜欢菊花茶");
    assert_eq!(row["attributed_to"], "202");
    assert_eq!(row["message_id"], "peer-note");
    assert_eq!(row["provenance"], "inference");
    assert_eq!(row["scope"], "private");
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

#[test]
fn ticket24_forgetting_correction_allows_predecessor_unarchive() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context, 5);
    let owner = |action, mut body: Value| {
        body["key"] = json!("private:7001");
        ticket24_memory_request(port, action, "owner-token", body)
    };
    let (write_status, first) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    let (correction_status, correction) = owner(
        "write",
        json!({"messageId":"owner-correction","content":"更正，我喜欢冷茶","supersedes":first["id"]}),
    );
    let (forget_status, forgotten) = owner("forget", json!({"id":correction["id"]}));
    let (archive_status, restored) =
        owner("archive", json!({"id":first["id"],"archived":false}));
    let (search_status, search) = owner("search", json!({"query":""}));
    server.join().unwrap();
    assert_eq!(write_status, 200, "{first}");
    assert_eq!(correction_status, 200, "{correction}");
    assert_eq!(forget_status, 200, "{forgotten}");
    assert_eq!(forgotten["ok"], true);
    assert_eq!(archive_status, 200, "{restored}");
    assert_eq!(
        restored["changed"], true,
        "forgetting the replacement must allow the predecessor to be unarchived"
    );
    assert_eq!(search_status, 200, "{search}");
    assert_eq!(search["count"], 1);
    assert_eq!(search["memories"][0]["id"], first["id"]);
}

#[test]
fn ticket24_memory_write_tool_strips_message_and_correction_fields() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context.clone(), 2);
    let (write_status, first) = ticket24_memory_request(
        port,
        "write",
        "owner-token",
        json!({"key":"private:7001","messageId":"owner-note","content":"我喜欢热茶"}),
    );
    let written = crate::memory_actions::tool_action(
        &context,
        "write",
        &json!({
            "key":"private:7001","token":"owner-token","messageId":"owner-correction",
            "content":"更正，我喜欢冷茶","supersedes":first["id"]
        }),
    );
    let (search_status, search) = ticket24_memory_request(
        port,
        "search",
        "owner-token",
        json!({"key":"private:7001","query":""}),
    );
    server.join().unwrap();
    assert_eq!(write_status, 200, "{first}");
    let written = written.expect("the model tool must accept the write as an inference");
    assert_eq!(search_status, 200, "{search}");
    let rows = search["memories"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["id"] == first["id"]), "tool write must leave the fact current");
    let row = rows.iter().find(|row| row["id"] == written["id"]).unwrap();
    assert_eq!(row["provenance"], "inference");
    assert!(row["message_id"].is_null());
}

#[test]
fn ticket336_provenance_renders_before_kind_in_search_and_recall() {
    let (_root, context) = ticket24_memory_fixture();
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["private:7001"]["unread"] =
        json!([{"text":"我喜欢热茶，管理员喜欢咖啡，手写笔记测试热茶，饮品偏好"}]);
    context
        .write_atomic(
            RuntimeArea::Data,
            "social-v2.json",
            state.to_string().as_bytes(),
        )
        .unwrap();
    let (port, server) = start_http_fixture(context.clone(), 4);
    let owner = |action, mut body: Value| {
        body["key"] = json!("private:7001");
        ticket24_memory_request(port, action, "owner-token", body)
    };
    let (fact_status, fact) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    let (quote_status, quote) = ticket24_memory_request(
        port,
        "write",
        "group-token",
        json!({
            "key":"group:101","messageId":"group-note","content":"管理员喜欢咖啡","kind":"fact"
        }),
    );
    let (unverified_status, unverified) = owner(
        "write",
        json!({"kind":"fact","subject":"饮品偏好","content":"手写笔记测试热茶"}),
    );
    let (preview_status, preview) = owner("preview", json!({}));
    server.join().unwrap();
    assert_eq!(fact_status, 200, "{fact}");
    assert_eq!(quote_status, 200, "{quote}");
    assert_eq!(unverified_status, 200, "{unverified}");
    assert_eq!(preview_status, 200, "{preview}");

    let block = preview["block"].as_str().unwrap();
    assert!(block.contains("- [事实·fact] 我喜欢热茶"), "{block}");
    assert!(block.contains("- [推断·fact·饮品偏好] 手写笔记测试热茶"), "{block}");
    assert!(block.contains("- [推断·inference] 202说：管理员喜欢咖啡"), "{block}");
    assert!(!block.contains("fact·推断"), "{block}");

    let tool_written = crate::mcp::call_tool_with_context(
        "memory_write",
        &json!({
            "key":"private:7001","token":"owner-token","kind":"fact","content":"工具写入乌龙茶"
        }),
        &context,
    )
    .unwrap();
    assert!(tool_written.contains("已记住"), "{tool_written}");

    let search_text = crate::mcp::call_tool_with_context(
        "memory_search",
        &json!({"key":"private:7001","token":"owner-token","query":"","limit":10}),
        &context,
    )
    .unwrap();
    assert!(search_text.contains("事实·fact] 我喜欢热茶"), "{search_text}");
    assert!(
        search_text.contains("推断·fact · 饮品偏好] 手写笔记测试热茶"),
        "{search_text}"
    );
    assert!(search_text.contains("推断·fact] 工具写入乌龙茶"), "{search_text}");
    assert!(
        search_text.contains("推断·inference] 202说：管理员喜欢咖啡"),
        "{search_text}"
    );
    assert!(!search_text.contains("fact·推断"), "{search_text}");
}

#[test]
fn ticket336_rewriting_archived_message_returns_archived_state_until_unarchived() {
    let (_root, context) = ticket24_memory_fixture();
    let (port, server) = start_http_fixture(context.clone(), 9);
    let owner = |action, mut body: Value| {
        body["key"] = json!("private:7001");
        ticket24_memory_request(port, action, "owner-token", body)
    };
    let (s1, first) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(s1, 200, "{first}");
    assert_eq!(first["deduped"], false);
    assert_eq!(first["archived"], false);

    let (s2, active_dedup) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(s2, 200, "{active_dedup}");
    assert_eq!(active_dedup["id"], first["id"]);
    assert_eq!(active_dedup["deduped"], true);
    assert_eq!(active_dedup["archived"], false);

    let (s3, archived) = owner("archive", json!({"id":first["id"],"archived":true}));
    assert_eq!(s3, 200, "{archived}");
    assert_eq!(archived["changed"], true);

    let (s4, hidden_search) = owner("search", json!({"query":"热茶"}));
    assert_eq!(s4, 200, "{hidden_search}");
    assert_eq!(hidden_search["count"], 0);

    let (s5, archived_dedup) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(s5, 200, "{archived_dedup}");
    assert_eq!(archived_dedup["id"], first["id"]);
    assert_eq!(archived_dedup["deduped"], true);
    assert_eq!(archived_dedup["archived"], true);

    let (s6, restored) = owner("archive", json!({"id":first["id"],"archived":false}));
    assert_eq!(s6, 200, "{restored}");
    assert_eq!(restored["changed"], true);

    let (s7, restored_dedup) = owner(
        "write",
        json!({"messageId":"owner-note","content":"我喜欢热茶"}),
    );
    assert_eq!(s7, 200, "{restored_dedup}");
    assert_eq!(restored_dedup["id"], first["id"]);
    assert_eq!(restored_dedup["deduped"], true);
    assert_eq!(restored_dedup["archived"], false);

    let (s8, unlinked) = owner(
        "write",
        json!({"content":"未关联归档内容","kind":"thought"}),
    );
    assert_eq!(s8, 200, "{unlinked}");
    assert_eq!(unlinked["archived"], false);

    let (s9, unlinked_arch) = owner("archive", json!({"id":unlinked["id"],"archived":true}));
    assert_eq!(s9, 200, "{unlinked_arch}");
    server.join().unwrap();

    let tool_dedup = crate::mcp::call_tool_with_context(
        "memory_write",
        &json!({
            "key":"private:7001","token":"owner-token","content":"未关联归档内容","kind":"thought"
        }),
        &context,
    )
    .unwrap();
    let expected_id = unlinked["id"].as_i64().unwrap();
    assert!(
        tool_dedup.contains(&format!("#{expected_id}")) && tool_dedup.contains("已归档"),
        "{tool_dedup}"
    );
    assert!(!tool_dedup.contains("已更新使用时间"), "{tool_dedup}");
}
