//! Ticket 33: the Schedule page (`/schedule`, alias `/tasks`) and its JSON routes over `schedule.rs`.
//!
//! Mounted behind the console front: Host, credential and Origin checks and the audit record of every
//! administrator write happen before [`route`] is called. Agent credentials are refused here. System jobs
//! take `confirm: true` for every action; HER's timers offer cancel and run now only (no pause).

use super::*;
use crate::schedule::{Creator, ItemChange, NewItem, Repeat, RunResult, ScheduleStore, StoreError, SystemJob, TimerKind};
use crate::tasks::{TaskId, REGISTRY};

type Reply = Result<(u16, Value), (u16, String)>;

const JSON_TYPE: &str = "application/json; charset=utf-8";
const NOT_FOUND: &str = "未找到该日程项";
const UNSUPPORTED: &str = "该项目不支持此操作";

pub(super) fn is_schedule_path(p: &str) -> bool {
    p == "/schedule" || p == "/api/schedule" || p.starts_with("/api/schedule/")
}

pub(super) fn route(context: &RuntimeContext, cfg: &Value, req: &Request) -> (u16, String, String) {
    if req.headers.contains_key("x-agent-token") || req.headers.contains_key("x-agent-call") {
        let body = json!({"ok": false, "error": "日程接口仅限管理员控制台，不接受 agent 凭据"}).to_string();
        return (403, JSON_TYPE.to_string(), body);
    }
    let method = req.method.as_str();
    let path = req.path.as_str();
    if path == "/schedule" {
        return match method {
            "GET" => (200, "text/html; charset=utf-8".to_string(), crate::console_tasks::PAGE.to_string()),
            _ => refusal(405, "method not allowed"),
        };
    }
    let outcome: Reply = if path == "/api/schedule" {
        match method {
            "GET" => list(context, cfg),
            "POST" => create(context, &req.body),
            _ => Err((405, "method not allowed".to_string())),
        }
    } else if path == "/api/schedule/history" {
        match method {
            "GET" => history(context, &req.query),
            _ => Err((405, "method not allowed".to_string())),
        }
    } else if let Some((target, action)) = path.strip_prefix("/api/schedule/").and_then(|rest| rest.rsplit_once('/')) {
        match method {
            "POST" => act(context, cfg, target, action, &req.body),
            _ => Err((405, "method not allowed".to_string())),
        }
    } else {
        Err((404, "unknown schedule route".to_string()))
    };
    match outcome {
        Ok((status, value)) => (status, JSON_TYPE.to_string(), value.to_string()),
        Err((status, message)) => refusal(status, &message),
    }
}

fn refusal(status: u16, message: &str) -> (u16, String, String) {
    (status, JSON_TYPE.to_string(), json!({"ok": false, "error": message}).to_string())
}

fn store_error(error: StoreError) -> (u16, String) {
    (error.status(), error.message())
}

fn parse_object(body: &str) -> Result<serde_json::Map<String, Value>, (u16, String)> {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(fields)) => Ok(fields),
        _ => Err((400, "请求体必须是 JSON 对象".to_string())),
    }
}

fn list(context: &RuntimeContext, cfg: &Value) -> Reply {
    let api = crate::supervisor::task_api(context, cfg).map_err(|error| (503, error))?;
    let latest = api.dispatcher.store().latest_by_task().map_err(|error| (503, error))?;
    let jobs: Vec<SystemJob> = {
        let scheduler = api.scheduler.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        REGISTRY
            .iter()
            .map(|def| {
                let last = latest.get(def.id.as_str());
                SystemJob {
                    id: def.id.as_str(),
                    effect: def.effect,
                    repeat: scheduler.schedule(def.id).unwrap_or(def.schedule).describe(),
                    next_at: scheduler.next_run(def.id),
                    enabled_by_config: crate::supervisor::enabled(cfg, def),
                    running: last.is_some_and(|run| !run.state.is_terminal()),
                    last_result: last.map(|run| {
                        let state = serde_json::to_value(run.state).ok().and_then(|v| v.as_str().map(str::to_string));
                        let summary = match (state, &run.reason) {
                            (Some(state), Some(reason)) => format!("{state} {reason}"),
                            (Some(state), None) => state,
                            (None, _) => String::new(),
                        };
                        RunResult { at: run.ended_at.or(run.started_at).unwrap_or(run.queued_at), summary, late: false }
                    }),
                }
            })
            .collect()
    };
    let now = crate::parity_clock::now_ms();
    let entries = ScheduleStore::new(context).entries(&jobs, now).map_err(store_error)?;
    Ok((200, json!({"ok": true, "now": now, "entries": entries})))
}

