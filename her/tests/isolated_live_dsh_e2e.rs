//! Public-entry DSH-path runtime isolation test.
//!
//! The fake DSH server receives the real prompt from the isolated live candidate, derives the
//! session key and token from that prompt, and emulates a `qq_send_message` tool relay by calling
//! the public isolated console HTTP route. This proves the bridge-to-console boundary and selected
//! runtime paths; it does not claim to exercise a real DSH model or its tool planner.

#[path = "../src/install_root.rs"]
mod install_root;
mod support;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioListener, TcpStream as TokioStream};
use tokio::sync::Notify;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
};

const GROUP_ID: i64 = 420042;
const SELF_ID: i64 = 9001;
const SESSION_TOKEN: &str = "ticket04-dsh-selected-agent-token";
const CONSOLE_TOKEN: &str = "ticket04-dsh-selected-console-token";
const ONEBOT_TOKEN: &str = "ticket04-dsh-selected-onebot-token";
const PROVIDER: &str = "ticket04-fake-dsh-provider";
const MODEL: &str = "ticket04-fake-dsh-model";
const SESSION_ID: &str = "ticket04-dsh-session";
const WORKSPACE_ID: &str = "ticket04-dsh-workspace";
const INBOUND_TEXT: &str = "选定根里的 DSH 路径消息";
/// What frozen JS `segmentsToText` makes of the fixture event: the bot mention becomes `@<card>` (the fake
/// gateway answers every member lookup with this card) and stays in the text.
const INBOUND_RENDERED: &str = "@ticket04-selected-card选定根里的 DSH 路径消息";
const OUTBOUND_TEXT: &str = "收到，选定根的 DSH 回复";
const SELECTED_SECRET: [u8; 32] = [0x42; 32];
const AMBIENT_SECRET: [u8; 32] = [0xa7; 32];

#[derive(Debug)]
struct TempRoots {
    selected: PathBuf,
    ambient: PathBuf,
}

impl TempRoots {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let base = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her ticket04 isolated DSH 中文 root {} {stamp}",
            std::process::id()
        )));
        let selected = base.join("selected runtime with spaces");
        let ambient = base.join("ambient profile decoy");
        let selected = crate::install_root::development_root(selected);
        let ambient = crate::install_root::development_root(ambient);
        fs::create_dir_all(&selected).expect("create selected runtime root");
        fs::create_dir_all(&ambient).expect("create ambient profile decoy");
        Self { selected, ambient }
    }
}

