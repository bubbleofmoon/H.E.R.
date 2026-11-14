//! Support-module checks. Only config_runtime.rs includes this file, so they run once.
use crate::support::{self, ports, FakeGateway, RuntimeRoot, CLOSED};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn a_reserved_port_stays_bound_until_its_owner_takes_it() {
    let reserved = ports::reserve();
    let port = reserved.port();
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_err(),
        "a live reservation refuses other binders"
    );
    let listener = reserved.into_listener();
    assert_eq!(listener.local_addr().unwrap().port(), port);
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_err(),
        "the handed-over listener still owns the port"
    );
}

#[test]
fn start_console_moves_to_a_fresh_port_when_another_binder_takes_the_reserved_one() {
    let root = RuntimeRoot::new("support-console");
    fs::write(
        root.join("config/config.json"),
        r#"{"consoleToken":"support-selftest-token"}"#,
    )
    .unwrap();
    fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    support::write_dependencies(&root, CLOSED, CLOSED, CLOSED, None);
    let mut squatter: Option<TcpListener> = None;
    let mut tried = Vec::new();
    let (mut child, port) = ports::start_console(Duration::from_secs(20), |port| {
        tried.push(port);
        if squatter.is_none() {
            // The race: another binder takes the port between its release and the child's bind.
            squatter =
                Some(TcpListener::bind(("127.0.0.1", port)).expect("squat the released port"));
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .args([
                "--console-serve",
                "--console-port",
                &port.to_string(),
                "--runtime-root",
            ])
            .arg(root.path())
            .env_remove("HER_RUNTIME_ROOT")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        support::hide_window(&mut command);
        command.spawn().expect("start support selftest console")
    });
    let _ = child.kill();
    let _ = child.wait();
    let squatted = squatter.as_ref().unwrap().local_addr().unwrap().port();
    assert_eq!(tried.len(), 2, "{tried:?}");
    assert_eq!(tried[0], squatted);
    assert_eq!(port, tried[1]);
    assert_ne!(port, squatted);
}

#[test]
fn the_fake_gateway_records_actions_and_answers_configured_envelopes() {
    let gateway = FakeGateway::start();
    gateway.reply(
        "get_login_info",
        json!({"status":"ok","retcode":0,"data":{"user_id":9001}}),
    );
    let first = post(
        gateway.port,
        "/send_group_msg",
        &json!({"group_id":1,"message":"hi"}),
    );
    let second = post(
        gateway.port,
        "/send_private_msg",
        &json!({"user_id":2,"message":"yo"}),
    );
    let login = post(gateway.port, "/get_login_info", &json!({}));
    assert_eq!(first["data"]["message_id"], 91000);
    assert_eq!(second["data"]["message_id"], 91001);
    assert_eq!(login["data"]["user_id"], 9001);
    assert_eq!(
        gateway.wait_for("send_group_msg", 1),
        vec![json!({"group_id":1,"message":"hi"})]
    );
    assert_eq!(gateway.count("send_private_msg"), 1);
}

#[test]
fn write_dependencies_writes_the_selected_map_and_the_root_removes_itself() {
    let root = RuntimeRoot::new("support-deps");
    support::write_dependencies(
        &root,
        CLOSED,
        "http://127.0.0.1:2",
        "http://127.0.0.1:3",
        Some("ws://127.0.0.1:4"),
    );
    let written: Value =
        serde_json::from_slice(&fs::read(root.join("dependencies.json")).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({"dsh":CLOSED,"model":"http://127.0.0.1:2","onebot":"http://127.0.0.1:3","onebot_ws":"ws://127.0.0.1:4"})
    );
    let path = root.path().to_path_buf();
    drop(root);
    assert!(!path.exists(), "a runtime root removes itself");
}

#[test]
fn held_ports_stay_bound_until_this_thread_releases_them() {
    let reserved = ports::reserve();
    let port = reserved.port();
    ports::hold(reserved);
    assert!(TcpListener::bind(("127.0.0.1", port)).is_err(), "a held port refuses other binders");
    ports::release_held();
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok(), "release_held frees the port for the next child");
}

fn post(port: u16, path: &str, body: &Value) -> Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let text = body.to_string();
    write!(stream, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
}
