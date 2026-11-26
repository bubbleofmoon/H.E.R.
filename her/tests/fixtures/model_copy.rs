//! One synthetic QQ packet for the DSH, CLI and harness delivery boundaries.
pub const MATCH: &str = "不要";
pub const CHAT: &str = "不要消息 [引用 不要人：不要原话] [图片] [CQ:at,qq=123]";
pub const SENDER: &str = "不要昵称";
pub const REDACTED: &str = "【已隐藏】消息 [引用 【已隐藏】人：【已隐藏】原话] [图片] [CQ:at,qq=123]";
pub const ID: &str = "82";

pub fn assert_snapshot(prompt: &str) {
    let packet = prompt.split_once("【本次消息快照】\n").unwrap().1.lines().next().unwrap();
    let packet: serde_json::Value = serde_json::from_str(packet).unwrap();
    let message = ["messages", "recent"].into_iter().flat_map(|field| packet[field].as_array().unwrap())
        .find(|message| message["messageId"] == ID).expect("fixture packet identity");
    assert_eq!(message["text"], REDACTED, "chat packet must mark each redacted span");
    assert_eq!(message["sender"], "【已隐藏】昵称");
}