impl Drop for TempRoots {
    fn drop(&mut self) {
        if let Some(base) = self.selected.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

#[derive(Debug, Clone)]
struct ParsedHttpRequest {
    request_line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct RpcObservation {
    path: String,
    method: String,
    args: Value,
    selected_cookie_valid: bool,
}

#[derive(Debug, Clone)]
struct ToolObservation {
    path: String,
    request: Value,
    console_token_header: String,
    agent_token_header: String,
    status: u16,
    response: Value,
}

#[derive(Debug, Clone)]
struct DshWsObservation {
    selected_cookie_valid: bool,
    open_frame: Value,
}

#[derive(Debug, Clone)]
struct OneBotObservation {
    request_line: String,
    headers: String,
    body: Value,
}

#[derive(Debug, Clone)]
struct OneBotWsObservation {
    authorization: Option<String>,
    login_action: Option<String>,
}

#[derive(Debug, Clone)]
struct DshCapture {
    rpc: Vec<RpcObservation>,
    prompt: Option<String>,
    tool: Option<Result<ToolObservation, String>>,
    ws: Option<DshWsObservation>,
    errors: Vec<String>,
}

#[derive(Debug)]
struct DshShared {
    authority: String,
    selected_secret: [u8; 32],
    console_port: u16,
    console_token: String,
    rpc: Mutex<Vec<RpcObservation>>,
    prompt: Mutex<Option<String>>,
    tool: Mutex<Option<Result<ToolObservation, String>>>,
    ws: Mutex<Option<DshWsObservation>>,
    errors: Mutex<Vec<String>>,
    prompt_ready: Notify,
}

fn bind_loopback() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture listener");
    let address = listener.local_addr().expect("read fixture listener address");
    (listener, format!("127.0.0.1:{}", address.port()))
}

fn bind_http_url() -> (TcpListener, String) {
    let (listener, authority) = bind_loopback();
    (listener, format!("http://{authority}"))
}

fn bind_ws_url() -> (TcpListener, String) {
    let (listener, authority) = bind_loopback();
    (listener, format!("ws://{authority}"))
}

fn secret_text(secret: &[u8; 32]) -> String {
    format!("secret: \"{}\"\n", URL_SAFE_NO_PAD.encode(secret))
}

fn cookie_uses_secret(cookie: Option<&str>, secret: &[u8; 32], authority: &str) -> bool {
    let Some(cookie) = cookie else { return false };
    let Some((name, value)) = cookie.split_once('=') else {
        return false;
    };
    if !name.starts_with("dsh-auth-") {
        return false;
    }
    let mut parts = value.split('.');
    if parts.next() != Some("v1") {
        return false;
    }
    let Some(payload_b64) = parts.next() else {
        return false;
    };
    let Some(signature_b64) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(payload_b64) else {
        return false;
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&payload_bytes) else {
        return false;
    };
    if payload.get("authority").and_then(Value::as_str) != Some(authority) {
        return false;
    }
    let Ok(signature) = URL_SAFE_NO_PAD.decode(signature_b64) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(payload_b64.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

fn parse_sync_http_request(stream: &mut TcpStream) -> Result<ParsedHttpRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .map_err(|error| format!("set fake HTTP read timeout: {error}"))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream
            .read(&mut chunk)
            .map_err(|error| format!("read fake HTTP request: {error}"))?;
        if count == 0 {
            return Err("client closed before the HTTP request completed".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("fixture HTTP request exceeded 2 MiB".into());
        }
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break (header_end, content_length);
            }
        }
    };
    let headers_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let mut lines = headers_text.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let body_start = header_end + 4;
    Ok(ParsedHttpRequest {
        request_line,
        headers,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn parse_status_and_body(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response omitted header/body delimiter".to_string())?;
    let header = String::from_utf8_lossy(&raw[..split]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "HTTP response omitted a numeric status".to_string())?;
    Ok((status, raw[split + 4..].to_vec()))
}

fn send_sync_json(stream: &mut TcpStream, status: u16, value: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Fixture Error",
    };
    let body = value.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("write fake HTTP response: {error}"))
}

fn parse_path(request_line: &str) -> &str {
    request_line.split_whitespace().nth(1).unwrap_or("")
}

fn post_public_console_tool(
    console_port: u16,
    console_token: &str,
    prompt: &str,
) -> Result<ToolObservation, String> {
    // An ordinary wake names the conversation after 【唤醒】; the conversation's first wake is a
    // bootstrap wake and names it in 【引导唤醒】.
    let key = prompt
        .split_once("【唤醒】")
        .and_then(|(_, suffix)| suffix.lines().next())
        .or_else(|| {
            prompt
                .split_once("【引导唤醒】你已接入 QQ 会话 ")
                .and_then(|(_, suffix)| suffix.lines().next())
                .map(|line| line.trim_end_matches('。'))
        })
        .map(str::trim)
        .ok_or_else(|| "the real DSH prompt did not carry a wake key".to_string())?;
    let token = prompt
        .lines()
        .find_map(|line| line.strip_prefix("【会话令牌】"))
        .and_then(|line| line.split_once('（').map(|(token, _)| token.trim()))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "the real DSH prompt did not carry an agent token".to_string())?;
    if !prompt.contains(INBOUND_TEXT) {
        return Err("the real DSH prompt omitted the fake incoming message".into());
    }
    let request = json!({
        "key": key,
        "messages": [OUTBOUND_TEXT],
    });
    let body = request.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", console_port))
        .map_err(|error| format!("connect to selected public console: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(12)))
        .map_err(|error| format!("set public console response timeout: {error}"))?;
    let wire = format!(
        "POST /api/socialV2/send-message HTTP/1.1\r\nHost: 127.0.0.1:{console_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-console-token: {console_token}\r\nx-agent-token: {token}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(wire.as_bytes())
        .map_err(|error| format!("write public console tool request: {error}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| format!("read public console tool response: {error}"))?;
    let (status, response) = parse_status_and_body(&raw)?;
    let response = serde_json::from_slice(&response)
        .map_err(|error| format!("public console response was not JSON: {error}"))?;
    Ok(ToolObservation {
        path: "/api/socialV2/send-message".into(),
        request,
        console_token_header: console_token.to_string(),
        agent_token_header: token.to_string(),
        status,
        response,
    })
}

async fn read_async_http_request(stream: &mut TokioStream) -> Result<ParsedHttpRequest, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("read fake DSH HTTP request: {error}"))?;
        if count == 0 {
            return Err("DSH HTTP client closed before the request completed".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("fake DSH HTTP request exceeded 2 MiB".into());
        }
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break (header_end, content_length);
            }
        }
    };
    let headers_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let mut lines = headers_text.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let body_start = header_end + 4;
    Ok(ParsedHttpRequest {
        request_line,
        headers,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

async fn send_async_json(
    stream: &mut TokioStream,
    status: u16,
    value: &Value,
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Fixture Error",
    };
    let body = value.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| format!("write fake DSH response: {error}"))
}

async fn handle_dsh_rpc(mut stream: TokioStream, shared: Arc<DshShared>) -> Result<(), String> {
    let request = tokio::time::timeout(Duration::from_secs(8), read_async_http_request(&mut stream))
        .await
        .map_err(|_| "timed out reading a fake DSH RPC".to_string())??;
    let path = parse_path(&request.request_line).to_string();
    let cookie = request.headers.get("cookie").map(String::as_str);
    let cookie_valid = cookie_uses_secret(cookie, &shared.selected_secret, &shared.authority);
    let envelope: Value = serde_json::from_slice(&request.body)
        .map_err(|error| format!("DSH RPC body was not JSON: {error}"))?;
    let method = envelope
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = envelope
        .pointer("/payload/args")
        .cloned()
        .unwrap_or(Value::Null);
    shared.rpc.lock().unwrap().push(RpcObservation {
        path: path.clone(),
        method: method.clone(),
        args: args.clone(),
        selected_cookie_valid: cookie_valid,
    });
    if !cookie_valid {
        send_async_json(&mut stream, 401, &json!({"error":"fixture rejected non-selected DSH cookie"})).await?;
        return Ok(());
    }
    if path != format!("/api/{method}") {
        return Err(format!("DSH request path/method mismatch: {path} / {method}"));
    }
    // Frozen JS `checkDsh` requires a matching server-response envelope over successful HTTP
    // before it dispatches work. The selected DSH is healthy in this scenario.
    if method == "settings/describe" {
        send_async_json(
            &mut stream,
            200,
            &json!({"type":"server-response","rpcId":envelope["rpcId"],"result":{"ok":true,"value":{"namespaces":[]}}}),
        )
        .await?;
        return Ok(());
    }
    if method == "session/prompt" {
        let prompt = args
            .pointer("/request/content/0/text")
            .and_then(Value::as_str)
            .ok_or_else(|| "session/prompt omitted request.content[0].text".to_string())?
            .to_string();
        // Only the first prompt (the bootstrap wake) is observed and relayed. The reserved-mode
        // runner may deliver a `wakeReminder` prompt (frozen JS `bridge.js` "你还没有完成回合收尾")
        // before the bridge exits; it carries no wake key and must not replace the first prompt or
        // its tool observation.
        let first_prompt = {
            let mut observed = shared.prompt.lock().unwrap();
            if observed.is_none() {
                *observed = Some(prompt.clone());
                true
            } else {
                false
            }
        };
        if !first_prompt {
            send_async_json(&mut stream, 200, &json!({"type":"server-response","rpcId":envelope["rpcId"],"result":{"ok":true,"value":{"accepted":true}}})).await?;
            return Ok(());
        }
        let console_port = shared.console_port;
        let console_token = shared.console_token.clone();
        let tool = tokio::task::spawn_blocking(move || {
            post_public_console_tool(console_port, &console_token, &prompt)
        })
        .await
        .map_err(|error| format!("join fake DSH tool relay: {error}"))?;
        *shared.tool.lock().unwrap() = Some(tool);
        shared.prompt_ready.notify_one();
        send_async_json(&mut stream, 200, &json!({"type":"server-response","rpcId":envelope["rpcId"],"result":{"ok":true,"value":{"accepted":true}}})).await?;
        return Ok(());
    }
    let result = match method.as_str() {
        "workspace/create" => json!({"workspace":{"workspaceId":WORKSPACE_ID}}),
        "session/create" => json!({"sessionId":SESSION_ID}),
        "workspace/rename" | "session/selectModel" => json!({}),
        _ => {
            send_async_json(&mut stream, 404, &json!({"error":"unexpected fixture DSH method"})).await?;
            return Ok(());
        }
    };
    send_async_json(
        &mut stream,
        200,
        &json!({"type":"server-response","rpcId":envelope["rpcId"],"result":{"ok":true,"value":result}}),
    )
    .await
}

async fn handle_dsh_ws(mut stream: TokioStream, shared: Arc<DshShared>) -> Result<(), String> {
    let auth = Arc::new(Mutex::new(None::<String>));
    let callback_auth = auth.clone();
    let mut socket = tokio::time::timeout(
        Duration::from_secs(5),
        accept_hdr_async(stream, move |request: &Request, response: Response| {
            *callback_auth.lock().unwrap() = request
                .headers()
                .get("cookie")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            Ok(response)
        }),
    )
    .await
    .map_err(|_| "timed out upgrading selected DSH WebSocket".to_string())?
    .map_err(|error| format!("upgrade fake DSH WebSocket: {error}"))?;
    let cookie = auth.lock().unwrap().clone();
    let cookie_valid = cookie_uses_secret(cookie.as_deref(), &shared.selected_secret, &shared.authority);
    let open_message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .map_err(|_| "timed out waiting for DSH follow open frame".to_string())?
        .ok_or_else(|| "DSH follow client closed before open frame".to_string())?
        .map_err(|error| format!("read DSH follow open frame: {error}"))?;
    let Message::Text(open_text) = open_message else {
        return Err("DSH follow client did not send a text open frame".into());
    };
    let open_frame: Value = serde_json::from_str(&open_text)
        .map_err(|error| format!("DSH follow open frame was not JSON: {error}"))?;
    let stream_id = open_frame
        .get("streamId")
        .and_then(Value::as_str)
        .ok_or_else(|| "DSH follow frame omitted streamId".to_string())?
        .to_string();
    // The bridge also keeps one `$events` stream open on the same mux (frozen JS `dsh-client.js`
    // opens it for questions and approvals; Rust `closed_agent.rs` `stream`, ticket 13). It is not
    // the session follow: answer `ready`, send nothing else and keep it open until the client leaves.
    if open_frame.get("type").and_then(Value::as_str) == Some("open")
        && open_frame.get("endpoint").and_then(Value::as_str) == Some("$events")
    {
        socket
            .send(Message::Text(
                json!({
                    "type":"item",
                    "streamId":stream_id,
                    "value":{"type":"ready","clientId":"fixture-events-client"}
                })
                .to_string()
                .into(),
            ))
            .await
            .map_err(|error| format!("send DSH $events ready: {error}"))?;
        while let Ok(Some(Ok(message))) =
            tokio::time::timeout(Duration::from_secs(60), socket.next()).await
        {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
        return Ok(());
    }
    if open_frame.get("type").and_then(Value::as_str) != Some("open")
        || open_frame.get("endpoint").and_then(Value::as_str) != Some("session/follow")
        || open_frame
            .pointer("/payload/args/request/address/sessionId")
            .and_then(Value::as_str)
            != Some(SESSION_ID)
    {
        return Err(format!("unexpected DSH follow open frame: {open_frame}"));
    }
    *shared.ws.lock().unwrap() = Some(DshWsObservation {
        selected_cookie_valid: cookie_valid,
        open_frame: open_frame.clone(),
    });
    socket
        .send(Message::Text(
            json!({
                "type":"item",
                "streamId":stream_id,
                "value":{"type":"snapshot","records":[]}
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| format!("send DSH follow snapshot: {error}"))?;
    tokio::time::timeout(Duration::from_secs(22), shared.prompt_ready.notified())
        .await
        .map_err(|_| "timed out waiting for the actual session/prompt call".to_string())?;
    let prompt_present = shared.prompt.lock().unwrap().is_some();
    if !prompt_present {
        return Err("DSH tool relay was signaled without a real prompt".into());
    }
    for frame in [
        json!({
            "type":"item","streamId":stream_id,
            "value":{"type":"event","event":{"seq":1,"type":"turn/start","data":{"turn":1}}}
        }),
        json!({
            "type":"item","streamId":stream_id,
            "value":{"type":"event","event":{"seq":2,"type":"model/selection","data":{"turn":1,"provider":PROVIDER,"model":MODEL}}}
        }),
        json!({
            "type":"item","streamId":stream_id,
            "value":{"type":"event","event":{"seq":3,"type":"assistant/message","time":1_800_000_000_000i64,"data":{"turn":1,"step":0,"provider":PROVIDER,"usage":{"inputTokens":7,"outputTokens":3},"message":{"content":[{"type":"text","text":"synthetic DSH turn complete"}]}}}}
        }),
        json!({
            "type":"item","streamId":stream_id,
            "value":{"type":"event","event":{"seq":4,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}
        }),
    ] {
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .map_err(|error| format!("send DSH follow event: {error}"))?;
    }
    let _ = socket.send(Message::Close(None)).await;
    Ok(())
}

async fn handle_dsh_connection(stream: TokioStream, shared: Arc<DshShared>) -> Result<(), String> {
    let mut prefix = [0u8; 128];
    let first_line = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count = stream
                .peek(&mut prefix)
                .await
                .map_err(|error| format!("peek fake DSH connection: {error}"))?;
            if count == 0 {
                // The client connected and left without sending a byte: the bridge exits while a
                // reconnect is in flight (strace of the failing run: a non-blocking connect is
                // closed 4 ms later at process shutdown). No request was observed, so there is
                // nothing to classify and nothing for the fixture to reject.
                return Ok::<String, String>(String::new());
            }
            if let Some(end) = prefix[..count].windows(2).position(|window| window == b"\r\n") {
                return Ok(String::from_utf8_lossy(&prefix[..end]).to_string());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| "timed out classifying fake DSH connection".to_string())??;
    if first_line.is_empty() {
        Ok(())
    } else if first_line.starts_with("GET /api/remote.mux ") {
        handle_dsh_ws(stream, shared).await
    } else {
        handle_dsh_rpc(stream, shared).await
    }
}

fn capture_dsh(shared: &DshShared) -> DshCapture {
    DshCapture {
        rpc: shared.rpc.lock().unwrap().clone(),
        prompt: shared.prompt.lock().unwrap().clone(),
        tool: shared.tool.lock().unwrap().clone(),
        ws: shared.ws.lock().unwrap().clone(),
        errors: shared.errors.lock().unwrap().clone(),
    }
}

fn spawn_dsh_server(
    listener: TcpListener,
    authority: String,
    console_port: u16,
) -> ServerThread<Result<DshCapture, String>> {
    listener
        .set_nonblocking(true)
        .expect("make fake DSH listener nonblocking");
    let shared = Arc::new(DshShared {
        authority,
        selected_secret: SELECTED_SECRET,
        console_port,
        console_token: CONSOLE_TOKEN.to_string(),
        rpc: Mutex::new(Vec::new()),
        prompt: Mutex::new(None),
        tool: Mutex::new(None),
        ws: Mutex::new(None),
        errors: Mutex::new(Vec::new()),
        prompt_ready: Notify::new(),
    });
    let thread_shared = shared.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("create fake DSH runtime: {error}"))?;
        runtime.block_on(async move {
            let listener = TokioListener::from_std(listener)
                .map_err(|error| format!("adopt fake DSH listener: {error}"))?;
            let mut tasks = tokio::task::JoinSet::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(42);
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let task_shared = thread_shared.clone();
                            let task_errors = thread_shared.clone();
                            tasks.spawn(async move {
                                if let Err(error) = handle_dsh_connection(stream, task_shared).await {
                                    task_errors.errors.lock().unwrap().push(error);
                                }
                            });
                        }
                        Err(error) => return Err(format!("accept fake DSH connection: {error}")),
                    },
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {},
                    _ = tokio::time::sleep_until(deadline) => {
                        thread_shared.errors.lock().unwrap().push("fake DSH server reached its 42 second deadline".into());
                        break;
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            Ok(capture_dsh(&thread_shared))
        })
    });
    ServerThread { stop, handle: Some(handle) }
}

fn accept_fixture(listener: &TcpListener, deadline: Instant) -> Result<TcpStream, String> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) =>
            {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for a fixture HTTP request".into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("accept fixture HTTP request: {error}")),
        }
    }
}

fn serve_onebot_http(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    expect_send: bool,
) -> Result<Vec<OneBotObservation>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set fake OneBot listener nonblocking: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(38);
    let mut observations = Vec::new();
    while (!expect_send || !sent.load(Ordering::SeqCst)) && !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let mut stream = match accept_fixture(&listener, Instant::now() + Duration::from_millis(250)) {
            Ok(stream) => stream,
            Err(error) if error.contains("timed out") => continue,
            Err(error) => return Err(error),
        };
        let request = parse_sync_http_request(&mut stream)?;
        let path = parse_path(&request.request_line).to_string();
        let body = serde_json::from_slice(&request.body)
            .map_err(|error| format!("fake OneBot request body was not JSON: {error}"))?;
        observations.push(OneBotObservation {
            request_line: request.request_line,
            headers: request
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n"),
            body,
        });
        match path.as_str() {
            "/get_group_member_info" => {
                send_sync_json(
                    &mut stream,
                    200,
                    &json!({"status":"ok","retcode":0,"data":{"card":"ticket04-selected-card"}}),
                )?;
            }
            "/send_group_msg" => {
                send_sync_json(
                    &mut stream,
                    200,
                    &json!({"status":"ok","retcode":0,"data":{"message_id":770042}}),
                )?;
                sent.store(true, Ordering::SeqCst);
            }
            _ => {
                send_sync_json(
                    &mut stream,
                    404,
                    &json!({"status":"failed","retcode":404,"message":"unexpected fake OneBot action"}),
                )?;
            }
        }
    }
    if sent.load(Ordering::SeqCst) || !expect_send {
        Ok(observations)
    } else {
        Err("timed out waiting for the public console's OneBot send".into())
    }
}