fn create(context: &RuntimeContext, body: &str) -> Reply {
    let fields = parse_object(body)?;
    let conversation = fields
        .get("conversation")
        .and_then(Value::as_str)
        .and_then(canonical_v2_key)
        .ok_or((400, "会话标识无效".to_string()))?;
    let text = fields.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
    let at = fields.get("at").and_then(Value::as_i64).ok_or((400, "提醒时间无效".to_string()))?;
    let repeat = match fields.get("repeat") {
        None | Some(Value::Null) => Repeat::Once,
        Some(value) => serde_json::from_value::<Repeat>(value.clone()).map_err(|error| (400, format!("重复设置无效：{error}")))?,
    };
    let new = NewItem { creator: Creator::Owner, conversation, text, at, repeat };
    let item = ScheduleStore::new(context).create(new, crate::parity_clock::now_ms()).map_err(store_error)?;
    Ok((200, json!({"ok": true, "item": item})))
}

fn history(context: &RuntimeContext, query: &HashMap<String, String>) -> Reply {
    let limit = query.get("limit").and_then(|value| value.parse::<usize>().ok()).unwrap_or(100).min(500);
    let records = ScheduleStore::new(context).history(limit).map_err(store_error)?;
    Ok((200, json!({"ok": true, "history": records})))
}

fn act(context: &RuntimeContext, cfg: &Value, target: &str, action: &str, body: &str) -> Reply {
    let well_formed = !target.is_empty() && target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'));
    let Some((kind, rest)) = target.split_once(':').filter(|_| well_formed) else {
        return Err((404, NOT_FOUND.to_string()));
    };
    let fields = if body.trim().is_empty() { serde_json::Map::new() } else { parse_object(body)? };
    match kind {
        "system" => system_action(context, cfg, rest, action, &fields, body),
        "item" => item_action(context, rest, action, &fields),
        "replyCheck" | "reachOut" | "sleep" => timer_action(context, cfg, kind, rest, action, &fields),
        _ => Err((404, NOT_FOUND.to_string())),
    }
}

fn system_action(
    context: &RuntimeContext,
    cfg: &Value,
    job: &str,
    action: &str,
    fields: &serde_json::Map<String, Value>,
    body: &str,
) -> Reply {
    let id = TaskId::parse(job).ok_or((404, NOT_FOUND.to_string()))?;
    if fields.get("confirm") != Some(&json!(true)) {
        return Err((400, "系统任务操作需要 confirm: true".to_string()));
    }
    match action {
        "pause" | "resume" => {
            ScheduleStore::new(context).set_system_paused(id.as_str(), action == "pause").map_err(store_error)?;
            Ok((200, json!({"ok": true, "id": format!("system:{}", id.as_str())})))
        }
        "run" => {
            let api = crate::supervisor::task_api(context, cfg).map_err(|error| (503, error))?;
            let response = crate::console_tasks::trigger(&api, id.as_str(), body).ok_or((404, NOT_FOUND.to_string()))?;
            let value = serde_json::from_str::<Value>(&response.body).unwrap_or_else(|_| json!({"ok": false}));
            Ok((response.status, value))
        }
        "edit" | "cancel" => Err((409, "系统任务不能编辑或取消；可以暂停，或在对应设置中修改".to_string())),
        _ => Err((409, UNSUPPORTED.to_string())),
    }
}

fn optional<T>(fields: &serde_json::Map<String, Value>, key: &str, parse: impl FnOnce(&Value) -> Option<T>, message: &str) -> Result<Option<T>, (u16, String)> {
    match fields.get(key) {
        None => Ok(None),
        Some(value) => parse(value).map(Some).ok_or((400, message.to_string())),
    }
}

