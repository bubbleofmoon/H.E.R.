//! Ticket 33: the Schedule JSON routes over `schedule.rs` (the SPA page is served at `/schedule` via `console_ui`).
//!
//! Mounted behind `console_front`: Host, credential, agent-refusal and Origin checks and the audit record of every
//! administrator write happen before [`route`] is called. Every write rechecks the console credential under
//! the configuration gate (`AdminCaller::gate`). System jobs take
//! `confirm: true` for every action they offer; HER's timers offer cancel and run now only (no pause).

use super::*;
use crate::schedule::{
    optional_time, Creator, ItemChange, NewItem, Repeat, RunResult, ScheduleStore, StoreError,
    SystemJob, TimerKind, PAUSABLE_JOBS,
};
use crate::tasks::{TaskId, REGISTRY};

type Reply = Result<(u16, Value), (u16, String)>;

const JSON_TYPE: &str = "application/json; charset=utf-8";
const NOT_FOUND: &str = "未找到该日程项";
const UNSUPPORTED: &str = "该项目不支持此操作";

pub(super) fn is_schedule_path(p: &str) -> bool {
    p == "/api/schedule" || p.starts_with("/api/schedule/")
}

pub(super) fn route(caller: front::AdminCaller<'_>, cfg: &Value, req: &Request) -> (u16, String, String) {
    let context = caller.context();
    let method = req.method.as_str();
    let path = req.path.as_str();
    let outcome: Reply = if path == "/api/schedule" {
        match method {
            "GET" => list(context, cfg),
            "POST" => create(caller, req),
            _ => Err((405, "method not allowed".to_string())),
        }
    } else if path == "/api/schedule/history" {
        match method {
            "GET" => history(context, &req.query),
            _ => Err((405, "method not allowed".to_string())),
        }
    } else if let Some((target, action)) = path.strip_prefix("/api/schedule/").and_then(|rest| rest.rsplit_once('/')) {
        match method {
            "POST" => act(caller, cfg, req, target, action),
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
    let zone = crate::local_time::configured_zone(cfg);
    let current = crate::local_time::at(now, &zone);
    Ok((200, json!({
        "ok": true, "now": now, "entries": entries,
        "timeZone": {"name": zone.iana_name(), "offsetSeconds": current.offset().seconds(),
                     "label": crate::local_time::label(&current)}
    })))
}

fn create(caller: front::AdminCaller<'_>, req: &Request) -> Reply {
    let context = caller.context();
    let fields = parse_object(&req.body)?;
    let conversation = fields
        .get("conversation")
        .and_then(Value::as_str)
        .and_then(canonical_v2_key)
        .ok_or((400, "会话标识无效".to_string()))?;
    let text = fields.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
    let _authority = caller.gate()?;
    let cfg = selected_config_for_handler(context)?;
    let at = optional_time(&fields, &cfg)
        .map_err(store_error)?
        .ok_or((400, "提醒时间无效".to_string()))?;
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

fn act(caller: front::AdminCaller<'_>, cfg: &Value, req: &Request, target: &str, action: &str) -> Reply {
    let well_formed = !target.is_empty() && target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'));
    let Some((kind, rest)) = target.split_once(':').filter(|_| well_formed) else {
        return Err((404, NOT_FOUND.to_string()));
    };
    let fields = if req.body.trim().is_empty() { serde_json::Map::new() } else { parse_object(&req.body)? };
    match kind {
        "system" => system_action(caller, cfg, req, rest, action, &fields),
        "item" => item_action(caller, rest, action, &fields),
        "replyCheck" | "reachOut" | "sleep" => timer_action(caller, cfg, kind, rest, action, &fields),
        _ => Err((404, NOT_FOUND.to_string())),
    }
}

fn system_action(
    caller: front::AdminCaller<'_>,
    cfg: &Value,
    req: &Request,
    job: &str,
    action: &str,
    fields: &serde_json::Map<String, Value>,
) -> Reply {
    let context = caller.context();
    let id = TaskId::parse(job).ok_or((404, NOT_FOUND.to_string()))?;
    // What the job offers is checked before confirmation: an action it never offers answers 409, confirmed or not.
    match action {
        "pause" | "resume" if !PAUSABLE_JOBS.contains(&id.as_str()) => {
            return Err((409, "看门狗和手动任务不能暂停".to_string()));
        }
        "pause" | "resume" | "run" => {}
        "edit" | "cancel" => return Err((409, "系统任务不能编辑或取消；可以暂停，或在对应设置中修改".to_string())),
        _ => return Err((409, UNSUPPORTED.to_string())),
    }
    if fields.get("confirm") != Some(&json!(true)) {
        return Err((400, "系统任务操作需要 confirm: true".to_string()));
    }
    if action == "run" {
        // The task trigger's admission rechecks the credential under the same gate, so this one is released first.
        drop(caller.gate()?);
        let api = crate::supervisor::task_api(context, cfg).map_err(|error| (503, error))?;
        let response = crate::console_tasks::trigger(&api, id.as_str(), &req.body).ok_or((404, NOT_FOUND.to_string()))?;
        let value = serde_json::from_str::<Value>(&response.body).unwrap_or_else(|_| json!({"ok": false}));
        return Ok((response.status, value));
    }
    let _authority = caller.gate()?;
    ScheduleStore::new(context).set_system_paused(id.as_str(), action == "pause").map_err(store_error)?;
    Ok((200, json!({"ok": true, "id": format!("system:{}", id.as_str())})))
}

fn optional<T>(fields: &serde_json::Map<String, Value>, key: &str, parse: impl FnOnce(&Value) -> Option<T>, message: &str) -> Result<Option<T>, (u16, String)> {
    match fields.get(key) {
        None => Ok(None),
        Some(value) => parse(value).map(Some).ok_or((400, message.to_string())),
    }
}

fn item_action(caller: front::AdminCaller<'_>, id: &str, action: &str, fields: &serde_json::Map<String, Value>) -> Reply {
    let context = caller.context();
    if !matches!(action, "pause" | "resume" | "cancel" | "run" | "edit") {
        return Err((409, UNSUPPORTED.to_string()));
    }
    let _authority = caller.gate()?;
    let change = if action == "edit" {
        let cfg = selected_config_for_handler(context)?;
        ItemChange {
            text: optional(fields, "text", |v| v.as_str().map(str::to_string), "提醒内容必须是文字")?,
            at: optional_time(fields, &cfg).map_err(store_error)?,
            repeat: optional(fields, "repeat", |v| serde_json::from_value::<Repeat>(v.clone()).ok(), "重复设置无效")?,
        }
    } else {
        ItemChange::default()
    };
    let store = ScheduleStore::new(context);
    let now = crate::parity_clock::now_ms();
    let item = match action {
        "pause" => store.set_item_paused(id, true),
        "resume" => store.set_item_paused(id, false),
        "cancel" => store.cancel(id, now),
        "run" => store.run_now(id),
        _ => store.update_item(id, change, now),
    }
    .map_err(store_error)?;
    Ok((200, json!({"ok": true, "id": format!("item:{id}"), "item": item})))
}

fn timer_action(
    caller: front::AdminCaller<'_>,
    cfg: &Value,
    kind: &str,
    key: &str,
    action: &str,
    fields: &serde_json::Map<String, Value>,
) -> Reply {
    let context = caller.context();
    let now = crate::parity_clock::now_ms();
    let id = format!("{kind}:{key}");
    match (kind, action) {
        ("replyCheck", "run") => wake(caller, cfg, key, "replyCheck"),
        ("reachOut", "run") => wake(caller, cfg, key, "proactiveCheck"),
        ("sleep", "run") => wake(caller, cfg, key, "timeout"),
        ("replyCheck", "cancel") | ("reachOut", "cancel") => {
            let timer = if kind == "replyCheck" { TimerKind::ReplyCheck } else { TimerKind::ReachOut };
            // The timer's time as the page showed it: only that timer is cancelled, never one armed since.
            let at = fields.get("at").and_then(Value::as_i64).ok_or((400, "取消需要页面上显示的计时器时间 at".to_string()))?;
            let _authority = caller.gate()?;
            ScheduleStore::new(context).request_timer_cancel(key, timer, at, now).map_err(store_error)?;
            Ok((200, json!({"ok": true, "id": id, "status": "cancelRequested"})))
        }
        ("sleep", "cancel") => set_sleep(caller, key, None),
        ("sleep", "edit") => set_sleep(caller, key, Some(fields)),
        ("sleep", "pause") | ("sleep", "resume") | ("replyCheck", "pause") | ("replyCheck", "resume") | ("reachOut", "pause") | ("reachOut", "resume") => {
            Err((409, "HER 的内部计时器不能暂停；可以取消或立即运行".to_string()))
        }
        _ => Err((409, UNSUPPORTED.to_string())),
    }
}

fn wake(caller: front::AdminCaller<'_>, cfg: &Value, key: &str, reason: &str) -> Reply {
    drop(caller.gate()?);
    let body = json!({"key": key, "reason": reason}).to_string();
    access::social_v2_wake(caller.context(), cfg, &body).map(|value| (200, value))
}

fn set_sleep(caller: front::AdminCaller<'_>, key: &str, fields: Option<&serde_json::Map<String, Value>>) -> Reply {
    let context = caller.context();
    drop(caller.gate()?);
    crate::conversation_state::ConversationState::for_runtime(context).update(key, |conv| {
        // Rechecked under the social-state gate: a rotation may have completed since the check above.
        let current = selected_config_for_handler(context)?;
        let until = fields.map(|fields| {
            let at = optional_time(fields, &current).map_err(store_error)?.ok_or((400, "提醒时间无效".to_string()))?;
            if at <= crate::parity_clock::now_ms() {
                return Err((400, "睡眠结束时间必须在现在之后".to_string()));
            }
            Ok(at)
        }).transpose()?;
        let mut conversation_wake = conv
            .field("wakeConfig")
            .filter(|w| w.is_object())
            .cloned()
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
        conv.set_field("wakeConfig", conversation_wake);
        Ok(())
    })?;
    Ok((200, json!({"ok": true, "id": format!("sleep:{key}")})))
}