fn serve_unused_model_endpoint(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
) -> Result<usize, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set model sentinel listener nonblocking: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(42);
    let mut count = 0usize;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                count += 1;
                let _ = send_sync_json(&mut stream, 404, &json!({"error":"unexpected model dependency request"}));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) =>
            {
                thread::sleep(Duration::from_millis(15));
            }
            Err(error) => return Err(format!("accept model sentinel request: {error}")),
        }
    }
    Ok(count)
}

fn serve_onebot_ws(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    hold_without_send: Option<Duration>,
    completed_root: Option<PathBuf>,
) -> Result<OneBotWsObservation, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set fake OneBot WS listener nonblocking: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create fake OneBot WS runtime: {error}"))?;
    runtime.block_on(async move {
        let listener = TokioListener::from_std(listener)
            .map_err(|error| format!("adopt fake OneBot WS listener: {error}"))?;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .map_err(|_| "timed out waiting for isolated live OneBot WS connect".to_string())?
            .map_err(|error| format!("accept fake OneBot WS connection: {error}"))?;
        let auth = Arc::new(Mutex::new(None::<String>));
        let callback_auth = auth.clone();
        let mut socket = tokio::time::timeout(
            Duration::from_secs(5),
            accept_hdr_async(stream, move |request: &Request, response: Response| {
                *callback_auth.lock().unwrap() = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                Ok(response)
            }),
        )
        .await
        .map_err(|_| "timed out completing OneBot WS handshake".to_string())?
        .map_err(|error| format!("complete fake OneBot WS handshake: {error}"))?;
        let login = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .map_err(|_| "timed out waiting for OneBot get_login_info".to_string())?
            .ok_or_else(|| "candidate closed before OneBot login".to_string())?
            .map_err(|error| format!("read OneBot login request: {error}"))?;
        let Message::Text(login_text) = login else {
            return Err("OneBot get_login_info request was not text".into());
        };
        let login: Value = serde_json::from_str(&login_text)
            .map_err(|error| format!("OneBot login request was not JSON: {error}"))?;
        let login_action = login.get("action").and_then(Value::as_str).map(str::to_string);
        if login_action.as_deref() != Some("get_login_info") {
            return Err(format!("unexpected OneBot initial action: {login_action:?}"));
        }
        socket
            .send(Message::Text(
                json!({"echo":"login","data":{"user_id":SELF_ID,"nickname":"ticket04-dsh-bot"}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| format!("reply to fake OneBot login: {error}"))?;
        socket
            .send(Message::Text(
                json!({
                    "post_type":"message",
                    "message_type":"group",
                    "group_id":GROUP_ID,
                    "user_id":8808,
                    "message_id":990042,
                    "self_id":SELF_ID,
                    "raw_message":INBOUND_TEXT,
                    "message":[
                        {"type":"at","data":{"qq":SELF_ID.to_string()}},
                        {"type":"text","data":{"text":INBOUND_TEXT}}
                    ],
                    "sender":{"nickname":"ticket04-dsh-user"}
                })
                .to_string()
                .into(),
            ))
            .await
            .map_err(|error| format!("send selected fake incoming message: {error}"))?;
        let deadline = Instant::now() + hold_without_send.unwrap_or(Duration::from_secs(32));
        // A tool send happens inside session/prompt, before DSH publishes turn/end. Closing
        // here on the send alone makes --no-reconnect exit while its collector is still
        // writing usage/timing. The terminal ledger record follows that bookkeeping.
        let finished = || sent.load(Ordering::SeqCst) && completed_root.as_ref().is_none_or(|root| {
            fs::read_to_string(root.join("data/turn-ledger.jsonl")).unwrap_or_default()
                .lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|record| record["k"] == "ended")
        });
        while !finished() && !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A failure scenario holds the connection for a bounded window and then closes it, so the
        // `--no-reconnect` candidate exits without a send; the caller asserts that no send happened.
        if hold_without_send.is_none() && !finished() {
            return Err("public isolated send and collector completion did not finish before deadline".into());
        }
        let _ = socket.send(Message::Close(None)).await;
        let authorization = auth.lock().unwrap().clone();
        Ok(OneBotWsObservation {
            authorization,
            login_action,
        })
    })
}

struct ServerThread<T> {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<T>>,
}

impl<T> ServerThread<T> {
    fn finish(mut self) -> T {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().expect("server thread handle").join().expect("fixture server thread panicked")
    }
}

impl<T> Drop for ServerThread<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_onebot_http(listener: TcpListener, sent: Arc<AtomicBool>) -> ServerThread<Result<Vec<OneBotObservation>, String>> {
    spawn_onebot_http_with(listener, sent, true)
}

fn spawn_onebot_http_with(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    expect_send: bool,
) -> ServerThread<Result<Vec<OneBotObservation>, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || serve_onebot_http(listener, sent, thread_stop, expect_send));
    ServerThread { stop, handle: Some(handle) }
}

fn spawn_model_sentinel(listener: TcpListener) -> ServerThread<Result<usize, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || serve_unused_model_endpoint(listener, thread_stop));
    ServerThread { stop, handle: Some(handle) }
}

