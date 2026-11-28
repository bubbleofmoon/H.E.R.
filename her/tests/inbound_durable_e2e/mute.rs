//! #46 group mute: QQ's mute notices, the connect refresh and the wake suppression, against the
//! fake gateway and the live bridge. The mute state is `data/group-mutes.json`.

use super::*;

const OPERATOR: i64 = 1001;
const OWNER_ID: i64 = 8808;

fn now_seconds() -> i64 {
    now_ms() / 1000
}

/// `group_ban` notice for `user` (0 is QQ's whole-group form).
fn ban_frame(user: i64, sub_type: &str, duration: i64) -> Value {
    json!({
        "time": now_seconds(),
        "self_id": SELF_ID,
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": sub_type,
        "group_id": GROUP_ID,
        "operator_id": OPERATOR,
        "user_id": user,
        "duration": duration
    })
}

fn heartbeat_frame() -> Value {
    json!({
        "time": now_seconds(),
        "self_id": SELF_ID,
        "post_type": "meta_event",
        "meta_event_type": "heartbeat",
        "status": {"online": true, "good": true}
    })
}

/// Like `start_with_config`, with the fake gateway's initial state given.
fn start_mute(root: &Path, scripts: Vec<Vec<Step>>, initial: FakeGateway) -> Harness {
    let ws_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake gateway WebSocket");
    let ws_url = format!(
        "ws://127.0.0.1:{}",
        ws_listener.local_addr().expect("ws address").port()
    );
    let onebot_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake OneBot HTTP");
    let onebot_url = format!(
        "http://127.0.0.1:{}",
        onebot_listener.local_addr().expect("http address").port()
    );
    write_fixture(root, &ws_url, &onebot_url, Some(OWNER_ID));
    let gateway = Arc::new(Mutex::new(initial));
    {
        let gateway = gateway.clone();
        let root = root.to_path_buf();
        thread::spawn(move || serve_ws(ws_listener, scripts, gateway, root));
    }
    {
        let gateway = gateway.clone();
        thread::spawn(move || serve_onebot(onebot_listener, gateway));
    }
    let (console, console_port) =
        crate::support::ports::start_console(Duration::from_secs(20), |port| {
            spawn_console(root, port)
        });
    let console = crate::support::ports::OwnedChild(console);
    let child = spawn_candidate(root, console_port);
    Harness {
        child,
        console,
        console_port,
        gateway,
        root: root.to_path_buf(),
    }
}

