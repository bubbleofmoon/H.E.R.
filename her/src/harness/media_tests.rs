//! Inbound media on harness turns (#164): the parts `media::model_parts` acquired become canonical
//! user parts, delivery filters their text like any user text, a turn on a newly opened harness (as
//! after a restart) replays them unchanged, and a route without image input gets a placeholder.
use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Opaque base64 payloads: the harness never decodes inbound image data.
const PNG: &str = "iVBORw0KGgo=";
const WEBP: &str = "UklGRg==";

/// The parts `media::model_parts` renders for an image, a sticker and a face whose picture could
/// not be fetched.
fn inbound_media() -> Vec<Value> {
    vec![
        json!({"type":"text","text":"[图片1]"}),
        json!({"type":"image","mediaType":"image/png","data":PNG,"name":"qq-image-1.png"}),
        json!({"type":"text","text":"[图片2]"}),
        json!({"type":"image","mediaType":"image/webp","data":WEBP,"name":"qq-image-2.webp"}),
        json!({"type":"text","text":"[表情3][表情:微笑]"}),
    ]
}

fn image_url(mime: &str, data: &str) -> Value {
    json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}})
}

/// A selected runtime whose model dependency is a loopback listener. Its config strips `WORD164`
/// from model text.
struct Runtime {
    root: std::path::PathBuf,
    context: RuntimeContext,
    model: Arc<TcpListener>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn runtime(tag: &str, harness: Value) -> Runtime {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-164-{tag}-{}", crate::turn_ledger::mint_id(""))),
    );
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let model = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("http://{}", model.local_addr().unwrap());
    let cfg = json!({
        "consoleToken":"synthetic-console-token-164", "harness":harness,
        "socialV2":{"context":{"stripFromModelText":["WORD164"]}}
    });
    std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
    std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-164").unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root).unwrap();
    Runtime { root, context, model }
}

