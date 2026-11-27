//! Ticket 27 (renamed by ticket 33): the Schedule JSON task routes (`/api/tasks/*`; the SPA is served at `/schedule` via `console_ui`).
//!
//! Mounted behind the console front (`console_front`): Host check, console credential, agent refusal, `application/json` and Origin checks
//! on POST, and the audit record of every administrator write happen before [`route`] is called. This
//! module adds what is specific to tasks: [`route`] rechecks `AdminCaller::gate` before starting or changing task lifecycle; only registered task ids
//! can be triggered; a manual trigger needs a client request id (repeating it returns the first run, a running task
//! answers 409); a trigger answers `202 queued` and the page follows the run until its real result. Logs are served
//! redacted (store redaction plus long digit runs masked); chat-bearing tasks show only metadata.
#![allow(dead_code)]

use crate::lifecycle::{effective, DesiredRead, DesiredState, DesiredStore, Effective};
use crate::task_runs::{LogStream, RunQuery, RunView};
use crate::tasks::{task_def, Disposition, Dispatcher, Scheduler, TaskId, Trigger, LEGACY_TASKS, REGISTRY};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct TaskRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a HashMap<String, String>,
    pub body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub struct TasksApi {
    pub dispatcher: Arc<Dispatcher>,
    pub scheduler: Arc<Mutex<Scheduler>>,
    pub desired: DesiredStore,
}

fn reply(status: u16, body: Value) -> Option<TaskResponse> {
    Some(TaskResponse { status, content_type: JSON_TYPE, body: body.to_string() })
}

fn error(status: u16, message: &str) -> Option<TaskResponse> {
    reply(status, json!({ "ok": false, "error": message }))
}

/// `None`: not a task route (the console continues with its own table).
pub fn route(caller: crate::console_front::AdminCaller<'_>, api: &TasksApi, req: &TaskRequest) -> Option<TaskResponse> {
    let path = req.path;
    let is_task_route = path == "/api/tasks" || path.starts_with("/api/tasks/");
    if !is_task_route {
        return None;
    }
    if path == "/api/tasks" {
        return match req.method {
            "GET" => list_tasks(api),
            _ => error(405, "method not allowed"),
        };
    }
    let rest = &path["/api/tasks/".len()..];
    if rest == "lifecycle" {
        return match req.method {
            "POST" => {
                let _authority = match caller.gate() {
                    Ok(gate) => gate,
                    Err((status, message)) => return error(status, &message),
                };
                set_lifecycle(api, req.body)
            }
            _ => error(405, "method not allowed"),
        };
    }
    if rest == "runs" {
        return match req.method {
            "GET" => list_runs(api, req.query),
            _ => error(405, "method not allowed"),
        };
    }
    if let Some(run) = rest.strip_prefix("runs/") {
        let (run_id, log) = match run.strip_suffix("/log") {
            Some(id) => (id, true),
            None => (run, false),
        };
        if req.method != "GET" {
            return error(405, "method not allowed");
        }
        return if log { run_log(api, run_id, req.query) } else { run_view(api, run_id) };
    }
    if let Some(task) = rest.strip_suffix("/run") {
        if req.method != "POST" {
            return error(405, "method not allowed");
        }
        if let Err((status, message)) = caller.gate() {
            return error(status, &message);
        }
        return trigger(api, task, req.body);
    }
    error(404, "unknown task route")
}