/// The stored mute entry of the fixture group (`Null` before any write).
fn mute_entry(root: &Path) -> Value {
    fs::read_to_string(root.join("data/group-mutes.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|store| store["groups"][format!("group:{GROUP_ID}")].clone())
        .unwrap_or(Value::Null)
}

/// Private texts the bridge sent to the owner that contain `marker`.
fn owner_texts(harness: &Harness, marker: &str) -> Vec<String> {
    let state = harness.gateway.lock().expect("gateway state");
    state
        .sends
        .iter()
        .filter(|(path, body)| path == "/send_private_msg" && body["user_id"] == json!(OWNER_ID))
        .filter_map(|(_, body)| {
            body["message"][0]["data"]["text"]
                .as_str()
                .map(str::to_string)
        })
        .filter(|text| text.contains(marker))
        .collect()
}

fn decision_count(root: &Path) -> usize {
    decisions(root).len()
}

#[test]
fn a_ban_for_her_mutes_notifies_once_and_a_mention_does_not_wake_until_the_lift() {
    let root = TempRoot::new("mute-ban");
    let harness = start_mute(
        root.path(),
        vec![vec![
            Step::Frame(ban_frame(SELF_ID, "ban", 600)),
            // A ban for someone else changes nothing.
            Step::Frame(ban_frame(1002, "ban", 600)),
            Step::Mention {
                message_id: 7101,
                text: "while muted",
            },
            Step::AwaitReceived { message_id: 7101 },
            Step::Frame(ban_frame(SELF_ID, "lift_ban", 0)),
            Step::Mention {
                message_id: 7102,
                text: "after the lift",
            },
            Step::Wait(60_000),
        ]],
        FakeGateway::default(),
    );

    assert!(
        wait_for(Duration::from_secs(30), || decision_count(root.path()) >= 1),
        "the mention after the lift was never decided"
    );
    assert!(
        wait_for(Duration::from_secs(30), || !owner_texts(
            &harness,
            "禁言提醒"
        )
        .is_empty()),
        "no mute notice reached the owner"
    );
    thread::sleep(Duration::from_secs(2));

    let all = decisions(root.path());
    assert_eq!(
        all.len(),
        1,
        "the muted mention made no wake decision: {all:?}"
    );
    assert_eq!(
        all[0]["shouldWake"],
        json!(true),
        "the mention after the lift did: {all:?}"
    );
    let social: Value =
        serde_json::from_slice(&fs::read(root.path().join("data/social-v2.json")).unwrap())
            .unwrap();
    let unread = social["conversations"]["group:420042"]["unread"].to_string();
    assert!(
        unread.contains("7101")
            || social["conversations"]["group:420042"]["recentMessages"]
                .to_string()
                .contains("7101"),
        "the message inside the mute was stored for the first wake after it: {social}"
    );
    let entry = mute_entry(root.path());
    assert_eq!(
        entry["rev"],
        json!(1),
        "the notice for another user did not start an episode: {entry}"
    );
    assert_eq!(
        entry["personalUntil"],
        json!(0),
        "the lift cleared the mute: {entry}"
    );
    assert_eq!(
        entry["seq"],
        json!(2),
        "only the own ban and the lift wrote; the notice for another user wrote nothing: {entry}"
    );
    let notices = owner_texts(&harness, "禁言");
    assert_eq!(
        notices.len(),
        1,
        "one notice for the episode and none for the lift: {notices:?}"
    );
    assert!(
        notices[0].contains("420042")
            && notices[0].contains("1001")
            && notices[0].contains("10 分钟"),
        "{notices:?}"
    );
}

#[test]
fn mute_fields_do_not_make_an_ordinary_frame_unreadable() {
    let root = TempRoot::new("mute-null-type");
    let mut frame = mention_event(7501, "carries a null notice type", now_seconds());
    frame["notice_type"] = Value::Null;
    frame["operator_id"] = json!("not a number");
    frame["duration"] = json!({"unexpected": true});
    let _harness = start_mute(
        root.path(),
        vec![vec![Step::Frame(frame), Step::Wait(60_000)]],
        FakeGateway::default(),
    );
    assert!(
        wait_for(Duration::from_secs(30), || decision_count(root.path()) >= 1),
        "a frame with null or odd mute fields was dropped"
    );
}

#[test]
fn a_whole_group_notice_with_user_zero_refreshes_and_mutes() {
    let root = TempRoot::new("mute-whole");
    let harness = start_mute(
        root.path(),
        vec![vec![
            Step::Mention {
                message_id: 7200,
                text: "before the mute",
            },
            Step::AwaitRequests {
                path: "/get_group_info",
                count: 1,
            },
            Step::AllShut(-1),
            Step::Frame(ban_frame(0, "ban", 0)),
            Step::AwaitMute(|entry| entry["wholeGroup"] == true),
            Step::Mention {
                message_id: 7202,
                text: "while muted",
            },
            Step::AwaitReceived { message_id: 7202 },
            Step::AllShut(0),
            Step::Frame(ban_frame(0, "lift_ban", 0)),
            Step::AwaitMute(|entry| {
                entry["wholeGroup"] == false && entry["seq"].as_i64().unwrap_or(0) >= 2
            }),
            Step::Mention {
                message_id: 7203,
                text: "after the lift",
            },
            Step::Wait(60_000),
        ]],
        FakeGateway {
            group_all_shut: Some(0),
            ..FakeGateway::default()
        },
    );

    assert!(
        wait_for(Duration::from_secs(40), || decision_count(root.path()) >= 2),
        "the mentions outside the mute were not decided"
    );
    assert!(
        wait_for(Duration::from_secs(30), || !owner_texts(
            &harness,
            "全员禁言"
        )
        .is_empty()),
        "no whole-group notice reached the owner"
    );
    thread::sleep(Duration::from_secs(2));

    assert_eq!(
        decision_count(root.path()),
        2,
        "the mention inside the mute made no decision: {:?}",
        decisions(root.path())
    );
    let notices = owner_texts(&harness, "全员禁言");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("1001"),
        "the notice names the operator: {notices:?}"
    );
    assert_eq!(mute_entry(root.path())["wholeGroup"], json!(false));
}

#[test]
fn a_connect_refresh_learns_a_mute_missed_while_away() {
    let root = TempRoot::new("mute-connect");
    let until_s = now_seconds() + 600;
    let harness = start_mute(
        root.path(),
        vec![vec![
            Step::Frame(heartbeat_frame()),
            Step::AwaitMute(|entry| entry["personalUntil"].as_i64().unwrap_or(0) > 0),
            Step::Mention {
                message_id: 7301,
                text: "while muted",
            },
            Step::AwaitReceived { message_id: 7301 },
            Step::Wait(60_000),
        ]],
        FakeGateway {
            shut_up_timestamp: Some(until_s),
            ..FakeGateway::default()
        },
    );

    assert!(
        wait_for(Duration::from_secs(40), || !owner_texts(
            &harness,
            "禁言提醒"
        )
        .is_empty()),
        "no mute notice reached the owner"
    );
    thread::sleep(Duration::from_secs(2));

    assert_eq!(
        mute_entry(root.path())["personalUntil"],
        json!(until_s * 1000)
    );
    let notices = owner_texts(&harness, "禁言提醒");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("操作人未知"), "{notices:?}");
    assert_eq!(
        decision_count(root.path()),
        0,
        "the mention inside the learned mute made no decision: {:?}",
        decisions(root.path())
    );
}

#[test]
fn a_ban_inside_the_batch_window_cancels_the_armed_wake() {
    let root = TempRoot::new("mute-armed");
    let _harness = start_mute(
        root.path(),
        vec![vec![
            // Ordinary text, not an @-mention: an @-mention is urgent and its wake fires on the next
            // tick, before the ban frame can be read. Ordinary first contact waits out the batch window.
            Step::Ordinary {
                message_id: 7401,
                text: "armed before the ban",
            },
            Step::Frame(ban_frame(SELF_ID, "ban", 600)),
            Step::Wait(60_000),
        ]],
        FakeGateway::default(),
    );

    let skipped = wait_for(Duration::from_secs(30), || {
        fs::read_to_string(root.path().join("candidate-out.log"))
            .unwrap_or_default()
            .contains("唤醒调度已自动跳过")
    });
    let log = fs::read_to_string(root.path().join("candidate-out.log")).unwrap_or_default();
    assert!(
        skipped,
        "the armed wake was not skipped for the mute; candidate log:\n{log}"
    );
    let armed = decisions(root.path());
    assert!(
        armed.iter().any(|d| d["shouldWake"] == json!(true)),
        "the mention armed a wake before the ban: {armed:?}"
    );
    let wakes = fs::read_to_string(root.path().join("data/logs/rbridge-live-wakes.jsonl"))
        .unwrap_or_default();
    assert!(
        !wakes
            .lines()
            .any(|line| line.contains("\"key\":\"group:420042\"")),
        "no wake was dispatched: {wakes}"
    );
}
