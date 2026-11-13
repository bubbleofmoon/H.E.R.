//! Ticket 27: the task page and its JSON routes.
//!
//! Mounted behind the console front (ticket 24): Host check, console credential, `application/json` and Origin checks
//! on POST, and the audit record of every administrator write happen before [`route`] is called (phase B wiring). This
//! module adds what is specific to tasks: agent credentials are refused on every task route; only registered task ids
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
    /// The request carried agent credentials (`x-agent-call` / `x-agent-token`).
    pub agent_call: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

const JSON_TYPE: &str = "application/json; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

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
pub fn route(api: &TasksApi, req: &TaskRequest) -> Option<TaskResponse> {
    let path = req.path;
    let is_task_route = path == "/tasks" || path == "/api/tasks" || path.starts_with("/api/tasks/");
    if !is_task_route {
        return None;
    }
    if req.agent_call {
        return error(403, "任务接口仅限管理员控制台，不接受 agent 凭据");
    }
    if path == "/tasks" {
        return match req.method {
            "GET" => Some(TaskResponse { status: 200, content_type: HTML_TYPE, body: PAGE.to_string() }),
            _ => error(405, "method not allowed"),
        };
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
            "POST" => set_lifecycle(api, req.body),
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
        use crate::lifecycle::{OwnerGate, OwnerProbe, Liveness, SUPERVISOR_LOCK};
        let desired_path = api.desired.path();
        let data = desired_path.parent()?.parent()?;
        let managed = match OwnerGate::new(data.join("logs").join(SUPERVISOR_LOCK)).probe() {
            OwnerProbe::Held { marker: Some(marker) } if marker.role == "supervisor" => {
                crate::lifecycle::current_identity().is_ok_and(|own|
                    if cfg!(windows) { own.exe.eq_ignore_ascii_case(&marker.identity.exe) } else { own.exe == marker.identity.exe })
                    && matches!(crate::lifecycle::check_identity(&marker.identity), Liveness::Matches)
            }
            _ => false,
        };
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

pub(crate) const PAGE: &str = r##"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>日程</title>
<style>
body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}
td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}th{background:#f4f4f4}
.state-failed,.state-unknown{color:#b00}.state-succeeded{color:#070}.state-running,.state-queued{color:#a60}
pre{background:#f8f8f8;border:1px solid #ddd;padding:8px;max-height:360px;overflow:auto;white-space:pre-wrap}
</style></head><body>
<h1>日程</h1>
<p>控制台令牌 <input id="token" type="password" size="32"> <button id="save">使用</button> <span id="desired"></span></p>
<p>运行状态： <button data-state="running">运行</button> <button data-state="maintenance">维护（暂停 live 和任务）</button> <button data-state="stopped">停止</button></p>
<h2 id="updates">HER 更新</h2>
<p id="release-available" role="status" hidden></p>
<button id="approve-update" type="button" hidden>批准更新</button>
<form id="update-settings"><label>更新策略 <select id="update-policy"><option value="off">关闭</option><option value="notify">通知（默认）</option><option value="auto">自动</option></select></label>
<button type="submit">保存</button> <span id="update-message" role="status"></span></form>
<h2 id="dependencies">依赖更新</h2>
<p>依赖升级始终需要管理员批准，包括自动策略。<button id="check-dependencies" type="button">检查依赖版本</button></p>
<div id="dependency-list"></div><p id="dependency-message" role="status"></p>
<h2 id="backups">备份设置</h2>
<form id="backup-settings">
<p><label>备份位置 <input id="backup-dir" size="54" placeholder="留空使用默认位置" spellcheck="false"></label></p>
<p>实际位置：<code id="backup-resolved" style="overflow-wrap:anywhere"></code></p>
<p><label>保留天数 <input id="backup-days" type="number" min="1" step="1" required></label> <label>每日时间（本地） <input id="backup-time" type="time" required></label> <label><input id="backup-state" type="checkbox"> 包含状态</label></p>
<p><button id="backup-save" type="submit">保存设置</button> <button type="button" data-run="backup" data-effect="创建并验证一次备份">立即备份</button> <span id="backup-message" role="status"></span></p>
<p>支持本地文件夹和 SMB 路径。SMB 凭据请在 Windows 凭据管理器中设置；保存后下次备份生效。</p>
</form>
<h3>备份记录 <button id="backup-refresh" type="button">刷新</button></h3>
<p><label>恢复范围 <select id="restore-scope"><option value="everything">全部（记忆、状态、角色和配置）</option><option value="memory">仅记忆</option></select></label></p>
<p id="backup-list-message" role="status"></p>
<table id="backup-list"><thead><tr><th>时间</th><th>文件 / 运行</th><th>大小</th><th>结果</th><th>恢复</th></tr></thead><tbody></tbody></table>
<h2 id="schedule">日程</h2><p><button id="tab-upcoming" type="button">即将运行</button> <button id="tab-history" type="button">历史</button> <span id="schedule-message" role="status"></span></p>
<section id="upcoming"><table id="schedule-upcoming"><thead><tr><th>类型</th><th>创建者</th><th>会话</th><th>下次运行</th><th>重复</th><th>动作</th><th>状态</th><th>创建时间</th><th>上次结果</th><th></th></tr></thead><tbody></tbody></table>
<form id="schedule-create"><label>会话 <input id="create-conversation" placeholder="group:123 / private:123" required></label> <label>时间 <input id="create-at" type="datetime-local" required></label> <label>重复 <select id="create-repeat"><option value="once">一次</option><option value="hourly">每小时</option><option value="daily">每天</option></select></label> <label>内容 <input id="create-text" size="40" maxlength="500" required></label> <button type="submit">新建提醒</button></form></section>
<section id="history" hidden><h3>提醒记录</h3><table id="schedule-history"><thead><tr><th>时间</th><th>项目</th><th>会话</th><th>结果</th></tr></thead><tbody></tbody></table>
<h2>运行历史</h2><p>筛选 <select id="filter"><option value="">全部</option></select> <button id="more">更早</button></p>
<table id="runs"><thead><tr><th>运行 ID</th><th>任务</th><th>来源</th><th>状态</th><th>退出码</th><th>原因</th><th>排队</th><th>开始</th><th>结束</th></tr></thead><tbody></tbody></table>
<h2>日志 <span id="logrun"></span></h2><pre id="log">选择一次运行查看日志</pre>
</section>
<h2>旧计划任务处置</h2><table id="legacy"><thead><tr><th>Windows 任务</th><th>处置</th><th>替代</th><th>说明</th></tr></thead><tbody></tbody></table>
<script>
const $=s=>document.querySelector(s);let before=null;
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();$('#save').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);load();loadBackupSettings();loadUpdates()};
const t=ms=>ms?new Date(ms).toLocaleString('zh-CN',{hour12:false}):'';
const esc=s=>String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
async function api(path,opt={}){const r=await fetch(path,{...opt,headers:{'x-console-token':token(),'content-type':'application/json'}});
 const j=await r.json().catch(()=>({ok:false,error:'HTTP '+r.status}));j.status=r.status;return j}

let displayedRelease=null;
async function loadUpdates(){try{const j=await api('/api/releases');if(!j.ok)throw Error(j.error);displayedRelease=j;$('#update-policy').value=j.settings.policy;const b=$('#release-available');b.hidden=!j.available;b.textContent=j.available?'HER 更新可用：'+(j.current||'未知版本')+' → '+j.available.tag:'';$('#approve-update').hidden=!j.available;$('#approve-update').disabled=!!j.approval;if(j.updateError)$('#update-message').textContent=j.updateError}catch(e){displayedRelease=null;$('#approve-update').hidden=true;$('#update-message').textContent=e.message}}
$('#approve-update').onclick=async()=>{const j=displayedRelease;if(!j?.available||!confirm('批准更新 HER：'+(j.current||'未知版本')+' → '+j.available.tag+'？'))return;$('#approve-update').disabled=true;try{const r=await api('/api/releases/approve',{method:'POST',body:JSON.stringify({confirm:true,tag:j.available.tag})});if(!r.ok)throw Error(r.error);$('#update-message').textContent='已批准，正在更新…'}catch(e){$('#update-message').textContent=e.message;$('#approve-update').disabled=false}};
$('#update-settings').onsubmit=async e=>{e.preventDefault();try{const j=await api('/api/releases/settings',{method:'POST',body:JSON.stringify({policy:$('#update-policy').value})});if(!j.ok)throw Error(j.error);$('#update-message').textContent='已保存';await loadUpdates()}catch(error){$('#update-message').textContent=error.message}};
loadUpdates();setInterval(loadUpdates,30000);
async function loadDependencies(){try{const j=await api('/api/dependencies');if(!j.ok)throw Error(j.error);const list=$('#dependency-list');list.replaceChildren();for(const [name,d] of Object.entries(j.dependencies)){const row=document.createElement('p');row.textContent=name+': '+(d.current||'未配置')+(d.available?' → '+d.available:(d.latest?'；最新版本：'+d.latest:''))+' '+(d.sourceUnavailable||d.error||'');if(d.available){const button=document.createElement('button');button.textContent='批准升级';button.disabled=!!j.approval||j.result?.status==='running';button.onclick=async()=>{if(!confirm('批准升级 '+name+'：'+d.current+' → '+d.available+'？'))return;button.disabled=true;try{const r=await api('/api/dependencies/approve',{method:'POST',body:JSON.stringify({confirm:true,name,version:d.available})});if(!r.ok)throw Error(r.error);await loadDependencies();$('#dependency-message').textContent='已批准，等待现有会话完成后升级…'}catch(e){$('#dependency-message').textContent=e.message;button.disabled=false}};row.append(button)}list.append(row)}$('#dependency-message').textContent=j.updateError||(j.result?.status?j.result.name+': '+j.result.status:'')}catch(e){$('#dependency-message').textContent=e.message}}
$('#check-dependencies').onclick=async()=>{const b=$('#check-dependencies');b.disabled=true;try{const j=await api('/api/dependencies/check',{method:'POST',body:'{}'});if(!j.ok)throw Error(j.error);await loadDependencies()}catch(e){$('#dependency-message').textContent=e.message}finally{b.disabled=false}};
loadDependencies();setInterval(loadDependencies,30000);

function fillBackupSettings(s){$('#backup-dir').value=s.dir??'';$('#backup-days').value=s.keepDays;$('#backup-time').value=s.time;$('#backup-state').checked=s.includeState;$('#backup-resolved').textContent=s.resolvedDir}
async function loadBackupSettings(){try{const j=await api('/api/backups/settings');if(!j.ok)throw Error(j.error);fillBackupSettings(j.settings);$('#backup-message').textContent=''}catch(e){$('#backup-message').textContent=e.message}}
$('#backup-settings').onsubmit=async e=>{e.preventDefault();const b=$('#backup-save'),message=$('#backup-message');b.disabled=true;try{
 const j=await api('/api/backups/settings',{method:'POST',body:JSON.stringify({dir:$('#backup-dir').value||null,keepDays:Number($('#backup-days').value),time:$('#backup-time').value,includeState:$('#backup-state').checked})});
 if(!j.ok){message.textContent=j.error;return}fillBackupSettings(j.settings);message.textContent='已保存；下次备份生效';await load()
}catch(error){message.textContent=error.message}finally{b.disabled=false}};
async function loadBackupList(){try{const j=await api('/api/backups');if(!j.ok)throw Error(j.error);
 const labels={verified:'已验证',unverified:'未验证',pending:'尚未验证',failed:'失败',unknown:'结果未知',cancelled:'已取消'};
 const rows=[...j.backups,...j.failedAttempts.map(r=>({time:r.endedAt||r.startedAt||r.queuedAt,name:r.runId,size:null,result:r.state}))].sort((a,b)=>b.time-a.time);
 $('#backup-list tbody').innerHTML=rows.map(r=>`<tr><td>${t(r.time)}</td><td>${esc(r.name)}</td><td>${r.size==null?'':esc(r.size)+' B'}</td><td>${esc(labels[r.result]||r.result)}</td><td>${r.result==='verified'?`<button type="button" data-restore="${esc(r.name)}">恢复…</button>`:''}</td></tr>`).join('');
 $('#backup-list-message').textContent=rows.length?'':'暂无备份'
}catch(e){$('#backup-list-message').textContent=e.message}}
$('#backup-refresh').onclick=loadBackupList;
$('#backup-list tbody').onclick=async e=>{
 const b=e.target.closest('[data-restore]');if(!b)return;b.disabled=true;
 const message=$('#backup-list-message'),selection={archive:b.dataset.restore,memoryOnly:$('#restore-scope').value==='memory'};
 try{
  const j=await api('/api/backups/restore/preview',{method:'POST',body:JSON.stringify(selection)});if(!j.ok)throw Error(j.error);
  const p=j.preview;
  const question='恢复备份：'+selection.archive+'\n目标：'+p.runtimeRoot+'\n时间：'+t(p.time)+'\n范围：'+(selection.memoryOnly?'仅记忆':'全部')+'\n内容：\n'+p.contents.join('\n')+'\n将替换：\n'+p.replaces.join('\n')+'\n将删除：\n'+(p.removes||[]).join('\n')+'\n恢复前会备份当前状态；健康检查失败会自动撤回。确认恢复？';
  if(!confirm(question))return;
  const result=await api('/api/backups/restore',{method:'POST',body:JSON.stringify({...selection,confirmation:p.confirmation})});if(!result.ok)throw Error(result.error);
  message.textContent='恢复已开始；控制台会重新启动，请稍后刷新。结果请查看审计日志。'
 }catch(error){message.textContent=error.message}finally{b.disabled=false}
};

function runRow(r){return `<tr><td><a href="#" data-log="${esc(r.runId)}">${esc(r.runId)}</a></td><td>${esc(r.taskId)}</td><td>${esc(r.source)}</td>
 <td class="state-${esc(r.state)}">${esc(r.state)}</td><td>${esc(r.exitCode)}</td><td>${esc(r.reason)}</td><td>${t(r.queuedAt)}</td><td>${t(r.startedAt)}</td><td>${t(r.endedAt)}</td></tr>`}
async function load(){const j=await api('/api/tasks');if(!j.ok){$('#desired').textContent=j.error;return}
 $('#desired').textContent='期望状态：'+j.desired.state;
  $('#legacy tbody').innerHTML=j.legacy.map(x=>`<tr><td>${esc(x.name)}</td><td>${x.disposition==='retired'?'退役':'迁移'}</td><td>${esc(x.replacement)}</td><td>${esc(x.note)}</td></tr>`).join('');
 if($('#filter').options.length===1)for(const x of j.tasks){const o=document.createElement('option');o.value=x.id;o.textContent=x.title;$('#filter').append(o)}
 before=null;$('#runs tbody').innerHTML='';await runs();await loadBackupList();await loadSchedule()}
async function runs(){const q=new URLSearchParams({limit:'50'});if($('#filter').value)q.set('task',$('#filter').value);if(before)q.set('before',before);
 const j=await api('/api/tasks/runs?'+q);if(!j.ok)return;$('#runs tbody').insertAdjacentHTML('beforeend',j.runs.map(runRow).join(''));before=j.nextBefore;$('#more').disabled=!before}
async function showLog(id){$('#logrun').textContent=id;let after=null,text='';
 for(;;){const q=new URLSearchParams();if(after)q.set('after',after);const j=await api(`/api/tasks/runs/${encodeURIComponent(id)}/log?`+q);
  if(!j.ok){text+=j.error;break}text+=j.lines.map(l=>`[${l.stream}] ${l.text}`).join('\n')+(j.lines.length?'\n':'');
  if(j.stdoutHidden&&after===null)text='（此任务的输出可能含聊天内容，仅显示元数据）\n'+text;after=j.nextAfter;if(!after)break}
 $('#log').textContent=text||'（无日志）'}
async function follow(id){for(;;){const j=await api('/api/tasks/runs/'+encodeURIComponent(id));if(!j.ok||!['queued','running'].includes(j.run.state)){await load();return showLog(id)}
 await new Promise(r=>setTimeout(r,1500))}}
document.addEventListener('click',async e=>{const control=e.target.closest('[data-state]');if(control){const state=control.dataset.state;
 if(!confirm('切换运行状态到 '+state+'？这会影响后台进程与任务。'))return;
 const j=await api('/api/tasks/lifecycle',{method:'POST',body:JSON.stringify({state,confirm:true})});if(!j.ok)alert(j.error);else await load();return}
 const b=e.target.closest('[data-run]');if(b){if(!confirm('运行「'+b.dataset.run+'」？\n'+b.dataset.effect))return;
  const requestId=(crypto.randomUUID?crypto.randomUUID():String(Date.now())+Math.random().toString(16).slice(2)).replace(/[^A-Za-z0-9_-]/g,'');
  if(b.dataset.run==='console-restart'){b.disabled=true;const j=await api('/api/restart',{method:'POST',body:JSON.stringify({requestId,confirm:true})});
   if(!j.ok){alert(j.error);b.disabled=false;return}alert('已排队，完成后可在运行历史查看结果');setTimeout(()=>load().catch(()=>{}),4000);return}
  b.disabled=true;const j=await api(`/api/tasks/${encodeURIComponent(b.dataset.run)}/run`,{method:'POST',body:JSON.stringify({requestId})});
  if(!j.ok){alert(j.error);if(j.runId)follow(j.runId);else load();return}follow(j.runId);return}
 const l=e.target.closest('[data-log]');if(l){e.preventDefault();showLog(l.dataset.log)}});
const schedKinds={job:'任务',reminder:'提醒',sleep:'潜水结束',replyCheck:'回复检查',reachOut:'主动冒泡'};
const schedCreators={system:'系统',owner:'管理员',her:'HER'};
const schedStatus={active:'待运行',paused:'已暂停',overdue:'已逾期',running:'运行中',disabled:'未启用'};
const schedLabels={pause:'暂停',resume:'恢复',edit:'编辑',cancel:'取消',run:'立即运行'};
function schedRow(x){const buttons=x.actions.map(a=>x.kind==='job'&&a==='run'?`<button data-run="${esc(x.id.slice(7))}" data-effect="${esc(x.action)}" ${x.status==='running'?'disabled':''}>${schedLabels.run}</button>`:`<button type="button" data-sched="${esc(x.id)}" data-act="${a}" data-text="${esc(x.action)}">${schedLabels[a]}</button>`).join(' ');
 return `<tr><td>${esc(schedKinds[x.kind]||x.kind)}</td><td>${esc(schedCreators[x.creator]||x.creator)}</td><td>${esc(x.conversation??'')}</td><td>${x.nextRunAt==null?'':t(x.nextRunAt)}</td><td>${esc(x.repeat)}</td><td>${esc(x.action)}</td><td>${esc(schedStatus[x.status]||x.status)}</td><td>${t(x.createdAt)}</td><td>${x.lastResult?esc(x.lastResult.summary)+' '+t(x.lastResult.at):''}</td><td>${buttons}</td></tr>`}
async function loadSchedule(){try{const j=await api('/api/schedule');if(!j.ok)throw Error(j.error);$('#schedule-upcoming tbody').innerHTML=j.entries.map(schedRow).join('');$('#schedule-message').textContent=''}catch(e){$('#schedule-message').textContent=e.message}}
async function loadScheduleHistory(){try{const j=await api('/api/schedule/history?limit=100');if(!j.ok)throw Error(j.error);$('#schedule-history tbody').innerHTML=j.history.map(r=>`<tr><td>${t(r.at)}</td><td>${esc(r.id)}</td><td>${esc(r.conversation)}</td><td>${esc(r.summary)}${r.late?'（迟到）':''}</td></tr>`).join('');$('#schedule-message').textContent=''}catch(e){$('#schedule-message').textContent=e.message}}
$('#tab-upcoming').onclick=()=>{$('#upcoming').hidden=false;$('#history').hidden=true};
$('#tab-history').onclick=()=>{$('#upcoming').hidden=true;$('#history').hidden=false;loadScheduleHistory()};
document.addEventListener('click',async e=>{const b=e.target.closest('[data-sched]');if(!b)return;
 const id=b.dataset.sched,act=b.dataset.act,message=$('#schedule-message'),body={};
 if(id.startsWith('system:')){if(!confirm((act==='pause'?'暂停':'恢复')+'系统任务「'+id.slice(7)+'」？'))return;body.confirm=true}
 if(act==='cancel'&&!confirm('取消这条日程？'))return;
 if(act==='edit'){const at=prompt('新的时间（本地，例如 2026-11-14 09:00）','');if(!at)return;const ms=new Date(at.replace(' ','T')).getTime();if(Number.isNaN(ms)){message.textContent='时间格式无效';return}body.at=ms;
  if(id.startsWith('item:')){const text=prompt('提醒内容',b.dataset.text||'');if(text===null)return;body.text=text}}
 b.disabled=true;
 try{const j=await api('/api/schedule/'+id+'/'+act,{method:'POST',body:JSON.stringify(body)});if(!j.ok){message.textContent=j.error||'操作失败';return}message.textContent='已完成';await loadSchedule()}
 catch(error){message.textContent=error.message}finally{b.disabled=false}});
$('#schedule-create').onsubmit=async e=>{e.preventDefault();const message=$('#schedule-message');const local=new Date($('#create-at').value);const at=local.getTime();
 const kind=$('#create-repeat').value;
 const repeat=kind==='hourly'?{kind:'every',minutes:60}:kind==='daily'?{kind:'daily',hour:local.getHours(),minute:local.getMinutes()}:{kind:'once'};
 const j=await api('/api/schedule',{method:'POST',body:JSON.stringify({conversation:$('#create-conversation').value,text:$('#create-text').value,at,repeat})});
 if(!j.ok){message.textContent=j.error;return}message.textContent='已新建';e.target.reset();await loadSchedule()};
$('#filter').onchange=()=>{before=null;$('#runs tbody').innerHTML='';runs()};$('#more').onclick=runs;load();loadBackupSettings();
</script></body></html>
"##;

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
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-console-tasks-{name}-{}-{}", std::process::id(), crate::parity_clock::now_ms())));
            std::fs::create_dir_all(&dir).unwrap();
            let clock = Arc::new(ManualClock::new(1_793_289_600_000));
            let store = Arc::new(TaskStore::open(dir.join("tasks"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
            let held = Arc::new(Held { open: Mutex::new(false), cv: Condvar::new() });
            let dispatcher = Dispatcher::new(store, Arc::new(HeldExecutor(held.clone())), clock.clone());
            let scheduler = Arc::new(Mutex::new(Scheduler::new(|d| d.enabled_by_default, 8 * 3_600_000, clock.now_ms())));
            let api = TasksApi { dispatcher, scheduler, desired: DesiredStore::new(dir.join("lifecycle")) };
            Self { api, held, dir }
        }

        fn release(&self) {
            *self.held.open.lock().unwrap() = true;
            self.held.cv.notify_all();
        }

        fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: &str) -> (u16, Value) {
            let query: HashMap<String, String> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let response = route(&self.api, &TaskRequest { method, path, query: &query, body, agent_call: false }).expect("a task route");
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
            assert!(route(&f.api, &TaskRequest { method: "GET", path, query: &query, body: "", agent_call: false }).is_none(), "{path}");
        }
    }

    #[test]
    fn the_page_is_served_and_agents_are_refused_everywhere() {
        let f = Fixture::new("page");
        let query = HashMap::new();
        let page = route(&f.api, &TaskRequest { method: "GET", path: "/tasks", query: &query, body: "", agent_call: false }).unwrap();
        assert_eq!((page.status, page.content_type), (200, HTML_TYPE));
        assert!(page.body.contains("/api/tasks") && page.body.contains("x-console-token") && page.body.contains("requestId"));
        for (method, path) in [("GET", "/tasks"), ("GET", "/api/tasks"), ("GET", "/api/tasks/runs"), ("POST", "/api/tasks/backup/run")] {
            let r = route(&f.api, &TaskRequest { method, path, query: &query, body: r#"{"requestId":"req-agent-01"}"#, agent_call: true }).unwrap();
            assert_eq!(r.status, 403, "{method} {path}");
        }
        assert!(f.api.dispatcher.store().list(&RunQuery::default()).unwrap().runs.is_empty(), "an agent starts nothing");
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