fn set_lifecycle(api: &TasksApi, body: &str) -> Option<TaskResponse> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else { return error(400, "body must be a JSON object") };
    if fields.get("confirm").and_then(Value::as_bool) != Some(true) {
        return error(400, "explicit confirmation is required");
    }
    let Some(state) = fields.get("state").and_then(Value::as_str).and_then(DesiredState::parse) else {
        return error(400, "state must be running, stopped, or maintenance");
    };
    let now = api.dispatcher.clock().now_ms();
    let until = match fields.get("minutes") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64().filter(|n| *n > 0 && *n <= 525_600) {
            Some(minutes) => Some(now + minutes as i64 * 60_000),
            None => return error(400, "minutes must be between 1 and 525600"),
        },
    };
    if state != DesiredState::Maintenance && until.is_some() {
        return error(400, "only maintenance can have an end time");
    }
    let reason = fields.get("reason").and_then(Value::as_str).unwrap_or("");
    if state != DesiredState::Running {
        use crate::lifecycle::{
            verify_owner, ImagePolicy, LiveLifecycle, OwnerObservation, OwnerRole, ProbeMode,
        };
        let desired_path = api.desired.path();
        let data = desired_path.parent()?.parent()?;
        let managed = matches!(
            verify_owner(
                &mut LiveLifecycle,
                data,
                OwnerRole::Supervisor,
                &ImagePolicy::SameReleaseTwin,
                ProbeMode::Existing,
            ),
            OwnerObservation::Verified(_)
        );
        if !managed {
            return reply(503, json!({"ok":false,"applied":false,"error":"supervisor is unavailable; lifecycle change was not applied; use the local lifecycle CLI"}));
        }
    }
    match api.desired.write(state, "console", reason, until, now) {
        Ok(record) if state != DesiredState::Running => reply(202, json!({"ok":true,"applied":false,"state":"pending","record":record})),
        Ok(record) => reply(200, json!({"ok":true,"record":record})),
        Err(message) => error(503, &message),
    }
}

fn desired_json(read: &DesiredRead, now: i64) -> Value {
    let record = match read {
        DesiredRead::Present(record) => serde_json::to_value(record).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    match effective(read, now) {
        Effective::Act { state, wants } => json!({ "state": state.as_str(), "wants": wants, "record": record }),
        Effective::Unknown(reason) => json!({ "state": "unknown", "reason": reason }),
    }
}

fn list_tasks(api: &TasksApi) -> Option<TaskResponse> {
    let now = api.dispatcher.clock().now_ms();
    let latest = match api.dispatcher.store().latest_by_task() {
        Ok(latest) => latest,
        Err(e) => return error(503, &e),
    };
    let scheduler = api.scheduler.lock().unwrap_or_else(|p| p.into_inner());
    let tasks: Vec<Value> = REGISTRY
        .iter()
        .map(|def| {
            let last = latest.get(def.id.as_str());
            json!({
                "id": def.id.as_str(),
                "title": def.title,
                "effect": def.effect,
                "schedule": scheduler.schedule(def.id).unwrap_or(def.schedule).describe(),
                "enabled": scheduler.enabled(def.id),
                "nextRunAt": scheduler.next_run(def.id),
                "replaces": def.replaces,
                "running": last.is_some_and(|r| !r.state.is_terminal()),
                "lastRun": last,
            })
        })
        .collect();
    drop(scheduler);
    let legacy: Vec<Value> = LEGACY_TASKS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "disposition": match t.disposition { Disposition::Migrated => "migrated", Disposition::Retired => "retired" },
                "replacement": t.replacement,
                "note": t.note,
            })
        })
        .collect();
    reply(200, json!({ "ok": true, "now": now, "desired": desired_json(&api.desired.read(), now), "tasks": tasks, "legacy": legacy }))
}

fn parse_u64(query: &HashMap<String, String>, key: &str) -> Result<Option<u64>, ()> {
    match query.get(key).map(String::as_str) {
        None | Some("") => Ok(None),
        Some(text) => text.parse().map(Some).map_err(|_| ()),
    }
}

fn list_runs(api: &TasksApi, query: &HashMap<String, String>) -> Option<TaskResponse> {
    let task = match query.get("task").map(String::as_str) {
        None | Some("") => None,
        Some(id) => match TaskId::parse(id) {
            Some(id) => Some(id.as_str().to_string()),
            None => return error(404, "unknown task"),
        },
    };
    let (Ok(before), Ok(limit)) = (parse_u64(query, "before"), parse_u64(query, "limit")) else {
        return error(400, "before and limit must be non-negative integers");
    };
    match api.dispatcher.store().list(&RunQuery { task, before, limit: limit.map(|l| l as usize) }) {
        Ok(page) => reply(200, json!({ "ok": true, "runs": page.runs, "nextBefore": page.next_before })),
        Err(e) => error(503, &e),
    }
}

