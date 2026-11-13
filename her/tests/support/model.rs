// Scriptable OpenAI-compatible SSE fixture. No provider SDK or external endpoint.
enum ModelReply {
    Refusal,
    StreamedRefusal,
    ErrorEvent,
    Text(String),
    ToolCalls(Vec<Value>),
    UsedTools {
        calls: Vec<Value>,
        total_tokens: i64,
    },
    HeldTools {
        calls: Vec<Value>,
        started: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
    },
    TruncatedTools(Vec<Value>),
    Truncated(String),
    Active {
        text: String,
        started: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
    },
    Delayed {
        text: String,
        release: Arc<std::sync::atomic::AtomicBool>,
    },
    Error {
        status: u16,
        message: String,
    },
}

struct FakeModel {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    authorizations: Arc<Mutex<Vec<bool>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl FakeModel {
    fn start(replies: Vec<&str>) -> Self {
        Self::script(
            replies
                .into_iter()
                .map(|r| ModelReply::Text(r.into()))
                .collect(),
        )
    }

    fn script(replies: Vec<ModelReply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let auth = authorizations.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, done) = (requests.clone(), stop.clone());
        let task = thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let Some(req) = read_model_request(&mut socket) else {
                    continue;
                };
                assert_eq!(req.request_line, "POST /v1/chat/completions HTTP/1.1");
                let authenticated =
                    req.headers
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .any(|(k, v)| {
                            k.eq_ignore_ascii_case("authorization")
                                && v.trim() == "Bearer synthetic-model-key-67"
                        });
                lock(&auth).push(authenticated);
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let n = lock(&seen).len();
                lock(&seen).push(body);
                let Some(reply) = replies.get(n) else {
                    respond(
                        &mut socket,
                        500,
                        &json!({"error":{"message":"unexpected request"}}),
                    );
                    continue;
                };
                if let ModelReply::HeldTools {
                    started, release, ..
                } = reply
                {
                    started.store(true, std::sync::atomic::Ordering::SeqCst);
                    while !release.load(std::sync::atomic::Ordering::SeqCst)
                        && !done.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                if let ModelReply::ToolCalls(calls)
                | ModelReply::TruncatedTools(calls)
                | ModelReply::HeldTools { calls, .. }
                | ModelReply::UsedTools { calls, .. } = reply
                {
                    let mut sse = String::new();
                    let mut frame = |delta: Value, finish: Value| {
                        let mut chunk = json!({"id":"fake-68","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
                        if finish == "tool_calls" {
                            if let ModelReply::UsedTools { total_tokens, .. } = reply {
                                chunk["usage"] = json!({"prompt_tokens":total_tokens-1,"completion_tokens":1,"total_tokens":total_tokens});
                            }
                        }
                        sse.push_str(&format!("data: {chunk}\n\n"));
                    };
                    frame(
                        json!({"role":"assistant","reasoning_content":"tool plan"}),
                        Value::Null,
                    );
                    for (index, call) in calls.iter().enumerate() {
                        frame(
                            json!({"tool_calls":[{"index":index,"id":call["id"],"type":"function","function":{"name":call["name"],"arguments":""}}]}),
                            Value::Null,
                        );
                    }
                    // Interleave argument fragments across calls, including split JSON strings.
                    let args: Vec<Vec<char>> = calls
                        .iter()
                        .map(|c| {
                            c["raw_arguments"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| c["arguments"].to_string())
                                .chars()
                                .collect()
                        })
                        .collect();
                    for pos in 0..args.iter().map(Vec::len).max().unwrap_or(0) {
                        for (index, arg) in args.iter().enumerate() {
                            if let Some(ch) = arg.get(pos) {
                                frame(
                                    json!({"tool_calls":[{"index":index,"function":{"arguments":ch.to_string()}}]}),
                                    Value::Null,
                                );
                            }
                        }
                    }
                    frame(json!({}), json!("tool_calls"));
                    sse.push_str("data: [DONE]\n\n");
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", sse.len());
                    if matches!(reply, ModelReply::TruncatedTools(_)) {
                        let split = sse.match_indices("\n\n").nth(2).unwrap().0 + 2;
                        let _ = socket.write_all(sse[..split].as_bytes());
                    } else {
                        let _ = socket.write_all(sse.as_bytes());
                    }
                    continue;
                }
                if matches!(reply, ModelReply::StreamedRefusal) {
                    // A refusal delta followed by a normal stop, as emitted by Chat Completions.
                    let refusal = json!({"choices":[{"index":0,"delta":{"refusal":"CHAT_SECRET refusal"},"finish_reason":null}]});
                    let stop = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                    let sse = format!("data: {refusal}\n\ndata: {stop}\n\ndata: [DONE]\n\n");
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len());
                    continue;
                }
                if matches!(reply, ModelReply::Refusal | ModelReply::ErrorEvent) {
                    let chunk=if matches!(reply, ModelReply::ErrorEvent) { json!({"error":{"code":"SAFETY","message":"CHAT_SECRET"}}) } else { json!({"id":"refusal-81","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}) };
                    let sse=format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    let _=write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",sse.len());
                    continue;
                }
                let reply = match reply {
                    ModelReply::ToolCalls(_)
                    | ModelReply::TruncatedTools(_)
                    | ModelReply::HeldTools { .. }
                    | ModelReply::UsedTools { .. } => unreachable!(),
                    ModelReply::Refusal | ModelReply::StreamedRefusal | ModelReply::ErrorEvent => unreachable!(),
                    ModelReply::Text(text) | ModelReply::Truncated(text) => text,
                    ModelReply::Active { text, .. } => text,
                    ModelReply::Delayed { text, release } => {
                        while !release.load(std::sync::atomic::Ordering::SeqCst)
                            && !done.load(std::sync::atomic::Ordering::SeqCst)
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        text
                    }
                    ModelReply::Error { status, message } => {
                        respond(
                            &mut socket,
                            *status,
                            &json!({"error":{"message":message,"type":"fixture"}}),
                        );
                        continue;
                    }
                };
                let mut sse = String::new();
                // Multiple content chunks force the real provider streaming decoder to assemble text.
                for text in reply.chars().map(|c| c.to_string()) {
                    sse.push_str(&format!("data: {}\n\n", json!({"id":"fake-67","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})));
                }
                sse.push_str(&format!("data: {}\n\n", json!({"id":"fake-67","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}})));
                sse.push_str("data: [DONE]\n\n");
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", sse.len());
                // Emit a real content delta before holding or truncating the response.
                let split = sse.find("\n\n").unwrap() + 2;
                if matches!(replies.get(n), Some(ModelReply::Truncated(_))) {
                    let _ = socket.write_all(sse[..split].as_bytes());
                    let _ = socket.flush();
                    continue; // Close before the advertised length, finish reason and [DONE].
                }
                if let Some(ModelReply::Active {
                    started, release, ..
                }) = replies.get(n)
                {
                    socket.write_all(sse[..split].as_bytes()).unwrap();
                    socket.flush().unwrap();
                    started.store(true, std::sync::atomic::Ordering::SeqCst);
                    while !release.load(std::sync::atomic::Ordering::SeqCst)
                        && !done.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    let _ = socket.write_all(sse[split..].as_bytes());
                } else {
                    let _ = socket.write_all(sse.as_bytes());
                }
            }
        });
        Self {
            base,
            requests,
            authorizations,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

fn read_model_request(socket: &mut TcpStream) -> Option<HttpReq> {
    // Windows accepts can inherit the listener's nonblocking mode.
    socket.set_nonblocking(false).ok()?;
    read_request(socket)
}