fn spawn_onebot_ws(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    completed_root: &Path,
) -> ServerThread<Result<OneBotWsObservation, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let completed_root = completed_root.to_path_buf();
    let handle = thread::spawn(move || serve_onebot_ws(listener, sent, thread_stop, None, Some(completed_root)));
    ServerThread { stop, handle: Some(handle) }
}

fn spawn_onebot_ws_holding(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    hold: Duration,
) -> ServerThread<Result<OneBotWsObservation, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || serve_onebot_ws(listener, sent, thread_stop, Some(hold), None));
    ServerThread { stop, handle: Some(handle) }
}

struct ChildCapture {
    child: Option<Child>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

#[derive(Debug)]
struct ChildOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timeout: Option<String>,
}

impl ChildCapture {
    fn spawn(mut command: Command) -> Self {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        hide_process_window(&mut command);
        let mut child = command.spawn().expect("spawn hidden synthetic test child");
        let stdout = child.stdout.take().expect("child stdout pipe");
        let stderr = child.stderr.take().expect("child stderr pipe");
        Self {
            child: Some(child),
            stdout: Some(thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = (&mut std::io::BufReader::new(stdout)).read_to_end(&mut bytes);
                bytes
            })),
            stderr: Some(thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = (&mut std::io::BufReader::new(stderr)).read_to_end(&mut bytes);
                bytes
            })),
        }
    }

    fn wait(&mut self, timeout: Duration) -> ChildOutput {
        let deadline = Instant::now() + timeout;
        let mut timed_out = None;
        let status = loop {
            let child = self.child.as_mut().expect("child still owned");
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let status = child.wait().expect("wait after child polling error");
                    timed_out = Some(format!("poll child exit failed: {error}"));
                    break status;
                }
            }
            if Instant::now() >= deadline {
                let child = self.child.as_mut().expect("child still owned");
                let _ = child.kill();
                let status = child.wait().expect("wait after child timeout");
                timed_out = Some(format!("child exceeded {timeout:?}"));
                break status;
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.child.take();
        ChildOutput {
            status,
            stdout: self.stdout.take().expect("stdout reader").join().expect("stdout drain thread"),
            stderr: self.stderr.take().expect("stderr reader").join().expect("stderr drain thread"),
            timeout: timed_out,
        }
    }

    fn terminate(&mut self) -> ChildOutput {
        let child = self.child.as_mut().expect("child still owned");
        let _ = child.kill();
        let status = child.wait().expect("wait after explicit termination");
        self.child.take();
        ChildOutput {
            status,
            stdout: self.stdout.take().expect("stdout reader").join().expect("stdout drain thread"),
            stderr: self.stderr.take().expect("stderr reader").join().expect("stderr drain thread"),
            timeout: None,
        }
    }
}

impl Drop for ChildCapture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn hide_process_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn child_command(root: &Path, ambient: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .env_remove("HER_FAIL_FIRST")
        .env("TICKET04_AMBIENT_SENTINEL", "synthetic ambient profile only")
        .env("DSH_HOME", ambient.join("user/dsh"))
        .env("HOME", ambient)
        .env("USERPROFILE", ambient)
        .env("APPDATA", ambient.join("Roaming"))
        .env("LOCALAPPDATA", ambient.join("Local"))
        .env("XDG_CONFIG_HOME", ambient.join(".config"))
        .env("XDG_CACHE_HOME", ambient.join(".cache"))
        .env("XDG_DATA_HOME", ambient.join(".local/share"));
    command
}

fn spawn_console(root: &Path, ambient: &Path, port: u16) -> ChildCapture {
    let mut command = child_command(root, ambient);
    command.arg("--console-serve").arg("--console-port").arg(port.to_string());
    ChildCapture::spawn(command)
}

fn spawn_live_candidate(root: &Path, ambient: &Path, console_port: u16) -> ChildCapture {
    let mut command = child_command(root, ambient);
    command
        .arg("--live")
        .arg("--live-groups")
        .arg(format!("group:{GROUP_ID}"))
        .arg("--no-reconnect")
        .arg("--console-port")
        .arg(console_port.to_string());
    ChildCapture::spawn(command)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set health response timeout: {error}"))?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read health response: {error}"))?;
    parse_status_and_body(&bytes)
}

fn wait_console_health(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let request = format!(
                "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                if let Ok((status, body)) = read_http_response(&mut stream) {
                    if status == 200 {
                        let payload: Value = serde_json::from_slice(&body)
                            .map_err(|error| format!("console health body was not JSON: {error}"))?;
                        if payload.pointer("/runtime/mode").and_then(Value::as_str) == Some("isolated")
                            && payload.get("port").and_then(Value::as_u64) == Some(port.into())
                        {
                            return Ok(());
                        }
                        return Err(format!("console health did not report the selected runtime/port: {payload}"));
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("isolated console did not become healthy within 12 seconds".into());
        }
        thread::sleep(Duration::from_millis(40));
    }
}

/// Background-feature and production-shaped endpoint values written into the selected config.
/// The original DSH E2E keeps `FixtureOptions::original()`; background tests point the config's
/// own endpoints at counting decoys so any fallback from `dependencies.json` is observable.
struct FixtureOptions {
    config_dsh_base: String,
    config_onebot_http: String,
    config_onebot_ws: String,
    sticker_enabled: bool,
    stickers: Vec<u8>,
    slang_enabled: bool,
    slang_store: Option<Vec<u8>>,
    slang_windows: Option<Vec<u8>>,
}

impl FixtureOptions {
    fn original() -> Self {
        Self {
            config_dsh_base: "http://127.0.0.1:1".into(),
            config_onebot_http: "http://127.0.0.1:1".into(),
            config_onebot_ws: "ws://127.0.0.1:1".into(),
            sticker_enabled: false,
            stickers: b"[]".to_vec(),
            slang_enabled: false,
            slang_store: None,
            slang_windows: None,
        }
    }
}

fn selected_fixture(root: &Path, dsh_url: &str, model_url: &str, onebot_url: &str, onebot_ws_url: &str) {
    selected_fixture_with(root, dsh_url, model_url, onebot_url, onebot_ws_url, &FixtureOptions::original());
}

fn selected_fixture_with(
    root: &Path,
    dsh_url: &str,
    model_url: &str,
    onebot_url: &str,
    onebot_ws_url: &str,
    options: &FixtureOptions,
) {
    fs::create_dir_all(root.join("config")).expect("create selected config area");
    fs::create_dir_all(root.join("data/logs")).expect("create selected data/log area");
    fs::create_dir_all(root.join("resources")).expect("create selected resources area");
    fs::create_dir_all(root.join("user/dsh")).expect("create selected DSH user area");
    // Keep the original fixture's slang block byte-for-byte; enabled scenarios raise the extraction
    // threshold so feeding the selected learning window never starts a model extraction.
    let slang_config = if options.slang_enabled {
        json!({"enabled":true,"extractMinMessages":1000})
    } else {
        json!({"enabled":false})
    };
    fs::write(
        root.join("config/config.json"),
        json!({
            "consoleToken": CONSOLE_TOKEN,
            "ownerQQ": 8808,
            "allow":{"groups":[GROUP_ID],"private":[]},
            "dsh":{
                "baseUrl":options.config_dsh_base,
                "provider":PROVIDER,
                "model":MODEL,
                "reasoningEffort":"medium",
                "fallbacks":[]
            },
            "snowluma":{
                "accessToken":ONEBOT_TOKEN,
                "httpUrl":options.config_onebot_http,
                "wsUrl":options.config_onebot_ws
            },
            "socialV2":{
                "agentPreset":"ticket04-selected-group-preset",
                "wake":{"batchWindowMs":1000,"maxWakePerMinute":8,"maxWakePerHour":24},
                "social":{"skipProbability":0.0},
                "tools":{"sendMessage":true,"getPrompt":true},
                "send":{"mergeMultiIntoOne":true,"maxMessageChars":800,"maxSendPerMinute":10,"maxSendPerHour":20,"burstIntervalMinMs":0,"maxGapMs":0},
                "sticker":{"enabled":options.sticker_enabled}
            },
            "slang":slang_config,
            "rollout":{"enabled":false,"rustGroups":[]}
        })
        .to_string(),
    )
    .expect("write selected config");
    let mut conversations = serde_json::Map::new();
    conversations.insert(
        format!("group:{GROUP_ID}"),
        json!({
            "agentToken":SESSION_TOKEN,
            "unread":[],
            "recentMessages":[],
            "wakeConfig":{
                "mode":"active",
                "infinite":true,
                "triggers":{"atMention":true,"nameMention":false,"question":false,"poke":false,"anyMessage":false,"probability":0.0},
                "batchWindowMs":1000
            }
        }),
    );
    fs::write(
        root.join("data/social-v2.json"),
        json!({"conversations":conversations})
        .to_string(),
    )
    .expect("write selected social state");
    fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#)
        .expect("write selected mode");
    fs::write(
        root.join("data/role-map.json"),
        format!(r#"{{"sessions":{{"group:{GROUP_ID}":"ticket04-selected-dsh-role"}}}}"#),
    )
    .expect("write selected role map");
    fs::write(root.join("data/stickers.json"), &options.stickers).expect("write selected stickers");
    if let Some(bytes) = &options.slang_store {
        fs::write(root.join("data/slang.json"), bytes).expect("write selected slang store");
    }
    if let Some(bytes) = &options.slang_windows {
        fs::write(root.join("data/slang-windows.json"), bytes).expect("write selected slang windows");
    }
    fs::write(
        root.join("user/dsh/.credentials.yaml"),
        secret_text(&SELECTED_SECRET),
    )
    .expect("write synthetic selected DSH credential");
    fs::write(
        root.join("dependencies.json"),
        json!({"dsh":dsh_url,"model":model_url,"onebot":onebot_url,"onebot_ws":onebot_ws_url}).to_string(),
    )
    .expect("write selected local dependencies");
}

fn write_ambient_decoy(ambient: &Path) -> Vec<u8> {
    let path = ambient.join("user/dsh/.credentials.yaml");
    fs::create_dir_all(path.parent().expect("ambient DSH parent"))
        .expect("create ambient DSH profile");
    let contents = secret_text(&AMBIENT_SECRET).into_bytes();
    fs::write(&path, &contents).expect("write synthetic ambient DSH credential decoy");
    contents
}

fn normalize_path(value: &str) -> String {
    // TEMP may use an 8.3 ancestor (RUNNER~1); compare filesystem identities after
    // expanding both spellings, including paths returned by the candidate.
    let canonical = fs::canonicalize(value).expect("canonicalize an existing fixture path");
    let value = canonical.to_string_lossy();
    let mut normalized = value.replace('/', "\\");
    if let Some(unc) = normalized.strip_prefix("\\\\?\\UNC\\") {
        normalized = format!("\\\\{unc}");
    } else if let Some(dos) = normalized.strip_prefix("\\\\?\\") {
        normalized = dos.to_string();
    }
    normalized
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

fn bounded_diagnostic_bytes(bytes: &[u8]) -> String {
    const LIMIT: usize = 12 * 1024;
    let shown = bytes.len().min(LIMIT);
    let mut text = String::from_utf8_lossy(&bytes[..shown]).into_owned();
    if bytes.len() > shown {
        text.push_str(&format!("\n<truncated {} bytes>", bytes.len() - shown));
    }
    text
}

fn bounded_diagnostic_file(path: &Path) -> String {
    const LIMIT: u64 = 12 * 1024;
    let mut bytes = Vec::new();
    match fs::File::open(path).and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes)) {
        Ok(_) => bounded_diagnostic_bytes(&bytes),
        Err(error) => format!("<unavailable {}: {error}>", path.display()),
    }
}