fn find_run(api: &TasksApi, run_id: &str) -> Result<Option<RunView>, Option<TaskResponse>> {
    api.dispatcher.store().get(run_id).map_err(|e| error(503, &e))
}

fn run_view(api: &TasksApi, run_id: &str) -> Option<TaskResponse> {
    match find_run(api, run_id) {
        Ok(Some(run)) => reply(200, json!({ "ok": true, "run": run })),
        Ok(None) => error(404, "unknown run"),
        Err(response) => response,
    }
}

/// Long digit runs (QQ numbers, ids) are masked in served log text.
fn mask_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut String| {
        if digits.len() >= 6 {
            out.push_str("<num>");
        } else {
            out.push_str(digits);
        }
        digits.clear();
    };
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut digits, &mut out);
            out.push(c);
        }
    }
    flush(&mut digits, &mut out);
    out
}

fn run_log(api: &TasksApi, run_id: &str, query: &HashMap<String, String>) -> Option<TaskResponse> {
    let run = match find_run(api, run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return error(404, "unknown run"),
        Err(response) => return response,
    };
    let (Ok(after), Ok(limit)) = (parse_u64(query, "after"), parse_u64(query, "limit")) else {
        return error(400, "after and limit must be non-negative integers");
    };
    let chat_output = TaskId::parse(&run.task_id).map(|id| task_def(id).chat_output).unwrap_or(true);
    let page = match api.dispatcher.store().read_log(run_id, after, limit.map(|l| l as usize)) {
        Ok(Some(page)) => page,
        Ok(None) => return error(404, "unknown run"),
        Err(e) => return error(503, &e),
    };
    let lines: Vec<Value> = page
        .lines
        .iter()
        .filter(|line| !chat_output || line.s == LogStream::Meta)
        .map(|line| json!({ "n": line.n, "stream": line.s, "text": mask_numbers(&line.t) }))
        .collect();
    reply(
        200,
        json!({
            "ok": true,
            "runId": run.run_id,
            "state": run.state,
            "lines": lines,
            "nextAfter": page.next_after,
            "stdoutHidden": chat_output,
        }),
    )
}

