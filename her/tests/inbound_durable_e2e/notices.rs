use super::*;

const OWNER: i64 = 8808;
const EVENT_MS: i64 = 1_793_356_212_000; // 2026/10/30 18:30:12 at UTC+8.

fn start_notices(root: &Path, groups: &[i64]) -> Harness {
    start_notices_with(root, groups, FakeGateway::default())
}

fn start_notices_with(root: &Path, groups: &[i64], initial: FakeGateway) -> Harness {
    let ws = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let http = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    write_fixture(
        root,
        &format!("ws://{}", ws.local_addr().unwrap()),
        &format!("http://{}", http.local_addr().unwrap()),
        Some(OWNER),
    );
    let config_path = root.join("config/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["allow"] = json!({"groups": groups, "private": [OWNER, 8810]});
    fs::write(config_path, config.to_string()).unwrap();
    let gateway = Arc::new(Mutex::new(initial));
    let capture = gateway.clone();
    thread::spawn(move || serve_onebot(http, capture));
    // One socket per real process start. A heartbeat supplies a durable last-alive
    // boundary at disconnect without introducing any message or model turn.
    thread::spawn(move || {
        for stream in ws.incoming().take(3) {
            let Ok(mut socket) = accept(stream.unwrap()) else {
                continue;
            };
            let _ = socket.send(Message::Text(json!({"post_type":"meta_event", "meta_event_type":"heartbeat", "status":{"online":true,"good":true}}).to_string()));
            while socket.read().is_ok() {}
        }
    });
    let (console, console_port) = crate::support::ports::start_console(std::time::Duration::from_secs(20), |port| {
        spawn_console(root, port)
    });
    Harness {
        child: spawn_candidate(root, console_port),
        console,
        console_port,
        gateway,
        root: root.to_path_buf(),
    }
}

#[test]
fn a_known_gap_notice_refusal_stays_unannounced_and_retries_after_restart() {
    let root = TempRoot::new("notice-refused");
    seed_gaps(root.path());
    let initial = FakeGateway {
        send_retcode: Some(1404),
        ..FakeGateway::default()
    };
    let mut harness = start_notices_with(root.path(), &[], initial);
    assert!(
        wait_for(Duration::from_secs(30), || {
            fs::read_to_string(root.path().join("candidate-out.log"))
                .unwrap_or_default()
                .contains("retcode=1404")
        }),
        "gateway refusal was not observed"
    );
    let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
    // Opening and recovery can each attempt a notice. A refusal log proves only
    // one attempt finished; killing a later in-flight attempt leaves its windows
    // unknown. Reserve the writer while checking all ten seeded plus two opening
    // windows are pending, and retain it through the kill so no new claim races us.
    assert!(
        wait_for(Duration::from_secs(30), || {
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let states: (i64, i64) = db
                .query_row(
                    "SELECT count(*), sum(notice_state <> 'pending') FROM inbound_gaps",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            if states == (12, 0) {
                return true;
            }
            db.execute_batch("ROLLBACK").unwrap();
            false
        }),
        "refused opening and recovery notices did not return every window to pending"
    );
    let marked: i64 = db
        .query_row(
            "SELECT count(*) FROM inbound_gaps WHERE notified_ms > 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        marked, 0,
        "queue admission or a known gateway refusal is not delivery"
    );
    harness.child.kill().unwrap();
    harness.child.wait().unwrap();
    db.execute_batch("ROLLBACK").unwrap();
    harness.gateway.lock().unwrap().send_retcode = None;
    let before = sent(&harness, "private:8808").len();
    harness.child = spawn_candidate(root.path(), harness.console_port);
    assert!(wait_for(Duration::from_secs(30), || sent(
        &harness,
        "private:8808"
    )
    .len()
        > before));
    let notices = sent(&harness, "private:8808");
    assert!(
        notices.last().unwrap().contains("private:8808：7 个"),
        "the refused windows must be included: {notices:?}"
    );
    assert!(wait_for(Duration::from_secs(10), || db
        .query_row(
            "SELECT count(*) FROM inbound_gaps WHERE id <= 11 AND notified_ms = 0",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
        == 0));
}

fn stop_candidate(harness: &mut Harness) {
    // Child is the owned process handle retained at launch, never a process lookup.
    eprintln!(
        "ticket89 stopping owned candidate pid={} at {}",
        harness.child.id(),
        now_ms()
    );
    harness.child.kill().unwrap();
    harness.child.wait().unwrap();
}

#[test]
fn a_gap_notice_queued_before_a_crash_is_retried_after_restart() {
    let root = TempRoot::new("notice-queued-crash");
    seed_gaps(root.path());
    // Occupy the single outbox worker with a separate startup alert. The gap
    // notice is admitted behind it but cannot begin network I/O.
    fs::write(
        root.path().join("data/last-crash.json"),
        json!({"time":EVENT_MS,"message":"fixture crash"}).to_string(),
    )
    .unwrap();
    let gate = Arc::new(SlowHistory::default());
    let initial = FakeGateway {
        held_send: Some(gate.clone()),
        ..FakeGateway::default()
    };
    let mut harness = start_notices_with(root.path(), &[], initial);
    assert!(wait_for(Duration::from_secs(30), || gate
        .started
        .load(Ordering::SeqCst)));
    assert!(wait_for(Duration::from_secs(10), || fs::read_to_string(
        root.path().join("candidate-out.log")
    )
    .unwrap_or_default()
    .contains("[her] connected")));
    let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
    // The console confirms that selection/admission completed before the crash.
    assert!(wait_for(Duration::from_secs(10), || {
        gaps_of(&admin(harness.console_port)).iter().any(|g| {
            g["id"] == 1
                && (g["noticeState"] == "queued" || g["notifiedMs"].as_i64().unwrap_or(0) > 0)
        })
    }));
    stop_candidate(&mut harness);
    assert_eq!(
        db.query_row(
            "SELECT notified_ms FROM inbound_gaps WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "queued work is not delivered"
    );
    assert!(
        sent(&harness, "private:8808").is_empty(),
        "gap send never began"
    );
    gate.released.store(true, Ordering::SeqCst);
    harness.gateway.lock().unwrap().held_send = None;
    harness.child = spawn_candidate(root.path(), harness.console_port);
    assert!(wait_for(Duration::from_secs(30), || !sent(
        &harness,
        "private:8808"
    )
    .is_empty()));
    assert!(
        sent(&harness, "private:8808")[0].contains("private:8808：7 个"),
        "queued windows survive restart"
    );
    assert!(wait_for(Duration::from_secs(10), || db
        .query_row(
            "SELECT notified_ms FROM inbound_gaps WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
        > 0));
}

#[test]
fn a_gap_notice_sent_before_a_crash_but_not_marked_is_unknown_and_not_resent() {
    let root = TempRoot::new("notice-sent-crash");
    seed_gaps(root.path());
    let gate = Arc::new(SlowHistory::default());
    let initial = FakeGateway {
        held_send: Some(gate.clone()),
        ..FakeGateway::default()
    };
    let mut harness = start_notices_with(root.path(), &[], initial);
    assert!(wait_for(Duration::from_secs(30), || gate
        .started
        .load(Ordering::SeqCst)));
    assert_eq!(
        sent(&harness, "private:8808").len(),
        1,
        "gateway received the gap send"
    );
    let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
    // Hold the outcome checkpoint, then let the gateway confirm delivery. No
    // sleeps or product failpoints: SQLite excludes every competing writer.
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    gate.released.store(true, Ordering::SeqCst);
    assert!(
        wait_for(Duration::from_secs(10), || gate
            .responded
            .load(Ordering::SeqCst)),
        "fake gateway did not confirm delivery"
    );
    stop_candidate(&mut harness);
    let marked: i64 = db
        .query_row(
            "SELECT notified_ms FROM inbound_gaps WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(marked, 0, "crash precedes the delivery checkpoint");
    harness.gateway.lock().unwrap().held_send = None;
    harness.child = spawn_candidate(root.path(), harness.console_port);
    assert!(wait_for(Duration::from_secs(30), || sent(
        &harness,
        "private:8808"
    )
    .len()
        == 2));
    let notices = sent(&harness, "private:8808");
    assert!(
        notices[1].contains("private:8808：1 个"),
        "only the new window is resent: {notices:?}"
    );
    assert!(wait_for(Duration::from_secs(10), || gaps_of(&admin(
        harness.console_port
    ))
    .iter()
    .any(|g| g["id"] == 1
        && g["noticeState"] == "unknown"
        && g["notifiedMs"] == 0)));
}

fn sent(harness: &Harness, marker: &str) -> Vec<String> {
    let state = harness.gateway.lock().unwrap();
    state
        .requests
        .iter()
        .filter_map(|(path, body)| {
            if !path.contains("send") {
                return None;
            }
            assert_eq!(
                path, "/send_private_msg",
                "owner notices must never reach a group"
            );
            assert_eq!(body["user_id"], OWNER);
            let text = body["message"][0]["data"]["text"]
                .as_str()
                .expect("plain notice text segment");
            assert!(
                !text.contains("SECRET-CHAT") && !text.contains("SECRET-DETAIL"),
                "{text}"
            );
            text.contains(marker).then(|| text.to_string())
        })
        .collect()
}

// Seed modern owed claims; legacy schemas without claims are tested separately.
fn seed_gaps(root: &Path) {
    let db = rusqlite::Connection::open(root.join("data/inbound.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE inbound_gaps (
        id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL,
        from_ms INTEGER NOT NULL, to_ms INTEGER NOT NULL, status TEXT NOT NULL,
        reason TEXT NOT NULL, detail TEXT NOT NULL, recovered INTEGER NOT NULL DEFAULT 0,
        opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
        notice_state TEXT NOT NULL DEFAULT 'pending', notified_ms INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE inbound_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )
    .unwrap();
    for (conversation, count, status, reason) in [
        (
            "private:8808",
            5,
            "unrecovered",
            "private-history-unverified",
        ),
        (
            "private:8810",
            2,
            "unrecovered",
            "private-history-unverified",
        ),
        ("group:42", 1, "recovered", "covered"),
        ("group:42", 1, "unknown", "history-empty"),
        ("group:43", 1, "recovered", "covered"),
    ] {
        for i in 0..count {
            db.execute("INSERT INTO inbound_gaps (conversation, from_ms, to_ms, status, reason, detail, opened_ms, updated_ms)
                VALUES (?1, ?2, ?3, ?4, ?5, 'SECRET-DETAIL', ?6, ?6)",
                rusqlite::params![conversation, EVENT_MS + i * 60_000, EVENT_MS + (i + 1) * 60_000, status, reason, now_ms()]).unwrap();
        }
    }
    db.execute(
        "INSERT INTO inbound_meta VALUES ('last_seen_ms', ?1)",
        [EVENT_MS + 300_000],
    )
    .unwrap();
}

#[test]
fn private_gap_windows_are_grouped_and_only_new_windows_are_announced_after_restarts() {
    let root = TempRoot::new("notice-gaps");
    seed_gaps(root.path());
    let mut harness = start_notices(root.path(), &[GROUP_ID]);
    for run in 0..3 {
        assert!(
            wait_for(Duration::from_secs(30), || sent(&harness, "private:8808")
                .len()
                > run),
            "missing gap notice for start {run}"
        );
        let notices = sent(&harness, "private:8808");
        let text = &notices[run];
        eprintln!("ticket89 restart {run}: {text}");
        assert_eq!(
            text.matches("private:8808").count(),
            1,
            "one line per conversation: {text}"
        );
        assert_eq!(text.matches("private:8810").count(), 1, "{text}");
        assert!(
            text.contains("无法补收") && text.contains("私聊无历史") && text.contains("重发"),
            "{text}"
        );
        assert!(
            text.contains("你与机器人的私聊") && text.contains("可能漏收"),
            "owner's own window: {text}"
        );
        if run == 0 {
            assert!(
                text.contains("private:8808：6 个") && text.contains("private:8810：3 个"),
                "{text}"
            );
            assert!(
                text.contains(
                    if matches!(
                        std::env::var("TZ").as_deref(),
                        Ok("UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z")
                    ) {
                        "2026/10/30 10:30:12"
                    } else {
                        "2026/10/30 18:30:12"
                    }
                ),
                "local time: {text}"
            );
            assert!(
                text.contains("group:42：2 个")
                    && text.contains("已补收 1")
                    && text.contains("待重试 1"),
                "{text}"
            );
            let archived = text
                .lines()
                .find(|line| line.contains("group:43：1 个"))
                .expect("recovered conversation line");
            assert!(
                archived.contains("已补收 1")
                    && archived.contains("仅存档")
                    && archived.contains("需回复请重发"),
                "backfill does not wake a turn: {archived}"
            );
        } else {
            assert!(
                text.contains("private:8808：1 个") && text.contains("private:8810：1 个"),
                "only new windows: {text}"
            );
            assert!(
                !text.contains("group:42"),
                "old results must not repeat: {text}"
            );
        }
        assert!(
            wait_for(Duration::from_secs(10), || {
                let gaps = gaps_of(&admin(harness.console_port));
                gaps.len() == 13 + run * 3
                    && gaps
                        .iter()
                        .all(|g| g["notifiedMs"].as_i64().unwrap_or(0) > 0)
            }),
            "every listed window, including recovered history, must be remembered"
        );
        assert!(
            wait_for(Duration::from_secs(10), || sent(&harness, "group:420042")
                .len()
                == run + 1),
            "new group results must still be announced after the immediate private notice"
        );
        if run < 2 {
            harness.child.kill().unwrap();
            harness.child.wait().unwrap();
            harness.child = spawn_candidate(root.path(), harness.console_port);
        }
    }
    assert_eq!(sent(&harness, "private:8808").len(), 3);
}

#[test]
fn an_interrupted_event_notice_names_conversation_time_action_and_any_later_turn_once() {
    for later_in_same_conversation in [false, true] {
        let root = TempRoot::new(if later_in_same_conversation {
            "notice-interrupted-later"
        } else {
            "notice-interrupted"
        });
        let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE inbound_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, event_key TEXT NOT NULL UNIQUE,
            conversation TEXT NOT NULL, origin TEXT NOT NULL, stage TEXT NOT NULL,
            received_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, raw TEXT NOT NULL);",
        )
        .unwrap();
        db.execute_batch(
            "CREATE TABLE inbound_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO inbound_meta VALUES ('last_seen_ms', ?1)",
            [now_ms() - 180_000],
        )
        .unwrap();
        db.execute("INSERT INTO inbound_events (event_key, conversation, origin, stage, received_ms, updated_ms, raw)
            VALUES ('group:420042|message:8901', 'group:420042', 'live', 'processing', ?1, ?1, ?2)",
            rusqlite::params![now_ms() - 120_000, mention_event(8901, "SECRET-CHAT", EVENT_MS / 1000).to_string()]).unwrap();
        drop(db);
        // A later completed turn in another conversation must not trigger the note.
        let key = if later_in_same_conversation {
            "group:420042"
        } else {
            "group:999"
        };
        let records = [
            json!({"k":"begun", "turn":"later-turn", "t":now_ms() - 60_000, "key":key, "inbound":[], "epoch":"fixture-epoch"}),
            json!({"k":"ended", "turn":"later-turn", "t":now_ms() - 59_000, "state":"completed"}),
        ];
        fs::write(
            root.path().join("data/turn-ledger.jsonl"),
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let mut harness = start_notices(root.path(), &[]);
        // The notice can arrive before the first WebSocket connection.
        // Establish it before killing the process and expecting two connections.
        assert!(
            wait_for(Duration::from_secs(30), || {
                fs::read_to_string(root.path().join("candidate-out.log"))
                    .unwrap_or_default()
                    .contains("[her] connected")
            }),
            "initial bridge did not connect"
        );
        assert!(
            wait_for(Duration::from_secs(30), || !sent(&harness, "中断")
                .is_empty()),
            "interrupted event notice was not sent"
        );
        let notices = sent(&harness, "中断");
        assert_eq!(notices.len(), 1);
        let text = &notices[0];
        eprintln!("ticket89 interrupted later={later_in_same_conversation}: {text}");
        assert!(text.contains("group:420042"), "conversation: {text}");
        let local_time = if matches!(
            std::env::var("TZ").as_deref(),
            Ok("UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z")
        ) {
            "2026/10/30 10:30:12"
        } else {
            "2026/10/30 18:30:12"
        };
        assert!(
            text.contains(local_time),
            "original event time, not restart time: {text}"
        );
        assert!(
            text.contains("重发") && text.contains("已获回复") && text.contains("无需操作"),
            "action: {text}"
        );
        assert_eq!(
            text.contains("已有后续回合"),
            later_in_same_conversation,
            "{text}"
        );
        // A subsequent private gap notice is a FIFO checkpoint for the owner's
        // alert outbox: the startup notice is enqueued before reconnect notices.
        assert!(
            wait_for(Duration::from_secs(30), || !sent(&harness, "私聊无历史")
                .is_empty()),
            "initial gap notice did not drain the startup alert outbox"
        );
        let gap_notices_before_restart = sent(&harness, "私聊无历史").len();
        harness.child.kill().unwrap();
        harness.child.wait().unwrap();
        harness.child = spawn_candidate(root.path(), harness.console_port);
        assert!(
            wait_for(Duration::from_secs(30), || {
                fs::read_to_string(root.path().join("candidate-out.log"))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains("[her] connected"))
                    .count()
                    >= 2
            }),
            "restarted bridge did not reconnect"
        );
        assert!(
            wait_for(Duration::from_secs(30), || sent(&harness, "私聊无历史")
                .len()
                > gap_notices_before_restart),
            "restarted alert outbox did not reach its reconnect checkpoint"
        );
        assert_eq!(
            sent(&harness, "中断").len(),
            1,
            "the same interrupted event must not be announced again"
        );
    }
}