#[test]
fn isolated_live_dsh_routes_selected_prompt_through_public_console_to_fake_onebot() {
    let roots = TempRoots::new();
    let ambient_original = write_ambient_decoy(&roots.ambient);
    let (dsh_listener, dsh_authority) = bind_loopback();
    let dsh_url = format!("http://{dsh_authority}");
    let (model_listener, model_url) = bind_http_url();
    let (onebot_listener, onebot_url) = bind_http_url();
    let (onebot_ws_listener, onebot_ws_url) = bind_ws_url();
    let console_reservation = crate::support::ports::reserve();
    let console_port = console_reservation.port();
    assert_ne!(
        onebot_url, onebot_ws_url.replace("ws://", "http://"),
        "HTTP and WebSocket dependencies need distinct explicitly configured ports"
    );
    selected_fixture(
        &roots.selected,
        &dsh_url,
        &model_url,
        &onebot_url,
        &onebot_ws_url,
    );

    let onebot_sent = Arc::new(AtomicBool::new(false));
    let mut dsh_server = spawn_dsh_server(dsh_listener, dsh_authority.clone(), console_port);
    let mut model_server = spawn_model_sentinel(model_listener);
    let mut onebot_http_server = spawn_onebot_http(onebot_listener, onebot_sent.clone());
    let mut onebot_ws_server = spawn_onebot_ws(onebot_ws_listener, onebot_sent.clone(), &roots.selected);

    drop(console_reservation); // free the held port on the line before the console spawn
    let mut console = spawn_console(&roots.selected, &roots.ambient, console_port);
    let health = wait_console_health(console_port);
    let candidate_output = if health.is_ok() {
        let mut candidate = spawn_live_candidate(&roots.selected, &roots.ambient, console_port);
        Some(candidate.wait(Duration::from_secs(38)))
    } else {
        None
    };
    let console_output = console.terminate();
    let dsh_result = dsh_server.finish();
    let model_result = model_server.finish();
    let onebot_http_result = onebot_http_server.finish();
    let onebot_ws_result = onebot_ws_server.finish();

    let console_diagnostic = format!(
        "timeout={:?}\nstdout:\n{}\nstderr:\n{}",
        console_output.timeout,
        bounded_diagnostic_bytes(&console_output.stdout),
        bounded_diagnostic_bytes(&console_output.stderr),
    );
    let candidate_diagnostic = candidate_output.as_ref().map_or_else(
        || "<candidate was not started because console health failed>".to_string(),
        |output| {
            format!(
                "exit={:?} timeout={:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                output.timeout,
                bounded_diagnostic_bytes(&output.stdout),
                bounded_diagnostic_bytes(&output.stderr),
            )
        },
    );
    let selected_logs = [
        "data/rust-heartbeat-live.json",
        "data/sessions.json",
        "data/social-v2.json",
        "data/logs/rbridge-live.jsonl",
        "data/logs/rbridge-live-wakes.jsonl",
        "data/logs/turn-timing.jsonl",
        "data/token-usage-rust.jsonl",
        "data/turn-ledger.jsonl",
    ]
    .into_iter()
    .map(|relative| {
        let path = roots.selected.join(relative);
        format!("{relative}:\n{}", bounded_diagnostic_file(&path))
    })
    .collect::<Vec<_>>()
    .join("\n");
    eprintln!(
        "ticket04 DSH E2E diagnostics\nhealth={health:?}\nconsole child:\n{console_diagnostic}\nlive candidate:\n{candidate_diagnostic}\nfake DSH result:\n{}\nfake model result:\n{}\nfake OneBot HTTP result:\n{}\nfake OneBot WS result:\n{}\nselected files/logs:\n{selected_logs}",
        bounded_diagnostic_bytes(format!("{dsh_result:?}").as_bytes()),
        bounded_diagnostic_bytes(format!("{model_result:?}").as_bytes()),
        bounded_diagnostic_bytes(format!("{onebot_http_result:?}").as_bytes()),
        bounded_diagnostic_bytes(format!("{onebot_ws_result:?}").as_bytes()),
    );

    assert!(
        health.is_ok(),
        "selected isolated console startup failed: {health:?}; stdout={}; stderr={}",
        String::from_utf8_lossy(&console_output.stdout),
        String::from_utf8_lossy(&console_output.stderr)
    );
    let candidate_output = candidate_output.expect("candidate starts only after console health succeeds");
    let candidate_stdout = String::from_utf8_lossy(&candidate_output.stdout);
    let candidate_stderr = String::from_utf8_lossy(&candidate_output.stderr);
    assert!(
        candidate_output.timeout.is_none(),
        "candidate did not exit in time: {:?}; stdout={candidate_stdout}; stderr={candidate_stderr}",
        candidate_output.timeout
    );
    assert!(
        candidate_output.status.success(),
        "isolated live candidate exited {:?}; stdout={candidate_stdout}; stderr={candidate_stderr}",
        candidate_output.status.code()
    );
    assert!(
        health.is_ok(),
        "console child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&console_output.stdout),
        String::from_utf8_lossy(&console_output.stderr)
    );
    let dsh = dsh_result.expect("fake DSH fixture must complete");
    assert!(dsh.errors.is_empty(), "fake DSH errors: {:?}", dsh.errors);
    let onebot_http = onebot_http_result.expect("fake OneBot HTTP fixture must complete");
    let onebot_ws = onebot_ws_result.expect("fake OneBot WS fixture must complete");
    assert_eq!(model_result.expect("model sentinel fixture must finish"), 0, "DSH path must not use the unrelated selected model endpoint");

    let expected_paths = [
        "/api/workspace/create",
        "/api/session/create",
        "/api/workspace/rename",
        "/api/session/selectModel",
        "/api/session/prompt",
    ];
    let actual_paths: Vec<&str> = dsh.rpc.iter().map(|request| request.path.as_str()).collect();
    for expected in expected_paths {
        assert!(actual_paths.contains(&expected), "missing selected DSH request {expected}: {actual_paths:?}");
    }
    assert!(
        dsh.rpc.iter().all(|request| request.selected_cookie_valid),
        "every selected DSH RPC must use a cookie signed by user/dsh/.credentials.yaml"
    );
    assert!(dsh.ws.as_ref().is_some_and(|ws| ws.selected_cookie_valid), "DSH follow WebSocket must use the selected signed cookie");
    let workspace = dsh.rpc.iter().find(|request| request.method == "workspace/create").expect("workspace create RPC");
    let workspace_path = workspace.args.pointer("/request/path").and_then(Value::as_str).expect("workspace path in DSH request");
    assert_eq!(
        normalize_path(workspace_path),
        normalize_path(&roots.selected.join("data/agents").to_string_lossy()),
        "workspace creation must use the selected runtime data area"
    );
    let session_create = dsh.rpc.iter().find(|request| request.method == "session/create").expect("session create RPC");
    assert_eq!(session_create.args.pointer("/request/agentPreset").and_then(Value::as_str), Some("ticket04-selected-group-preset"));
    let model_select = dsh.rpc.iter().find(|request| request.method == "session/selectModel").expect("model selection RPC");
    assert_eq!(model_select.args.pointer("/request/provider").and_then(Value::as_str), Some(PROVIDER));
    assert_eq!(model_select.args.pointer("/request/model").and_then(Value::as_str), Some(MODEL));
    assert_eq!(model_select.args.pointer("/request/reasoningEffort").and_then(Value::as_str), Some("medium"));
    let prompt = dsh.prompt.as_deref().expect("actual prompt delivered through session/prompt");
    // The fixture conversation has never woken (`bootstrapSent` absent), so this first wake is the bootstrap wake.
    assert!(prompt.contains(&format!("【引导唤醒】你已接入 QQ 会话 group:{GROUP_ID}")), "selected group key missing from prompt");
    assert!(prompt.contains(INBOUND_TEXT), "real OneBot inbound text missing from the DSH prompt");
    // Live 114e5c25 replaces the compatibility full render at DSH submission, including
    // bootstrap wakes: concise status plus one authenticated packet carry the live event.
    assert!(
        prompt.contains("【此刻状态】未读 1 条；lastAiReplyAt=null"),
        "status must carry the selected live unread count and reply state"
    );
    assert_eq!(
        prompt.matches(INBOUND_TEXT).count(),
        1,
        "the live event appears in full once in the authenticated wake snapshot"
    );
    let snapshot_start = prompt.find("【本次消息快照】").expect("wake snapshot section");
    assert_eq!(
        prompt[snapshot_start..].matches(INBOUND_TEXT).count(),
        1,
        "the wake snapshot carries the inbound text exactly once"
    );
    let wake_start = prompt.find("【当前唤醒】").expect("existing wake section");
    let snapshot: Value = serde_json::Deserializer::from_str(
        &prompt[snapshot_start + "【本次消息快照】".len()..],
    )
    .into_iter::<Value>()
    .next()
    .expect("wake snapshot JSON")
    .expect("valid wake snapshot JSON");
    let messages = snapshot["messages"].as_array().expect("snapshot unread messages");
    assert_eq!(messages.len(), 1, "same message ID appears once in the authenticated packet");
    let event = &messages[0];
    assert_eq!(event["messageId"], "990042");
    assert_eq!(event["sender"], "ticket04-dsh-user", "snapshot must use the selected live sender");
    assert_eq!(event["userId"], "8808");
    assert_eq!(event["text"], INBOUND_RENDERED, "snapshot must carry the complete rendered live event");
    assert_eq!(event["seq"], 1);
    assert_eq!(event["inboundKey"], format!("group:{GROUP_ID}|message:990042"));
    assert_eq!(snapshot["unreadCount"], 1);
    assert_eq!(snapshot["includedUnreadCount"], 1);
    assert_eq!(snapshot["readThroughSeq"], 1);
    assert_eq!(snapshot["partial"], false);
    let event_line = prompt[snapshot_start..]
        .find(INBOUND_RENDERED)
        .map(|offset| snapshot_start + offset)
        .expect("actual selected OneBot event in authenticated snapshot");
    assert!(wake_start < snapshot_start && snapshot_start < event_line, "live wake/snapshot ordering changed: {prompt}");
    assert!(prompt.contains("ticket04-selected-dsh-role"), "selected-root role missing from DSH prompt");
    assert!(prompt.contains(SESSION_TOKEN), "selected session token missing from actual DSH prompt");
    assert_eq!(dsh.ws.as_ref().unwrap().open_frame.pointer("/payload/args/request/address/sessionId").and_then(Value::as_str), Some(SESSION_ID));

    let tool = dsh.tool.as_ref().expect("fake DSH observed a real prompt and attempted the public tool route").as_ref().expect("public console send-message request was sent");
    assert_eq!(tool.path, "/api/socialV2/send-message");
    assert_eq!(tool.status, 200, "public selected-root route rejected the prompt-derived tool call: {}", tool.response);
    assert_eq!(tool.request["key"], format!("group:{GROUP_ID}"));
    assert_eq!(tool.request["messages"][0], OUTBOUND_TEXT);
    assert_eq!(tool.console_token_header, CONSOLE_TOKEN);
    assert_eq!(tool.agent_token_header, SESSION_TOKEN);
    assert_eq!(tool.response["ok"], true);
    assert_eq!(tool.response["key"], format!("group:{GROUP_ID}"));
    assert_eq!(tool.response["sent"], 1);

    let expected_authorization = format!("Bearer {ONEBOT_TOKEN}");
    assert_eq!(onebot_ws.authorization.as_deref(), Some(expected_authorization.as_str()));
    assert_eq!(onebot_ws.login_action.as_deref(), Some("get_login_info"));
    let send = onebot_http.iter().find(|observation| observation.request_line.starts_with("POST /send_group_msg ")).expect("public route's selected OneBot send");
    assert!(send.headers.to_ascii_lowercase().contains(&format!("authorization: bearer {ONEBOT_TOKEN}").to_ascii_lowercase()), "OneBot HTTP request did not use the selected token: {}", send.headers);
    assert_eq!(send.body["group_id"], GROUP_ID);
    assert!(send.body["message"].as_array().is_some_and(|segments| segments.iter().any(|segment| segment.pointer("/data/text").and_then(Value::as_str) == Some(OUTBOUND_TEXT))), "OneBot did not receive the actual prompt-derived outbound text: {}", send.body);

    let session_file = roots.selected.join("data/sessions.json");
    let sessions: Value = serde_json::from_slice(&fs::read(&session_file).expect("selected Rust session map")).expect("selected session map JSON");
    assert_eq!(sessions["workspaceId"], WORKSPACE_ID);
    let key = format!("group:{GROUP_ID}");
    let stored_session = sessions["sessions"].get(key.as_str()).expect("stored selected group session");
    assert_eq!(stored_session, SESSION_ID, "the unified mapping stores the JS format: key -> sessionId");
    assert!(roots.selected.join("data/agents").is_dir(), "selected DSH workspace directory missing");

    let selected_state_path = roots.selected.join("data/social-v2.json");
    let selected_state: Value = serde_json::from_slice(&fs::read(&selected_state_path).expect("selected social state after public send")).expect("selected social state JSON");
    let conversation = selected_state["conversations"]
        .get(key.as_str())
        .expect("selected conversation remains in selected social state");
    assert_eq!(conversation["agentToken"], SESSION_TOKEN);
    assert!(conversation["lastAiReplyAt"].as_i64().unwrap_or(0) > 0, "public send did not update the selected conversation state");
    assert!(conversation["sendTimes"].as_array().is_some_and(|times| !times.is_empty()), "selected conversation did not record the successful send");

    let heartbeat: Value = serde_json::from_slice(&fs::read(roots.selected.join("data/rust-heartbeat-live.json")).expect("selected live heartbeat")).expect("heartbeat JSON");
    assert_eq!(heartbeat["live"], true);
    assert_eq!(heartbeat["groups"][0], format!("group:{GROUP_ID}"));
    let decision = fs::read_to_string(roots.selected.join("data/logs/rbridge-live.jsonl")).expect("selected decision log");
    let wakes = fs::read_to_string(roots.selected.join("data/logs/rbridge-live-wakes.jsonl")).expect("selected wake log");
    let timing = fs::read_to_string(roots.selected.join("data/logs/turn-timing.jsonl")).expect("selected turn timing log");
    assert!(decision.contains("\"type\":\"start\""));
    assert!(wakes.contains(&format!("group:{GROUP_ID}")));
    assert!(timing.contains(&format!("group:{GROUP_ID}")) && timing.contains("\"ok\":true"));
    let ledger = fs::read_to_string(roots.selected.join("data/token-usage-rust.jsonl")).expect("selected DSH usage ledger");
    let ledger_record: Value = serde_json::from_str(ledger.lines().next().expect("ledger row")).expect("ledger row JSON");
    assert_eq!(ledger_record["sid"], SESSION_ID);
    assert_eq!(ledger_record["key"], format!("group:{GROUP_ID}"));
    assert_eq!(ledger_record["model"], MODEL);
    assert_eq!(ledger_record["provider"], PROVIDER);
    assert_eq!(ledger_record["b"]["output"], 3.0);

    assert_eq!(
        fs::read(roots.ambient.join("user/dsh/.credentials.yaml")).expect("ambient synthetic DSH credential remains"),
        ambient_original,
        "selected runtime must not modify ambient DSH credentials"
    );
    assert!(
        !candidate_stdout.contains(SESSION_TOKEN)
            && !candidate_stdout.contains(ONEBOT_TOKEN)
            && !candidate_stdout.contains(&URL_SAFE_NO_PAD.encode(SELECTED_SECRET)),
        "candidate stdout leaked a synthetic credential"
    );
    assert!(
        !candidate_stderr.contains(SESSION_TOKEN)
            && !candidate_stderr.contains(ONEBOT_TOKEN)
            && !candidate_stderr.contains(&URL_SAFE_NO_PAD.encode(SELECTED_SECRET)),
        "candidate stderr leaked a synthetic credential"
    );
}