/// Answer one model request with the streamed `reply` and return the request body.
async fn answer(listener: Arc<TcpListener>, reply: String) -> Value {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("the model received no request")
        .unwrap();
    let mut bytes = Vec::new();
    let (start, length) = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "truncated model request");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..split])
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .unwrap();
            if bytes.len() >= split + 4 + length {
                break (split + 4, length);
            }
        }
    };
    let chunk = json!({"id":"fake-164","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{"role":"assistant","content":reply},"finish_reason":null}]});
    let end = json!({"id":"fake-164","object":"chat.completion.chunk","model":"fake-chat",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    socket.write_all(response.as_bytes()).await.unwrap();
    serde_json::from_slice(&bytes[start..start + length]).unwrap()
}

/// One completed turn of `group:164` on a newly opened harness, so its history comes only from
/// `data/conversations.db`, as after a restart. Returns the request body the model received.
async fn turn(runtime: &Runtime, id: &str, prompt: &str, media: Vec<Value>, reply: &str) -> Value {
    let cfg = runtime.context.load_config().unwrap();
    let model = tokio::spawn(answer(runtime.model.clone(), reply.to_string()));
    let harness = Harness::open(&runtime.context).unwrap();
    let mut turn = harness
        .admit(TurnRequest {
            id: id.into(),
            conversation: "group:164".into(),
            generation: "generation-164".into(),
            prompt: prompt.into(),
            media,
            profile: "chat".into(),
            route: ModelRoute::from_runtime(&runtime.context, &cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: Tools::empty(),
            limits: Limits::from_config(&cfg),
        })
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("the turn did not end");
    let request = model.await.unwrap();
    assert_eq!(result, Ok(reply.to_string()), "{id}");
    request
}

#[tokio::test(flavor = "current_thread")]
async fn ticket164_inbound_image_sticker_and_voice_label_reach_the_model_and_replay_identically_after_a_restart() {
    let runtime = runtime("replay", json!({"model":"fake-chat"})).await;
    let first = turn(&runtime, "media-first", "看看这个 WORD164 [语音]", inbound_media(), "第一句回复").await;
    let sent = first["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        sent,
        json!({"role":"user","content":[
            {"type":"text","text":"看看这个 【已隐藏】 [语音]"},
            {"type":"text","text":"[图片1]"},
            image_url("image/png", PNG),
            {"type":"text","text":"[图片2]"},
            image_url("image/webp", WEBP),
            {"type":"text","text":"[表情3][表情:微笑]"}
        ]}),
        "the image, the sticker, the face label and the voice label follow the filtered prompt"
    );
    // The canonical user fact keeps the unfiltered prompt as its text and the images as binary parts.
    let db = rusqlite::Connection::open(runtime.root.join("data/conversations.db")).unwrap();
    let stored: String = db
        .query_row(
            "SELECT payload FROM events WHERE turn_id='media-first' AND kind='user'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(db);
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["text"], "看看这个 WORD164 [语音]", "the user text holds the prompt alone");
    let message: ChatMessage = serde_json::from_value(stored["message"].clone()).unwrap();
    let binaries: Vec<(String, String)> = message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Binary(Binary {
                content_type,
                source: genai::chat::BinarySource::Base64(data),
                ..
            }) => Some((content_type.clone(), data.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        binaries,
        vec![("image/png".to_string(), PNG.to_string()), ("image/webp".to_string(), WEBP.to_string())]
    );
    let second = turn(&runtime, "media-second", "第二句", Vec::new(), "第二句回复").await;
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{second}");
    assert_eq!(messages[0], sent, "replay after a restart is identical");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2], json!({"role":"user","content":"第二句"}));
}

#[tokio::test(flavor = "current_thread")]
async fn ticket164_a_route_without_image_input_gets_a_placeholder_and_the_stored_fact_keeps_the_image() {
    let runtime = runtime("no-image-input", json!({"model":"fake-chat","imageInput":false})).await;
    let media = vec![
        json!({"type":"text","text":"[图片1]"}),
        json!({"type":"image","mediaType":"image/png","data":PNG,"name":"qq-image-1.png"}),
    ];
    let first = turn(&runtime, "text-only-first", "看看这个 [语音]", media, "第一句回复").await;
    assert_eq!(
        first["messages"][0],
        json!({"role":"user","content":format!("看看这个 [语音]\n\n[图片1]\n\n{NO_IMAGE_INPUT}")}),
        "no image part reaches a route without image input"
    );
    assert!(!first.to_string().contains(PNG), "the image data never reaches it");
    let replayed = turn(&runtime, "text-only-second", "第二句", Vec::new(), "第二句回复").await;
    assert_eq!(replayed["messages"][0], first["messages"][0], "the replayed fact gets the same placeholder");
    // Image input on again: the canonical fact kept the image, so the replay carries it.
    let mut cfg = runtime.context.load_config().unwrap();
    cfg["harness"]["imageInput"] = json!(true);
    std::fs::write(runtime.root.join("config/config.json"), cfg.to_string()).unwrap();
    let restored = turn(&runtime, "text-only-third", "第三句", Vec::new(), "第三句回复").await;
    assert_eq!(restored["messages"][0]["content"][2], image_url("image/png", PNG), "{restored}");
}

#[test]
fn ticket164_model_messages_replace_every_image_part_only_without_image_input() {
    let call = ToolCall {
        call_id: "call-164".into(),
        fn_name: "fixture_tool_164".into(),
        fn_arguments: json!({}),
        thought_signatures: None,
    };
    let mut tool = ChatMessage::tool(ToolResponse::from_tool_call(&call, "{}"));
    tool.content
        .extend(vec![ContentPart::Binary(Binary::from_base64("image/png", PNG, None))]);
    let messages = vec![
        user_message("看看这个", &inbound_media()),
        ChatMessage::assistant(vec![call.clone()]),
        tool,
        ChatMessage::assistant("好的"),
    ];
    /// (binary parts, placeholder texts) of a delivered request.
    fn count(messages: &[ChatMessage]) -> (usize, usize) {
        let parts: Vec<&ContentPart> = messages.iter().flat_map(|m| m.content.iter()).collect();
        (
            parts.iter().filter(|p| matches!(p, ContentPart::Binary(_))).count(),
            parts
                .iter()
                .filter(|p| matches!(p, ContentPart::Text(text) if text == NO_IMAGE_INPUT))
                .count(),
        )
    }
    assert_eq!(count(&model_messages(messages.clone(), true)), (3, 0), "a route with image input gets every image");
    let delivered = model_messages(messages, false);
    assert_eq!(count(&delivered), (0, 3), "a route without image input gets one placeholder per image");
    assert_eq!(delivered.len(), 5, "the tool image keeps its own user message after the tool response");
    assert!(
        matches!(
            &delivered[3].content.parts()[..],
            [ContentPart::Text(header), ContentPart::Text(text)]
                if header == "Images from tool call call-164:" && text == NO_IMAGE_INPUT
        ),
        "the tool image's user message holds its header and the placeholder"
    );
}

#[test]
fn ticket164_user_message_keeps_text_only_turns_and_drops_unusable_media_parts() {
    assert_eq!(
        serde_json::to_value(user_message("只有字", &[])).unwrap(),
        serde_json::to_value(ChatMessage::user("只有字")).unwrap(),
        "a text-only turn keeps its single text message"
    );
    let message = user_message(
        "看看",
        &[
            json!({"type":"text","text":"[图片1]（图片获取失败）"}),
            json!({"type":"image","mediaType":"image/png","data":""}),
            json!({"type":"image","mediaType":"application/pdf","data":PNG}),
            json!({"type":"image","data":PNG}),
            json!({"type":"audio","data":PNG}),
            json!({"type":"text"}),
            json!({"type":"image","mediaType":"image/jpeg","data":PNG,"name":"qq-image-2.jpg"}),
        ],
    );
    let parts: Vec<Value> = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({"text": text}),
            ContentPart::Binary(binary) => json!({"mime": binary.content_type, "name": binary.name}),
            _ => panic!("only text and image parts are expected"),
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            json!({"text":"看看"}),
            json!({"text":"[图片1]（图片获取失败）"}),
            json!({"mime":"image/jpeg","name":"qq-image-2.jpg"}),
        ]
    );
}