pub(crate) fn trigger(api: &TasksApi, task: &str, body: &str) -> Option<TaskResponse> {
    let Some(id) = TaskId::parse(task) else { return error(404, "unknown task") };
    if id == TaskId::ConsoleRestart { return error(409, "restart must be submitted through /api/restart"); }
    if !task_def(id).runs_when_held {
        match effective(&api.desired.read(), api.dispatcher.clock().now_ms()) {
            Effective::Act { wants, .. } if wants.jobs => {}
            Effective::Act { .. } => return error(409, "background jobs are held by the desired state"),
            Effective::Unknown(_) => return error(409, "desired state is unknown"),
        }
    }
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return error(400, "body must be a JSON object");
    };
    let Some(request_id) = fields.get("requestId").and_then(Value::as_str) else {
        return error(400, "requestId is required");
    };
    if !crate::task_runs::valid_request_id(request_id) {
        return error(400, "requestId must be 8-64 letters, digits, '-' or '_'");
    }
    match api.dispatcher.manual(id, request_id, "console") {
        Trigger::Started(run) => reply(202, json!({ "ok": true, "runId": run.run_id, "state": run.state, "run": run })),
        Trigger::Duplicate(run) => {
            reply(200, json!({ "ok": true, "duplicate": true, "runId": run.run_id, "state": run.state, "run": run }))
        }
        Trigger::AlreadyRunning(run) => reply(
            409,
            json!({ "ok": false, "error": "任务正在运行，未重复启动", "runId": run.map(|r| r.run_id) }),
        ),
        Trigger::Held(reason) | Trigger::Refused(reason) => error(409, &reason),
        Trigger::Unavailable(reason) => error(503, &reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DesiredState;
    use crate::task_runs::{credential_redactor, Finish, RunHandle, RunState, StoreLimits, TaskStore};
    use crate::dependency::ManualClock;
    use crate::tasks::{Clock, Executor, TaskDef};
    use std::sync::atomic::AtomicBool;
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    /// Holds every run until released, then logs a fixed output and finishes as told.
    struct Held {
        open: Mutex<bool>,
        cv: Condvar,
    }

    struct HeldExecutor(Arc<Held>);

    impl Executor for HeldExecutor {
        fn execute(&self, def: &TaskDef, run: &mut RunHandle, _: &AtomicBool) -> Finish {
            run.mark_running(Some(4321), 0).unwrap();
            let mut open = self.0.open.lock().unwrap();
            while !*open {
                open = self.0.cv.wait(open).unwrap();
            }
            run.log(LogStream::Out, "chat line from group 123456789: hello");
            run.log(LogStream::Err, "failed with token=abcdef123 for qq 987654321");
            if def.id == TaskId::Backup {
                Finish::Failed { exit_code: Some(2), reason: "exit code 2".into() }
            } else {
                Finish::Succeeded
            }
        }
    }

    struct Fixture {
        api: TasksApi,
        held: Arc<Held>,
        dir: std::path::PathBuf,
        context: crate::runtime::RuntimeContext,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-console-tasks-{name}-{}-{}", std::process::id(), crate::parity_clock::now_ms())));
            for sub in ["config", "data", "resources", "user/dsh", "tasks", "lifecycle"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            std::fs::write(dir.join("config/config.json"), "{}").unwrap();
            std::fs::write(
                dir.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
            )
            .unwrap();
            let context = crate::runtime::RuntimeContext::from_root(&dir).unwrap();
            let clock = Arc::new(ManualClock::new(1_793_289_600_000));
            let store = Arc::new(TaskStore::open(dir.join("tasks"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
            let held = Arc::new(Held { open: Mutex::new(false), cv: Condvar::new() });
            let dispatcher = Dispatcher::new(store, Arc::new(HeldExecutor(held.clone())), clock.clone());
            let scheduler = Arc::new(Mutex::new(Scheduler::new(|d| d.enabled_by_default, crate::local_time::beijing_tariff_zone(), clock.now_ms())));
            let api = TasksApi { dispatcher, scheduler, desired: DesiredStore::new(dir.join("lifecycle")) };
            Self { api, held, dir, context }
        }

        fn release(&self) {
            *self.held.open.lock().unwrap() = true;
            self.held.cv.notify_all();
        }

        fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: &str) -> (u16, Value) {
            let query: HashMap<String, String> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let response = route(crate::console_front::AdminCaller::new(&self.context), &self.api, &TaskRequest { method, path, query: &query, body }).expect("a task route");
            assert_eq!(response.content_type, JSON_TYPE);
            (response.status, serde_json::from_str(&response.body).unwrap())
        }

        fn wait_state(&self, run_id: &str, terminal: bool) -> Value {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let (status, body) = self.call("GET", &format!("/api/tasks/runs/{run_id}"), &[], "");
                assert_eq!(status, 200, "{body}");
                let state = body["run"]["state"].as_str().unwrap().to_string();
                let done = !matches!(state.as_str(), "queued" | "running");
                if done == terminal && (terminal || state == "running") {
                    return body["run"].clone();
                }
                assert!(Instant::now() < deadline, "{body}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.release();
            std::thread::sleep(Duration::from_millis(50));
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn non_task_paths_are_left_to_the_console() {
        let f = Fixture::new("passthrough");
        let query = HashMap::new();
        for path in ["/api/status", "/api/tasksx", "/task", "/"] {
            assert!(route(crate::console_front::AdminCaller::new(&f.context), &f.api, &TaskRequest { method: "GET", path, query: &query, body: "" }).is_none(), "{path}");
        }
    }

    #[test]
    fn tasks_list_schedule_next_run_desired_state_and_legacy_dispositions() {
        let f = Fixture::new("list");
        let (status, body) = f.call("GET", "/api/tasks", &[], "");
        assert_eq!(status, 200);
        let tasks = body["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), REGISTRY.len());
        let backup = tasks.iter().find(|t| t["id"] == "backup").unwrap();
        assert_eq!(backup["schedule"], "每天 04:00");
        assert_eq!(backup["enabled"], true);
        assert_eq!(backup["nextRunAt"], 1_793_289_600_000i64 + 4 * 3_600_000);
        assert_eq!(backup["replaces"], "QQBot-Backup");
        assert!(!tasks.iter().any(|t| t["id"] == "ab-report"));
        assert_eq!(body["desired"]["state"], "running");
        let legacy = body["legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 10);
        let shadow = legacy.iter().find(|t| t["name"] == "QQBot-RBridgeShadow").unwrap();
        assert_eq!(shadow["disposition"], "retired");

        f.api.desired.write(DesiredState::Stopped, "cli", "rollback", None, 1).unwrap();
        let (_, body) = f.call("GET", "/api/tasks", &[], "");
        assert_eq!(body["desired"]["state"], "stopped");
        assert_eq!(body["desired"]["record"]["reason"], "rollback");
        std::fs::write(f.api.desired.path(), "{").unwrap();
        let (_, body) = f.call("GET", "/api/tasks", &[], "");
        assert_eq!(body["desired"]["state"], "unknown");
    }

    #[test]
    fn manual_trigger_is_deduplicated_and_reports_the_real_result() {
        let f = Fixture::new("trigger");
        let (status, started) = f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"req-click-0001"}"#);
        assert_eq!(status, 202, "{started}");
        assert_eq!(started["state"], "queued", "accepted, not reported as done");
        let run_id = started["runId"].as_str().unwrap().to_string();
        f.wait_state(&run_id, false);

        // double click: same request id, same run
        let (status, again) = f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"req-click-0001"}"#);
        assert_eq!((status, again["duplicate"].clone(), again["runId"].as_str()), (200, json!(true), Some(run_id.as_str())));
        // another click while it runs: refused, pointing at the running run
        let (status, busy) = f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"req-click-0002"}"#);
        assert_eq!((status, busy["runId"].as_str()), (409, Some(run_id.as_str())));
        let (_, list) = f.call("GET", "/api/tasks", &[], "");
        assert_eq!(list["tasks"].as_array().unwrap().iter().find(|t| t["id"] == "backup").unwrap()["running"], true);

        f.release();
        let run = f.wait_state(&run_id, true);
        assert_eq!((run["state"].clone(), run["exitCode"].clone(), run["source"].clone()), (json!("failed"), json!(2), json!("manual")));
        assert_eq!(run["reason"], "exit code 2");
        assert_eq!(run["by"], "console");
        assert!(run["startedAt"].is_number() && run["endedAt"].is_number());
        let (_, runs) = f.call("GET", "/api/tasks/runs", &[("task", "backup")], "");
        assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn trigger_input_is_validated_and_only_registered_tasks_run() {
        let f = Fixture::new("validate");
        assert_eq!(f.call("POST", "/api/tasks/rm%20-rf/run", &[], r#"{"requestId":"req-valid-001"}"#).0, 404);
        assert_eq!(f.call("POST", "/api/tasks/../backup/run", &[], r#"{"requestId":"req-valid-001"}"#).0, 404);
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], "not json").0, 400);
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], "[]").0, 400);
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], "{}").0, 400);
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"x"}"#).0, 400);
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"has space here"}"#).0, 400);
        assert_eq!(f.call("GET", "/api/tasks/backup/run", &[], "").0, 405);
        assert_eq!(f.call("POST", "/api/tasks", &[], "{}").0, 405);
        assert_eq!(f.call("GET", "/api/tasks/nothing", &[], "").0, 404);
        assert_eq!(f.call("GET", "/api/tasks/runs", &[("task", "nope")], "").0, 404);
        assert_eq!(f.call("GET", "/api/tasks/runs", &[("limit", "-1")], "").0, 400);
        assert!(f.api.dispatcher.store().list(&RunQuery::default()).unwrap().runs.is_empty());
        // the same request id cannot start a second task
        assert_eq!(f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"req-shared-01"}"#).0, 202);
        assert_eq!(f.call("POST", "/api/tasks/memory-digest/run", &[], r#"{"requestId":"req-shared-01"}"#).0, 409);
    }

    #[test]
    fn logs_are_redacted_paged_and_hide_chat_output() {
        let f = Fixture::new("logs");
        f.release();
        let (_, backup) = f.call("POST", "/api/tasks/backup/run", &[], r#"{"requestId":"req-logs-0001"}"#);
        let (_, digest) = f.call("POST", "/api/tasks/memory-digest/run", &[], r#"{"requestId":"req-logs-0002"}"#);
        let backup_id = backup["runId"].as_str().unwrap();
        let digest_id = digest["runId"].as_str().unwrap();
        f.wait_state(backup_id, true);
        f.wait_state(digest_id, true);

        let (status, log) = f.call("GET", &format!("/api/tasks/runs/{backup_id}/log"), &[], "");
        assert_eq!(status, 200);
        let text = log["lines"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("hello"), "backup output is not chat-bearing: {text}");
        for secret in ["abcdef123", "123456789", "987654321"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let (_, log) = f.call("GET", &format!("/api/tasks/runs/{digest_id}/log"), &[], "");
        assert_eq!(log["stdoutHidden"], true);
        let lines = log["lines"].as_array().unwrap();
        assert!(lines.is_empty(), "old chat-bearing stdout and stderr must both be hidden: {log}");
        assert!(!log.to_string().contains("hello"));

        let (_, page) = f.call("GET", &format!("/api/tasks/runs/{backup_id}/log"), &[("limit", "1")], "");
        assert_eq!(page["lines"].as_array().unwrap().len(), 1);
        let (_, next) = f.call("GET", &format!("/api/tasks/runs/{backup_id}/log"), &[("after", &page["nextAfter"].to_string())], "");
        assert_eq!(next["lines"][0]["n"], 2);
        assert_eq!(f.call("GET", "/api/tasks/runs/..%2Fruns/log", &[], "").0, 404);
        assert_eq!(f.call("GET", "/api/tasks/runs/backup-1-2-3/log", &[], "").0, 404);
    }

    #[test]
    fn run_history_pages_newest_first() {
        let f = Fixture::new("history");
        f.release();
        let mut ids = Vec::new();
        for i in 0..3 {
            let (_, r) = f.call("POST", "/api/tasks/backup/run", &[], &format!(r#"{{"requestId":"req-hist-000{i}"}}"#));
            let id = r["runId"].as_str().unwrap().to_string();
            f.wait_state(&id, true);
            ids.push(id);
        }
        let (_, page) = f.call("GET", "/api/tasks/runs", &[("limit", "2")], "");
        let got: Vec<&str> = page["runs"].as_array().unwrap().iter().map(|r| r["runId"].as_str().unwrap()).collect();
        assert_eq!(got, [ids[2].as_str(), ids[1].as_str()]);
        let before = page["nextBefore"].to_string();
        let (_, rest) = f.call("GET", "/api/tasks/runs", &[("limit", "2"), ("before", &before)], "");
        assert_eq!(rest["runs"][0]["runId"], ids[0]);
        assert_eq!(rest["nextBefore"], Value::Null);
        assert_eq!(rest["runs"][0]["state"], json!(RunState::Failed));
    }

    #[test]
    fn number_masking_keeps_short_numbers() {
        assert_eq!(mask_numbers("exit 2 after 12345 ms for 1234567"), "exit 2 after 12345 ms for <num>");
    }
}