// ---------------------------------------------------------------------------------------------
// Background I/O, replaceable endpoints and failure fallback scenarios.
// ---------------------------------------------------------------------------------------------

const STICKER_SENTINEL: &str = "ticket04-selected-sticker-sentinel";
const CORRUPT_STICKERS: &[u8] = b"{ticket04 corrupt selected stickers";
const CORRUPT_SLANG: &[u8] = b"[ticket04 corrupt selected slang";
const CORRUPT_WINDOWS: &[u8] = b"{ticket04 corrupt selected learning windows";

/// One request seen by the failing selected DSH dependency.
#[derive(Debug, Clone)]
struct FailingDshRequest {
    path: String,
    selected_cookie_valid: bool,
}

/// The selected DSH passes the readiness probe but fails the turn's RPCs.
fn serve_failing_dsh(
    listener: TcpListener,
    authority: String,
    stop: Arc<AtomicBool>,
) -> Result<Vec<FailingDshRequest>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set failing DSH listener nonblocking: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(42);
    let mut requests = Vec::new();
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let mut stream = match accept_fixture(&listener, Instant::now() + Duration::from_millis(250)) {
            Ok(stream) => stream,
            Err(error) if error.contains("timed out") => continue,
            Err(error) => return Err(error),
        };
        let _ = stream.set_nonblocking(false);
        // A connection that never sends a byte (the bridge closes a reconnect in flight at exit)
        // carries no request; skip it instead of failing the whole fixture.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        if !matches!(stream.peek(&mut [0u8; 1]), Ok(count) if count > 0) {
            continue;
        }
        let request = parse_sync_http_request(&mut stream)?;
        requests.push(FailingDshRequest {
            path: parse_path(&request.request_line).to_string(),
            selected_cookie_valid: cookie_uses_secret(
                request.headers.get("cookie").map(String::as_str),
                &SELECTED_SECRET,
                &authority,
            ),
        });
        if parse_path(&request.request_line) == "/api/settings/describe" {
            let envelope: Value = serde_json::from_slice(&request.body)
                .map_err(|error| format!("DSH probe body was not JSON: {error}"))?;
            send_sync_json(
                &mut stream,
                200,
                &json!({"type":"server-response","rpcId":envelope["rpcId"],"result":{"ok":true,"value":{"namespaces":[]}}}),
            )?;
        } else {
            send_sync_json(&mut stream, 503, &json!({"error":"selected fixture DSH is unavailable"}))?;
        }
    }
    Ok(requests)
}

