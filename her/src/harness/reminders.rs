//! Conversation-bound Schedule tools. The caller keeps the configuration gate through dispatch.

use crate::runtime::RuntimeContext;
use crate::schedule::{optional_time, Creator, ItemChange, NewItem, ScheduleStore, StoreError};
use serde_json::{json, Value};

pub(super) const CREATE: &str = "reminder_create";
pub(super) const LIST: &str = "reminder_list";
pub(super) const CHANGE: &str = "reminder_change";
pub(super) const CANCEL: &str = "reminder_cancel";
pub(super) const NAMES: &[&str] = &[CREATE, LIST, CHANGE, CANCEL];

const PAGE_SIZE: usize = 5;

pub(super) fn definitions() -> [Value; 4] {
    let id = json!({"type":"string", "minLength":1, "description":"编号来自 reminder_list 或 reminder_create。"});
    let fields = json!({
        "text": {"type":"string", "description":"提醒内容，最多 500 字。"},
        "at": {"type":"integer", "description":"UTC Unix 毫秒时间戳，与 localAt 二选一。"},
        "localAt": {"type":"string", "description":"使用当前时区的 YYYY-MM-DDTHH:MM，与 at 二选一。"},
        "repeat": {
            "type":"object", "description":"once 为单次，every 使用 minutes，daily 使用本地 hour 和 minute。",
            "properties": {
                "kind": {"type":"string", "enum":["once","every","daily"]},
                "minutes": {"type":"integer", "minimum":1, "maximum":u32::MAX},
                "hour": {"type":"integer", "minimum":0, "maximum":23},
                "minute": {"type":"integer", "minimum":0, "maximum":59}
            },
            "required":["kind"]
        }
    });
    let mut change = fields.clone();
    change["id"] = id.clone();
    let def = |name, description, properties, required| {
        json!({
            "name": name, "description": description,
            "inputSchema": {"type":"object", "properties":properties, "required":required}
        })
    };
    [
        def(CREATE, "为当前会话创建提醒。当前仅支持私聊，群聊创建需要管理员批准。必须提供 at 或 localAt，省略 repeat 时只提醒一次。遇到拒绝时，用自己的语气说明原因。", fields, json!(["text"])),
        def(LIST, "列出自己在当前会话中有效或已暂停的提醒。用 nextOffset 继续查看。", json!({"offset":{"type":"integer","minimum":0}}), json!([])),
        def(CHANGE, "修改当前会话中自己的提醒。当前仅支持私聊，群聊修改需要管理员批准。只提供要修改的字段。遇到拒绝时，用自己的语气说明原因。", change, json!(["id"])),
        def(CANCEL, "取消当前会话中自己的提醒。", json!({"id":id}), json!(["id"])),
    ]
}

pub(super) fn dispatch(
    context: &RuntimeContext,
    cfg: &Value,
    conversation: &str,
    name: &str,
    args: &Value,
) -> Value {
    match run(context, cfg, conversation, name, args) {
        Ok(payload) => super::tools::text(&payload.to_string(), false),
        Err(StoreError::Unavailable(_)) => {
            super::tools::error("暂时无法读取或保存提醒，请稍后再试")
        }
        Err(error) => super::tools::error(&error.message()),
    }
}

fn run(
    context: &RuntimeContext,
    cfg: &Value,
    conversation: &str,
    name: &str,
    args: &Value,
) -> Result<Value, StoreError> {
    if conversation.starts_with("group:") && matches!(name, CREATE | CHANGE) {
        return Err(StoreError::Conflict(
            "群聊提醒需要管理员批准，目前无法创建或修改".into(),
        ));
    }
    let store = ScheduleStore::new(context);
    let now = crate::parity_clock::now_ms();
    let id = || {
        args["id"]
            .as_str()
            .ok_or_else(|| StoreError::Invalid("提醒编号无效".into()))
    };
    let item = match name {
        LIST => {
            let offset = match args.get("offset") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .and_then(|offset| usize::try_from(offset).ok())
                    .ok_or_else(|| StoreError::Invalid("列表起点无效".into()))?,
            };
            let items = store.list_her(conversation)?;
            let end = offset.saturating_add(PAGE_SIZE).min(items.len());
            let page = items.get(offset..end).unwrap_or_default();
            let next = (end < items.len()).then_some(end);
            return Ok(json!({"items":page, "nextOffset":next}));
        }
        CANCEL => store.cancel_her(conversation, id()?, now)?,
        CREATE | CHANGE => {
            let fields = args
                .as_object()
                .ok_or_else(|| StoreError::Invalid("提醒参数无效".into()))?;
            let at = optional_time(fields, cfg)?;
            let text = fields
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let repeat = fields
                .get("repeat")
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .map_err(|_| StoreError::Invalid("重复设置无效".into()))
                })
                .transpose()?;
            if name == CREATE {
                let at = at.ok_or_else(|| StoreError::Invalid("请提供提醒时间".into()))?;
                let text = text.ok_or_else(|| StoreError::Invalid("请提供提醒内容".into()))?;
                let repeat = repeat.unwrap_or(crate::schedule::Repeat::Once);
                store.create(
                    NewItem {
                        creator: Creator::Her,
                        conversation: conversation.into(),
                        text,
                        at,
                        repeat,
                    },
                    now,
                )?
            } else {
                let change = ItemChange { text, at, repeat };
                store.change_her(conversation, id()?, change, now)?
            }
        }
        _ => return Err(StoreError::Invalid("提醒操作无效".into())),
    };
    Ok(json!({"item":item}))
}