fn item_action(context: &RuntimeContext, id: &str, action: &str, fields: &serde_json::Map<String, Value>) -> Reply {
    let store = ScheduleStore::new(context);
    let now = crate::parity_clock::now_ms();
    let item = match action {
        "pause" => store.set_item_paused(id, true),
        "resume" => store.set_item_paused(id, false),
        "cancel" => store.cancel(id, now),
        "run" => store.run_now(id),
        "edit" => {
            let change = ItemChange {
                text: optional(fields, "text", |v| v.as_str().map(str::to_string), "提醒内容必须是文字")?,
                at: optional(fields, "at", Value::as_i64, "提醒时间无效")?,
                repeat: optional(fields, "repeat", |v| serde_json::from_value::<Repeat>(v.clone()).ok(), "重复设置无效")?,
            };
            store.update_item(id, change, now)
        }
        _ => return Err((409, UNSUPPORTED.to_string())),
    }
    .map_err(store_error)?;
    Ok((200, json!({"ok": true, "id": format!("item:{id}"), "item": item})))
}

fn timer_action(
    context: &RuntimeContext,
    cfg: &Value,
    kind: &str,
    key: &str,
    action: &str,
    fields: &serde_json::Map<String, Value>,
) -> Reply {
    let now = crate::parity_clock::now_ms();
    let id = format!("{kind}:{key}");
    match (kind, action) {
        ("replyCheck", "run") => wake(context, cfg, key, "replyCheck"),
        ("reachOut", "run") => wake(context, cfg, key, "proactiveCheck"),
        ("sleep", "run") => wake(context, cfg, key, "timeout"),
        ("replyCheck", "cancel") | ("reachOut", "cancel") => {
            let timer = if kind == "replyCheck" { TimerKind::ReplyCheck } else { TimerKind::ReachOut };
            ScheduleStore::new(context).request_timer_cancel(key, timer, now).map_err(store_error)?;
            Ok((200, json!({"ok": true, "id": id, "status": "cancelRequested"})))
        }
        ("sleep", "cancel") => set_sleep(context, key, None),
        ("sleep", "edit") => {
            let at = fields.get("at").and_then(Value::as_i64).ok_or((400, "提醒时间无效".to_string()))?;
            if at <= now {
                return Err((400, "睡眠结束时间必须在现在之后".to_string()));
            }
            set_sleep(context, key, Some(at))
        }
        ("sleep", "pause") | ("sleep", "resume") | ("replyCheck", "pause") | ("replyCheck", "resume") | ("reachOut", "pause") | ("reachOut", "resume") => {
            Err((409, "HER 的内部计时器不能暂停；可以取消或立即运行".to_string()))
        }
        _ => Err((409, UNSUPPORTED.to_string())),
    }
}

fn wake(context: &RuntimeContext, cfg: &Value, key: &str, reason: &str) -> Reply {
    let body = json!({"key": key, "reason": reason}).to_string();
    access::social_v2_wake(context, cfg, &body).map(|value| (200, value))
}

fn set_sleep(context: &RuntimeContext, key: &str, until: Option<i64>) -> Reply {
    mutate_social_state_for(context, |state| {
        let conversation_wake = state
            .get_mut("conversations")
            .and_then(|c| c.get_mut(key))
            .and_then(|c| c.get_mut("wakeConfig"))
            .filter(|w| w.is_object())
            .ok_or((404, "该会话没有唤醒配置".to_string()))?;
        if conversation_wake.get("infinite").and_then(Value::as_bool) == Some(true) || !conversation_wake.get("sleepUntil").is_some_and(Value::is_string) {
            return Err((409, "该会话没有有限潜水".to_string()));
        }
        match until {
            Some(at) => conversation_wake["sleepUntil"] = json!(crate::sticker::iso_at(at)),
            None => {
                conversation_wake["infinite"] = json!(true);
                conversation_wake["sleepUntil"] = Value::Null;
            }
        }
        Ok(())
    })?;
    Ok((200, json!({"ok": true, "id": format!("sleep:{key}")})))
}