fn spawn_failing_dsh(
    listener: TcpListener,
    authority: String,
) -> ServerThread<Result<Vec<FailingDshRequest>, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || serve_failing_dsh(listener, authority, thread_stop));
    ServerThread { stop, handle: Some(handle) }
}

/// Relative paths of every file below `root`, sorted, using `/` separators.
fn file_inventory(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if let Ok(relative) = path.strip_prefix(base) {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

enum DshMode {
    /// The fake DSH observes the real prompt and relays a public send-message tool call.
    Working,
    /// The selected DSH answers every RPC with 503; the turn must fail without any fallback.
    Failing,
}

struct ScenarioRun {
    roots: TempRoots,
    ambient_credentials: Vec<u8>,
    health: Result<(), String>,
    candidate: Option<ChildOutput>,
    dsh: Option<Result<DshCapture, String>>,
    failing_dsh: Option<Result<Vec<FailingDshRequest>, String>>,
    model_contacts: Result<usize, String>,
    config_dsh_contacts: Result<usize, String>,
    config_onebot_http_contacts: Result<usize, String>,
    config_onebot_ws_contacts: Result<usize, String>,
    onebot_http: Result<Vec<OneBotObservation>, String>,
    onebot_ws: Result<OneBotWsObservation, String>,
    onebot_sent: bool,
}

/// Run the public console plus the isolated live candidate against started fakes. The selected
/// config's own `dsh.baseUrl` / `snowluma.httpUrl` / `snowluma.wsUrl` point at counting decoys:
/// the candidate must reach only the `dependencies.json` endpoints.
fn run_background_scenario(mode: DshMode, mut options: FixtureOptions) -> ScenarioRun {
    let roots = TempRoots::new();
    let ambient_credentials = write_ambient_decoy(&roots.ambient);
    let (dsh_listener, dsh_authority) = bind_loopback();
    let dsh_url = format!("http://{dsh_authority}");
    let (model_listener, model_url) = bind_http_url();
    let (onebot_listener, onebot_url) = bind_http_url();
    let (onebot_ws_listener, onebot_ws_url) = bind_ws_url();
    let (config_dsh_listener, config_dsh_url) = bind_http_url();
    let (config_http_listener, config_http_url) = bind_http_url();
    let (config_ws_listener, config_ws_url) = bind_ws_url();
    let console_reservation = crate::support::ports::reserve();
    let console_port = console_reservation.port();
    options.config_dsh_base = config_dsh_url;
    options.config_onebot_http = config_http_url;
    options.config_onebot_ws = config_ws_url;
    selected_fixture_with(&roots.selected, &dsh_url, &model_url, &onebot_url, &onebot_ws_url, &options);

    let onebot_sent = Arc::new(AtomicBool::new(false));
    let (mut dsh_server, mut failing_server) = match mode {
        DshMode::Working => (Some(spawn_dsh_server(dsh_listener, dsh_authority, console_port)), None),
        DshMode::Failing => (None, Some(spawn_failing_dsh(dsh_listener, dsh_authority))),
    };
    let model_server = spawn_model_sentinel(model_listener);
    let config_dsh_server = spawn_model_sentinel(config_dsh_listener);
    let config_http_server = spawn_model_sentinel(config_http_listener);
    let config_ws_server = spawn_model_sentinel(config_ws_listener);
    let onebot_http_server = match mode {
        DshMode::Working => spawn_onebot_http(onebot_listener, onebot_sent.clone()),
        DshMode::Failing => spawn_onebot_http_with(onebot_listener, onebot_sent.clone(), false),
    };
    let onebot_ws_server = match mode {
        DshMode::Working => spawn_onebot_ws(onebot_ws_listener, onebot_sent.clone(), &roots.selected),
        DshMode::Failing => {
            spawn_onebot_ws_holding(onebot_ws_listener, onebot_sent.clone(), Duration::from_secs(10))
        }
    };

    drop(console_reservation); // free the held port on the line before the console spawn
    let mut console = spawn_console(&roots.selected, &roots.ambient, console_port);
    let health = wait_console_health(console_port);
    let candidate = if health.is_ok() {
        let mut candidate = spawn_live_candidate(&roots.selected, &roots.ambient, console_port);
        Some(candidate.wait(Duration::from_secs(38)))
    } else {
        None
    };
    let console_output = console.terminate();
    let dsh = dsh_server.take().map(ServerThread::finish);
    let failing_dsh = failing_server.take().map(ServerThread::finish);
    let run = ScenarioRun {
        ambient_credentials,
        health,
        candidate,
        dsh,
        failing_dsh,
        model_contacts: model_server.finish(),
        config_dsh_contacts: config_dsh_server.finish(),
        config_onebot_http_contacts: config_http_server.finish(),
        config_onebot_ws_contacts: config_ws_server.finish(),
        onebot_http: onebot_http_server.finish(),
        onebot_ws: onebot_ws_server.finish(),
        onebot_sent: onebot_sent.load(Ordering::SeqCst),
        roots,
    };
    let candidate_diagnostic = run.candidate.as_ref().map_or_else(
        || "<not started>".to_string(),
        |output| {
            format!(
                "exit={:?} timeout={:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                output.timeout,
                bounded_diagnostic_bytes(&output.stdout),
                bounded_diagnostic_bytes(&output.stderr)
            )
        },
    );
    let dsh_diagnostic = format!(
        "{:?}",
        run.dsh
            .as_ref()
            .map(|result| result.as_ref().map(|capture| (&capture.rpc, &capture.errors, &capture.tool)))
    );
    eprintln!(
        "ticket04 background scenario diagnostics\nhealth={:?}\nconsole stdout:\n{}\nconsole stderr:\n{}\ncandidate:\n{candidate_diagnostic}\nDSH={}\nfailing DSH={:?}\nmodel={:?} configDsh={:?} configHttp={:?} configWs={:?}\nOneBot HTTP={:?}\nOneBot WS={:?}\nselected files={:?}\nambient files={:?}",
        run.health,
        bounded_diagnostic_bytes(&console_output.stdout),
        bounded_diagnostic_bytes(&console_output.stderr),
        bounded_diagnostic_bytes(dsh_diagnostic.as_bytes()),
        run.failing_dsh,
        run.model_contacts,
        run.config_dsh_contacts,
        run.config_onebot_http_contacts,
        run.config_onebot_ws_contacts,
        run.onebot_http,
        run.onebot_ws,
        file_inventory(&run.roots.selected),
        file_inventory(&run.roots.ambient),
    );
    run
}

impl ScenarioRun {
    fn assert_candidate_exited_cleanly(&self) -> String {
        assert!(self.health.is_ok(), "selected isolated console startup failed: {:?}", self.health);
        let output = self.candidate.as_ref().expect("candidate starts after console health");
        assert!(output.timeout.is_none(), "candidate did not exit in time: {:?}", output.timeout);
        assert!(output.status.success(), "isolated live candidate exited {:?}", output.status.code());
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn assert_no_fallback_endpoint_was_contacted(&self) {
        assert_eq!(
            self.model_contacts.as_ref().ok().copied(),
            Some(0),
            "DSH path must not contact the selected model endpoint"
        );
        assert_eq!(
            self.config_dsh_contacts.as_ref().ok().copied(),
            Some(0),
            "config dsh.baseUrl must not replace the selected DSH dependency"
        );
        assert_eq!(
            self.config_onebot_http_contacts.as_ref().ok().copied(),
            Some(0),
            "config snowluma.httpUrl must not replace the selected OneBot dependency"
        );
        assert_eq!(
            self.config_onebot_ws_contacts.as_ref().ok().copied(),
            Some(0),
            "config snowluma.wsUrl must not replace the selected OneBot WebSocket dependency"
        );
    }

    fn assert_ambient_profile_untouched(&self) {
        assert_eq!(
            file_inventory(&self.roots.ambient),
            vec!["user/dsh/.credentials.yaml".to_string()],
            "the candidate created files under the ambient HOME/USERPROFILE/APPDATA profile"
        );
        assert_eq!(
            fs::read(self.roots.ambient.join("user/dsh/.credentials.yaml"))
                .expect("ambient credential decoy"),
            self.ambient_credentials,
            "selected runtime modified the ambient DSH credential"
        );
    }

    fn assert_selected_heartbeat_and_gate(&self) {
        let heartbeat: Value =
            serde_json::from_slice(&self.selected_bytes("data/rust-heartbeat-live.json"))
                .expect("heartbeat JSON");
        assert_eq!(heartbeat["live"], true);
        assert_eq!(heartbeat["groups"][0], format!("group:{GROUP_ID}"));
        let gate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.roots.selected.join("data/logs/rbridge-live.lock"))
            .expect("selected-root live gate persists");
        gate.try_lock()
            .expect("candidate exit releases the selected-root live gate");
        let decision = String::from_utf8(self.selected_bytes("data/logs/rbridge-live.jsonl"))
            .expect("selected decision log text");
        assert!(decision.contains("\"type\":\"start\""));
        let wakes = String::from_utf8(self.selected_bytes("data/logs/rbridge-live-wakes.jsonl"))
            .expect("selected wake log text");
        assert!(wakes.contains(&format!("group:{GROUP_ID}")));
    }

    fn selected_bytes(&self, relative: &str) -> Vec<u8> {
        fs::read(self.roots.selected.join(relative))
            .unwrap_or_else(|error| panic!("read selected {relative}: {error}"))
    }

    fn working_prompt(&self) -> String {
        let dsh = self
            .dsh
            .as_ref()
            .expect("working DSH fixture")
            .as_ref()
            .expect("fake DSH fixture completes");
        assert!(dsh.errors.is_empty(), "fake DSH errors: {:?}", dsh.errors);
        assert!(
            dsh.rpc.iter().all(|request| request.selected_cookie_valid),
            "every DSH RPC uses the selected credential"
        );
        let tool = dsh
            .tool
            .as_ref()
            .expect("fake DSH relayed a tool call")
            .as_ref()
            .expect("public send-message request sent");
        assert_eq!(tool.status, 200, "public selected send-message route rejected the relay: {}", tool.response);
        let sends = self.onebot_http.as_ref().expect("fake OneBot HTTP completes");
        let send = sends
            .iter()
            .find(|observation| observation.request_line.starts_with("POST /send_group_msg "))
            .expect("selected OneBot receives the relayed send");
        assert_eq!(send.body["group_id"], GROUP_ID);
        assert!(self.onebot_ws.is_ok(), "fake OneBot WS: {:?}", self.onebot_ws);
        let ledger = String::from_utf8(self.selected_bytes("data/token-usage-rust.jsonl"))
            .expect("selected usage ledger text");
        let row: Value = serde_json::from_str(ledger.lines().next().expect("ledger row"))
            .expect("ledger JSON");
        assert_eq!(row["sid"], SESSION_ID);
        assert_eq!(row["key"], format!("group:{GROUP_ID}"));
        dsh.prompt.clone().expect("actual prompt delivered through session/prompt")
    }
}

#[test]
fn isolated_live_dsh_background_io_stays_in_selected_root_and_ignores_config_endpoints() {
    let stickers = json!([
        {"id":"ticket04-sticker-1","desc":STICKER_SENTINEL,"useCount":3,"tags":["ticket04"]}
    ])
    .to_string()
    .into_bytes();
    let slang = json!([
        {"content":"ticket04-selected-slang","meaning":"selected only","status":"confirmed","count":2}
    ])
    .to_string()
    .into_bytes();
    let mut options = FixtureOptions::original();
    options.sticker_enabled = true;
    options.stickers = stickers.clone();
    options.slang_enabled = true;
    options.slang_store = Some(slang.clone());
    let run = run_background_scenario(DshMode::Working, options);

    run.assert_candidate_exited_cleanly();
    let prompt = run.working_prompt();
    assert!(prompt.contains("【可用表情包】"), "enabled sticker context missing from the live DSH prompt");
    assert!(prompt.contains(STICKER_SENTINEL), "live DSH prompt did not read the selected sticker store");
    assert!(
        prompt.contains("你的 QQ 收藏表情里有 1 个表情"),
        "sticker context counted entries outside the selected store"
    );
    assert_eq!(run.selected_bytes("data/stickers.json"), stickers, "prompt sticker read rewrote the selected store");
    assert_eq!(run.selected_bytes("data/slang.json"), slang, "learning-window feed rewrote the selected slang store");

    let windows: Value = serde_json::from_slice(&run.selected_bytes("data/slang-windows.json"))
        .expect("selected learning windows JSON");
    let window = windows["windows"][format!("group:{GROUP_ID}")]
        .as_array()
        .expect("selected group learning window");
    assert_eq!(window.len(), 1, "selected learning window holds exactly the live inbound message");
    assert_eq!(window[0]["text"], INBOUND_RENDERED);
    let learning_gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(run.roots.selected.join("data/slang-windows.lock"))
        .expect("selected learning-window gate");
    learning_gate
        .try_lock()
        .expect("learning-window feed releases its selected gate");

    run.assert_selected_heartbeat_and_gate();
    run.assert_no_fallback_endpoint_was_contacted();
    run.assert_ambient_profile_untouched();
}

#[test]
fn isolated_live_dsh_corrupt_selected_background_stores_fail_closed_without_fallback() {
    let mut options = FixtureOptions::original();
    options.sticker_enabled = true;
    options.stickers = CORRUPT_STICKERS.to_vec();
    options.slang_enabled = true;
    options.slang_store = Some(CORRUPT_SLANG.to_vec());
    options.slang_windows = Some(CORRUPT_WINDOWS.to_vec());
    let run = run_background_scenario(DshMode::Working, options);

    let stdout = run.assert_candidate_exited_cleanly();
    let prompt = run.working_prompt();
    assert!(
        !prompt.contains("【可用表情包】"),
        "a corrupt selected sticker store must yield no sticker list rather than another store's stickers"
    );
    assert!(
        stdout.contains("[slang] 候选黑话窗口读写失败"),
        "corrupt selected learning window was not reported"
    );
    assert_eq!(run.selected_bytes("data/stickers.json"), CORRUPT_STICKERS, "corrupt selected sticker store was rewritten");
    assert_eq!(run.selected_bytes("data/slang.json"), CORRUPT_SLANG, "corrupt selected slang store was rewritten");
    assert_eq!(
        run.selected_bytes("data/slang-windows.json"),
        CORRUPT_WINDOWS,
        "corrupt selected learning window was rewritten"
    );

    run.assert_selected_heartbeat_and_gate();
    run.assert_no_fallback_endpoint_was_contacted();
    run.assert_ambient_profile_untouched();
}

#[test]
fn isolated_live_dsh_selected_dsh_failure_does_not_fall_back_to_config_or_ambient() {
    let run = run_background_scenario(DshMode::Failing, FixtureOptions::original());

    let stdout = run.assert_candidate_exited_cleanly();
    let requests = run
        .failing_dsh
        .as_ref()
        .expect("failing DSH fixture")
        .as_ref()
        .expect("failing DSH fixture completes");
    assert!(
        requests.iter().any(|request| request.path == "/api/workspace/create"),
        "the live turn did not reach the selected DSH dependency: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.selected_cookie_valid),
        "a DSH request after the failure used a non-selected credential: {requests:?}"
    );
    // The reserved-mode runner logs a failed wake turn as `[reserved2] <key> turn ended: Err(..)`
    // (`reserved2.rs`, ticket 16 8453f47d); the `回合失败` line belongs to the chat/closed-agent paths.
    assert!(
        stdout.contains(&format!("[reserved2] group:{GROUP_ID} turn ended: Err(")),
        "selected DSH failure was not reported as a failed turn"
    );
    assert!(!run.onebot_sent, "a failed selected DSH turn must not send to QQ");
    let onebot = run.onebot_http.as_ref().expect("fake OneBot HTTP completes");
    assert!(
        !onebot
            .iter()
            .any(|observation| observation.request_line.starts_with("POST /send_group_msg ")),
        "a failed selected DSH turn sent through OneBot: {onebot:?}"
    );
    assert!(run.onebot_ws.is_ok(), "fake OneBot WS: {:?}", run.onebot_ws);
    assert!(
        !run.roots.selected.join("data/sessions.json").exists(),
        "a failed workspace creation must not persist a selected session map"
    );
    assert!(
        !run.roots.selected.join("data/token-usage-rust.jsonl").exists(),
        "a turn that never reached the model must not write a ledger row"
    );

    run.assert_selected_heartbeat_and_gate();
    run.assert_no_fallback_endpoint_was_contacted();
    run.assert_ambient_profile_untouched();
}
