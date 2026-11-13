//! R4：后端抽象层。
//!
//! 目标：把「谁来跑这一轮对话」从 Pipeline 里抽出来，做成可切换的 `Backend`。
//! 现在只有 DSH 一个实现，另外三种 CLI（opencode / claude / codex）用同一个
//! 通用 `CliBackend` 覆盖 —— 它们的差别只是命令行和参数。
//!
//! 不在这一层做的事（先划清楚，免得半成品看起来很能打）：
//!   * 不做 opencode 的 serve + session 协议（那是 HTTP 长连接，和一次性 CLI 调用是两码事）；
//!   * 不做流式事件（DSH 以外都是"一次性拿完整输出"）。
//!
//! 所以 `stream_events` 在 trait 里**故意不存在**：现在没有实现需要它，
//! 摆一个空抽象比没有更糟 —— 会让人以为已经支持了。

// 抽象层的接口现在只被 --backends 和单测用到，接进主链路之前整体允许 dead_code。
#![allow(dead_code)]

use serde_json::{json, Value};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

/// 一次请求。
#[derive(Debug, Clone)]
pub struct BackendRequest {
    /// 会话 key（group:xxx / private:xxx），CLI 后端会把它作为会话标识传下去。
    pub key: String,
    pub prompt: String,
    /// 超时秒数。
    pub timeout_s: u64,
}

/// 一次成功的回复。
#[derive(Debug, Clone)]
pub struct BackendReply {
    pub text: String,
    /// 实际用的后端 id（failover 之后可能和请求的不一样）。
    pub backend: String,
}

#[derive(Debug)]
pub enum BackendError {
    /// 后端不可用（命令不存在 / 未安装）——可以换下一个候选。
    Unavailable(String),
    /// 跑了但失败（非零退出 / 超时）——可以换下一个候选。
    Failed(String),
    /// 配置错（写错了 backend id）——不该换，直接报。
    Config(String),
}

impl BackendError {
    pub fn message(&self) -> &str {
        match self {
            BackendError::Unavailable(m) | BackendError::Failed(m) | BackendError::Config(m) => m,
        }
    }
    /// 是否值得尝试下一个候选。
    pub fn retryable(&self) -> bool {
        !matches!(self, BackendError::Config(_))
    }
}

pub trait Backend {
    fn id(&self) -> &str;
    fn kind(&self) -> &'static str;
    /// 后端当前是否可用（命令在 PATH 里 / 服务可达）。只做轻量检查，不真的跑一轮。
    fn available(&self) -> bool;
    fn describe(&self) -> Value;
    fn ask(&self, req: &BackendRequest) -> Result<BackendReply, BackendError>;
}

/// 通用 CLI 后端：把 prompt 从 stdin 喂给命令，读 stdout。
///
/// 为什么用 stdin 而不是命令行参数：提示词动辄几千字，走 argv 会撞命令行长度上限，
/// 而且会被进程列表看到（群里聊的内容不该出现在 `ps` 里）。
pub struct CliBackend {
    pub id: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    context: RuntimeContext,
}

impl CliBackend {
    pub fn from_config(id: &str, v: &Value) -> Result<Self, BackendError> {
        Self::from_config_with_runtime(&RuntimeContext::legacy(), id, v)
    }

    pub fn from_config_with_runtime(
        context: &RuntimeContext,
        id: &str,
        v: &Value,
    ) -> Result<Self, BackendError> {
        let cmd = v
            .get("cmd")
            .and_then(|c| c.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| BackendError::Config(format!("后端 {id} 缺少 cmd")))?;
        let mut args: Vec<String> = v
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if is_agy(id, cmd) {
            // Agy can report a provider failure on stdout with exit 0. Never request prose.
            let mut i = 0;
            while i < args.len() {
                if args[i] == "--output-format" {
                    args.remove(i);
                    if i < args.len() { args.remove(i); }
                } else if args[i].starts_with("--output-format=") { args.remove(i); }
                else { i += 1; }
            }
            args.extend(["--output-format".into(), "json".into()]);
        }
        let env = v
            .get("env")
            .and_then(|e| e.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            id: id.to_string(),
            cmd: cmd.to_string(),
            args,
            env,
            context: context.clone(),
        })
    }
}

impl Backend for CliBackend {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "cli"
    }
    fn available(&self) -> bool {
        command_in_path(&self.cmd)
    }
    fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "kind": "cli",
            "cmd": self.cmd,
            "args": self.args,
            "available": self.available(),
        })
    }
    fn ask(&self, req: &BackendRequest) -> Result<BackendReply, BackendError> {
        let exe = resolve_command(&self.cmd).ok_or_else(|| {
            BackendError::Unavailable(format!("命令 {} 不在 PATH 里", self.cmd))
        })?;
        if self.context.is_isolated() {
            validate_isolated_cli_settings(&self.context, &self.args, &self.env)
                .map_err(BackendError::Config)?;
        }
        // 输出走**临时文件**而不是管道：管道有两个坑 ——
        // ① 缓冲区写满会阻塞子进程；② `wait_with_output` 没有超时，
        //    CLI 后端是 agent（会自己跑工具循环），实测长提示词下能跑 19 分钟。
        // 文件 + try_wait 轮询能同时解决这两件事。
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let (out_path, err_path) = if self.context.is_isolated() {
            let out = self
                .context
                .resolve_path(RuntimeArea::Logs, &format!("logs/cli-{stamp}.out"), PathIntent::WriteFile)
                .map_err(|e| BackendError::Failed(e.into()))?;
            let err = self
                .context
                .resolve_path(RuntimeArea::Logs, &format!("logs/cli-{stamp}.err"), PathIntent::WriteFile)
                .map_err(|e| BackendError::Failed(e.into()))?;
            (out, err)
        } else {
            (
                std::path::PathBuf::from(format!("C:/qqbot/logs/cli-{stamp}.out")),
                std::path::PathBuf::from(format!("C:/qqbot/logs/cli-{stamp}.err")),
            )
        };
        let mut output_files = CliOutputFiles::new(&self.context, out_path, err_path)?;
        let out_file = output_files.open_stdout()?;
        let err_file = output_files.open_stderr()?;
        let mut cmd = crate::subprocess::command_without_console(crate::subprocess::external_path(&exe).map_err(BackendError::Config)?);
        cmd.args(self.args.iter().map(crate::subprocess::external_path).collect::<Result<Vec<_>, _>>().map_err(BackendError::Config)?)
            .stdin(std::process::Stdio::piped())
            .stdout(out_file)
            .stderr(err_file);
        // 三个 CLI 认的变量名各不相同，但**用的是同一把 key**。
        // key 从 DSH 的凭据文件读（~/.dsh/.credentials.yaml 的 OPENCODE_GO_API_KEY），
        // 绝不写进 config.json、绝不进日志 —— 全机器只有一份，轮换时只改一处。
        for key in [
            "DSH_HOME", "DSH_CREDENTIALS", "USERPROFILE", "HOME", "OPENCODE_GO_API_KEY",
            "OPENCODE_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "OPENAI_API_KEY",
            "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
        ] {
            if self.context.is_isolated() {
                cmd.env_remove(key);
            }
        }
        if self.context.is_isolated() {
            configure_isolated_cli_environment(&mut cmd, &self.context)?;
        }
        for (k, v) in credential_env_for_runtime(&self.context) {
            cmd.env(k, v);
        }
        // 会话身份：CLI 后端（尤其 opencode）把自己的 MCP 工具接过去之后，
        // 模型并不知道当前会话的 key 和令牌 —— 不注入就只能靠模型瞎猜，
        // 实测会得到「未授权：智能体调用携带的会话令牌无效」。
        for (k, v) in session_env_for_runtime(&self.context, &req.key) {
            cmd.env(k, v);
        }
        // 关掉自动更新：这几个 CLI 默认会自己升级，升级后命令行/输出格式都可能变，
        // 而我们对拍依赖它们的行为稳定。
        cmd.env("DISABLE_AUTOUPDATER", "1")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CODEX_DISABLE_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env("CI", "1");
        for (k, v) in &self.env {
            if self.context.is_isolated() && isolated_environment_key(k) {
                return Err(BackendError::Config(format!(
                    "isolated backend {} cannot override selected runtime environment variable {k}",
                    self.id
                )));
            }
            cmd.env(k, v);
        }
        if self.context.is_isolated() {
            let model_url = self
                .context
                .dependency_url("model")
                .ok_or_else(|| BackendError::Failed("runtime model dependency is unavailable".into()))?;
            cmd.env("ANTHROPIC_BASE_URL", model_url)
                .env("OPENAI_BASE_URL", format!("{}/v1", model_url.trim_end_matches('/')));
        }
        let status = if self.context.is_isolated() {
            run_isolated_cli(cmd, &req.prompt, req.timeout_s, &self.cmd)?
        } else {
            run_legacy_cli(cmd, &req.prompt, req.timeout_s, &self.cmd)?
        };
        // 进程已退出，从临时文件读结果
        let (stdout_text, stderr_text) = output_files.read_texts()?;
        let text = decode_cli_exit(&stdout_text, &stderr_text, is_agy(&self.id, &self.cmd), &self.id, status.code())?;
        Ok(BackendReply {
            text,
            backend: self.id.clone(),
        })
    }
}

/// Process completion is decoded before any answer or failure leaves the CLI boundary.
pub(crate) fn decode_cli_exit(
    stdout_text: &str,
    stderr_text: &str,
    structured: bool,
    id: &str,
    code: Option<i32>,
) -> Result<String, BackendError> {
    if code != Some(0) {
        // Exit status is authoritative for failure, but a structured error still carries
        // the provider's class/code (including safety refusals that also exit nonzero).
        let failure = decode_cli_output(stdout_text, structured)
            .err()
            .map(|error| crate::model_failure::Failure::classify("", error.message()))
            .filter(|failure| !matches!(failure.code.as_str(), "INVALID_RESULT" | "EMPTY_ANSWER"))
            .unwrap_or_else(|| {
                crate::model_failure::Failure::classify(
                    "",
                    &format!("{stdout_text}\n{stderr_text}"),
                )
            });
        return Err(BackendError::Failed(format!(
            "{} command={} exit_code={:?}",
            failure.summary(),
            id,
            code
        )));
    }
    decode_cli_output(stdout_text, structured)
}

fn is_agy(id: &str, cmd: &str) -> bool {
    let name = cmd.replace('\\', "/").rsplit('/').next().unwrap_or(cmd).to_ascii_lowercase();
    matches!(name.as_str(), "agy" | "agy.exe" | "agy.cmd" | "agy.bat")
        || id == "agy" || id.starts_with("agy-")
}

/// Only recognized result envelopes are answers. Plain-text clients may return prose,
/// but process diagnostics and event streams never become conversation text.
pub(crate) fn decode_cli_output(stdout: &str, structured: bool) -> Result<String, BackendError> {
    use crate::model_failure::Failure;
    let fail = |failure: Failure| BackendError::Failed(failure.summary());
    let text = stdout.trim();
    if text.is_empty() { return Err(fail(Failure::new("empty-response", "EMPTY_ANSWER"))); }
    let parsed = serde_json::from_str::<Value>(text).ok();
    if let Some(failure) = parsed.as_ref().and_then(json_output_failure) {
        return Err(fail(failure));
    }
    let answer = match parsed.as_ref() {
        Some(value) if value.is_object() => {
            // Agy's JSON result carries its answer in `response`; other CLI result
            // envelopes use `result` (Claude) or `text`. Unknown objects fail closed.
            let keys: &[&str] = if structured { &["response"] } else { &["response", "result", "text"] };
            keys.iter().find_map(|key| value.get(key).and_then(Value::as_str))
                .ok_or_else(|| fail(Failure::new("backend", "INVALID_RESULT")))?.to_string()
        }
        _ if structured => return Err(fail(plain_output_failure(text)
            .unwrap_or_else(|| Failure::new("backend", "INVALID_RESULT")))),
        _ => {
            // A JSON-lines stream is a protocol, never a plain-text answer. Scan all
            // events before refusing it so a later error retains its class/code.
            let mut events = false;
            for line in text.lines() {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.is_object() {
                        events = true;
                        if let Some(failure) = json_output_failure(&value) { return Err(fail(failure)); }
                    }
                }
            }
            if events { return Err(fail(Failure::new("backend", "INVALID_RESULT"))); }
            text.to_string()
        }
    };
    if let Some(failure) = plain_output_failure(&answer) { return Err(fail(failure)); }
    Ok(answer)
}

pub(crate) fn json_output_failure(value: &Value) -> Option<crate::model_failure::Failure> {
    use crate::model_failure::Failure;
    let error = value.get("error").filter(|v| !v.is_null() && **v != Value::Bool(false));
    let code = error.and_then(|e| e.get("code").or_else(|| e.get("type")))
        .filter(|v| v.is_string() || v.is_number())
        .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let error_result = error.is_some() || value["is_error"] == true || value["isError"] == true
        || value["type"] == "error" || value["status"] == "error"
        || value["ok"] == false || value["success"] == false
        || value.get("errorMessage").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
        || value["subtype"].as_str().is_some_and(|s| s.starts_with("error"));
    if error_result { return Some(Failure::classify(&code, &value.to_string())); }
    if value.get("candidates").and_then(Value::as_array).is_some_and(Vec::is_empty)
        || value.pointer("/promptFeedback/blockReason").is_some()
        || value.get("refusal").is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
        || value["type"] == "refusal"
        || value.pointer("/choices/0/finish_reason").and_then(Value::as_str) == Some("content_filter")
        || value["candidates"].as_array().is_some_and(|candidates| candidates.iter().any(|c|
            c["finishReason"].as_str().is_some_and(|r| matches!(r, "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "RECITATION")))) {
        return Some(Failure::new("refusal", "MODEL_REFUSAL"));
    }
    // Nested error events (and errors wrapped in `result`) remain failures.
    match value {
        Value::Object(obj) => obj.values().filter(|v| v.is_object() || v.is_array()).find_map(json_output_failure),
        Value::Array(items) => items.iter().find_map(json_output_failure),
        _ => None,
    }
}

pub fn plain_output_failure(answer: &str) -> Option<crate::model_failure::Failure> {
    use crate::model_failure::Failure;
    if answer.trim().is_empty() { return Some(Failure::new("empty-response", "EMPTY_ANSWER")); }
    static DIAGNOSTIC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let diagnostic = DIAGNOSTIC.get_or_init(|| regex::Regex::new(
        r"(?im)^\s*(?:traceback \(most recent call last\):|[a-z_][a-z0-9_.]*(?:error|exception):|exception:|error:|api error:|fatal:|\[error\]|http(?:/\d(?:\.\d)?)?\s+[45]\d{2}\b|(?:<!doctype html|<html|<head|<body)\b|thread '[^']*' panicked|the prompt could not be submitted|the prompt was blocked|i cannot assist with|i can't assist with)"
    ).unwrap());
    diagnostic.is_match(answer).then(|| Failure::classify("", answer))
}

fn run_legacy_cli(
    mut command: Command,
    prompt: &str,
    timeout_s: u64,
    command_name: &str,
) -> Result<ExitStatus, BackendError> {
    let mut child = command
        .spawn()
        .map_err(|error| BackendError::Failed(format!("启动 {command_name} 失败：{error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        if let Err(error) = stdin.write_all(prompt.as_bytes()) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(BackendError::Failed(format!(
                "写入 {command_name} 标准输入失败：{error}"
            )));
        }
        // Legacy CLI clients keep the original synchronous stdin contract.
        drop(stdin);
    }

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(timeout_s.max(5).min(3600));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BackendError::Failed(format!(
                    "{command_name} 超时（{timeout_s}s）已强杀"
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BackendError::Failed(format!(
                    "等待 {command_name} 失败：{error}"
                )));
            }
        }
    }
}

#[cfg(windows)]
fn run_isolated_cli(
    command: Command,
    prompt: &str,
    timeout_s: u64,
    command_name: &str,
) -> Result<ExitStatus, BackendError> {
    windows_cli_lifecycle::run(command, prompt, timeout_s, command_name)
}

#[cfg(not(windows))]
fn run_isolated_cli(
    _command: Command,
    _prompt: &str,
    _timeout_s: u64,
    command_name: &str,
) -> Result<ExitStatus, BackendError> {
    Err(BackendError::Failed(format!(
        "isolated CLI process-tree containment is unsupported on this platform: {command_name}"
    )))
}

#[cfg(windows)]
pub(crate) mod windows_cli_lifecycle {
    use super::BackendError;
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, ChildStdin, Command, ExitStatus};
    use std::thread::ScopedJoinHandle;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
        JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenThread, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
        THREAD_SUSPEND_RESUME,
    };

    const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;
    const ERROR_NO_MORE_FILES: u32 = 18;
    const CLEANUP_GRACE: Duration = Duration::from_millis(1_500);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn close(&mut self) {
            if !self.0.is_null() && self.0 as isize != -1 {
                let handle = self.0;
                self.0 = std::ptr::null_mut();
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            self.close();
        }
    }

    struct JobObject(OwnedHandle);

    fn wait_stopped_until(
        processes: &[OwnedHandle],
        deadline: Instant,
        empty: impl Fn() -> Result<bool, String>,
    ) -> Result<bool, String> {
        loop {
            // Job accounting can reach zero before the last process object is signaled.
            // Keep the exact process handles pinned and require both witnesses.
            let mut signaled = true;
            for process in processes {
                match unsafe { WaitForSingleObject(process.0, 0) } {
                    WAIT_OBJECT_0 => {}
                    WAIT_TIMEOUT => signaled = false,
                    _ => return Err(format!(
                        "could not wait for CLI process: Win32 error {}",
                        unsafe { GetLastError() }
                    )),
                }
            }
            if signaled && empty()? {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(deadline.saturating_duration_since(now).min(POLL_INTERVAL));
        }
    }

    impl JobObject {
        fn new() -> Result<Self, String> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed with Win32 error {}",
                    unsafe { GetLastError() }
                ));
            }
            let job = Self(OwnedHandle(handle));
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job.0.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const std::ffi::c_void,
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0 {
                return Err(format!(
                    "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed with Win32 error {}",
                    unsafe { GetLastError() }
                ));
            }
            Ok(job)
        }

        fn is_empty(&self) -> Result<bool, String> {
            if self.0.0.is_null() {
                return Err("job handle was already closed".into());
            }
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            let mut returned_length = 0u32;
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0.0,
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of_val(&accounting) as u32,
                    &mut returned_length,
                )
            };
            if queried == 0 {
                return Err(format!(
                    "QueryInformationJobObject(BasicAccountingInformation) failed with Win32 error {}",
                    unsafe { GetLastError() }
                ));
            }
            if returned_length < std::mem::size_of_val(&accounting) as u32 {
                return Err(format!(
                    "QueryInformationJobObject returned a short accounting record ({returned_length} bytes)"
                ));
            }
            Ok(accounting.ActiveProcesses == 0)
        }

        fn process_handles(&self) -> Result<Vec<OwnedHandle>, String> {
            // The PID list has an aligned variable-length ULONG_PTR tail. Pin it before
            // requesting termination, while the job still exposes its active members.
            let mut buffer = vec![0usize; 65];
            loop {
                let queried = unsafe {
                    QueryInformationJobObject(
                        self.0.0,
                        JobObjectBasicProcessIdList,
                        buffer.as_mut_ptr().cast(),
                        (buffer.len() * std::mem::size_of::<usize>()) as u32,
                        std::ptr::null_mut(),
                    )
                };
                if queried == 0 {
                    let error = unsafe { GetLastError() };
                    if error == ERROR_MORE_DATA {
                        buffer.resize(buffer.len() * 2, 0);
                        continue;
                    }
                    return Err(format!("could not enumerate CLI job processes: Win32 error {error}"));
                }
                let info = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
                let ids = unsafe {
                    std::slice::from_raw_parts(
                        info.ProcessIdList.as_ptr(), info.NumberOfProcessIdsInList as usize,
                    )
                };
                let mut handles = Vec::new();
                for &pid in ids {
                    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid as u32) };
                    if handle.is_null() {
                        let error = unsafe { GetLastError() };
                        // An already-exited process can disappear between query and open.
                        if error == ERROR_INVALID_PARAMETER {
                            continue;
                        }
                        return Err(format!("could not pin CLI job process: Win32 error {error}"));
                    }
                    handles.push(OwnedHandle(handle));
                }
                return Ok(handles);
            }
        }

        fn wait_empty_until(&self, processes: &[OwnedHandle], deadline: Instant) -> Result<bool, String> {
            wait_stopped_until(processes, deadline, || self.is_empty())
        }

        fn terminate(&self) -> Result<(), String> {
            if self.0.0.is_null() {
                return Err("job handle was already closed".into());
            }
            if unsafe { TerminateJobObject(self.0.0, 1) } != 0 {
                return Ok(());
            }
            let error = unsafe { GetLastError() };
            if self.is_empty()? {
                Ok(())
            } else {
                Err(format!(
                    "TerminateJobObject failed with Win32 error {error}"
                ))
            }
        }
    }

    fn open_primary_thread(process_id: u32) -> Result<OwnedHandle, String> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot.is_null() || snapshot as isize == -1 {
            return Err(format!(
                "CreateToolhelp32Snapshot failed with Win32 error {}",
                unsafe { GetLastError() }
            ));
        }
        let snapshot = OwnedHandle(snapshot);
        let mut entry = THREADENTRY32::default();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut matching_thread = None;
        let mut found = unsafe { Thread32First(snapshot.0, &mut entry) };
        if found == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_MORE_FILES {
                return Err(format!(
                    "Thread32First failed with Win32 error {error}"
                ));
            }
        }
        while found != 0 {
            if entry.th32OwnerProcessID == process_id {
                if matching_thread.replace(entry.th32ThreadID).is_some() {
                    return Err(format!(
                        "suspended process {process_id} exposed more than one initial thread"
                    ));
                }
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            found = unsafe { Thread32Next(snapshot.0, &mut entry) };
            if found == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_NO_MORE_FILES {
                    return Err(format!(
                        "Thread32Next failed with Win32 error {error}"
                    ));
                }
            }
        }
        let thread_id = matching_thread
            .ok_or_else(|| format!("no initial thread found for suspended process {process_id}"))?;
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread.is_null() {
            return Err(format!(
                "OpenThread({thread_id}) failed with Win32 error {}",
                unsafe { GetLastError() }
            ));
        }
        Ok(OwnedHandle(thread))
    }

    pub(crate) struct CliProcessTree {
        child: Child,
        job: JobObject,
        assigned: bool,
        root_reaped: bool,
        finished: bool,
        #[cfg(test)]
        fail_next_wait_observation: bool,
    }

    enum SpawnFailure {
        Deadline(String),
        Failed(String),
    }

    impl SpawnFailure {
        fn operation(deadline: Instant, stage: &str, error: impl std::fmt::Display) -> Self {
            let message = format!("{stage}: {error}");
            if Instant::now() >= deadline {
                Self::Deadline(format!("deadline expired during {message}"))
            } else {
                Self::Failed(message)
            }
        }
    }

    impl CliProcessTree {
        /// Start a background task suspended, assign it to a kill-on-close job, then resume it.
        pub(crate) fn spawn_task(command: Command, timeout: Duration) -> Result<Self, String> {
            let deadline = Instant::now().checked_add(timeout).ok_or("task timeout is out of range")?;
            Self::spawn(command, deadline).map_err(|failure| match failure {
                SpawnFailure::Deadline(message) | SpawnFailure::Failed(message) => message,
            })
        }

        pub(crate) fn child_mut(&mut self) -> &mut Child { &mut self.child }

        pub(crate) fn stop_task(&mut self) -> Result<(), String> {
            self.terminate_and_reap()
        }

        fn spawn(mut command: Command, deadline: Instant) -> Result<Self, SpawnFailure> {
            if Instant::now() >= deadline {
                return Err(SpawnFailure::Deadline(
                    "deadline expired before creating the Windows job".into(),
                ));
            }
            let job = JobObject::new().map_err(|error| {
                SpawnFailure::operation(deadline, "creating Windows job", error)
            })?;
            use std::os::windows::process::CommandExt;
            // command_without_console set CREATE_NO_WINDOW; include it again while adding
            // CREATE_SUSPENDED so CommandExt's replacement semantics cannot clear that flag.
            command.creation_flags(CREATE_NO_WINDOW_FLAG | CREATE_SUSPENDED);
            let child = command
                .spawn()
                .map_err(|error| SpawnFailure::operation(deadline, "CreateProcess failed", error))?;
            let mut tree = Self {
                child,
                job,
                assigned: false,
                root_reaped: false,
                finished: false,
                #[cfg(test)]
                fail_next_wait_observation: false,
            };
            let setup = (|| {
                if Instant::now() >= deadline {
                    return Err(SpawnFailure::Deadline(
                        "deadline expired during suspended process creation".to_string(),
                    ));
                }
                let thread = open_primary_thread(tree.child.id()).map_err(|error| {
                    SpawnFailure::operation(deadline, "opening suspended primary thread", error)
                })?;
                if unsafe { AssignProcessToJobObject(tree.job.0.0, tree.child.as_raw_handle() as HANDLE) }
                    == 0
                {
                    let error = format!(
                        "AssignProcessToJobObject failed with Win32 error {}",
                        unsafe { GetLastError() }
                    );
                    return Err(SpawnFailure::operation(deadline, "assigning process to job", error));
                }
                tree.assigned = true;
                let previous_suspend_count = unsafe { ResumeThread(thread.0) };
                if previous_suspend_count == u32::MAX {
                    let error = format!(
                        "ResumeThread failed with Win32 error {}",
                        unsafe { GetLastError() }
                    );
                    return Err(SpawnFailure::operation(deadline, "resuming contained process", error));
                }
                if previous_suspend_count != 1 {
                    return Err(SpawnFailure::operation(
                        deadline,
                        "resuming contained process",
                        format!("ResumeThread returned unexpected suspend count {previous_suspend_count}"),
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(SpawnFailure::Deadline(
                        "deadline expired immediately after resuming the contained process".into(),
                    ));
                }
                Ok(())
            })();
            if let Err(mut failure) = setup {
                let cleanup = tree.terminate_and_reap().err();
                if let Some(cleanup) = cleanup {
                    match &mut failure {
                        SpawnFailure::Deadline(message) | SpawnFailure::Failed(message) => {
                            message.push_str(&format!("; cleanup was not confirmed: {cleanup}"));
                        }
                    }
                }
                return Err(failure);
            }
            Ok(tree)
        }

        fn take_stdin(&mut self) -> Option<ChildStdin> {
            self.child.stdin.take()
        }

        fn try_wait_root(&mut self) -> Result<Option<ExitStatus>, String> {
            #[cfg(test)]
            if self.fail_next_wait_observation {
                self.fail_next_wait_observation = false;
                return Err("injected one-shot root wait observation error".into());
            }
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.root_reaped = true;
                    Ok(Some(status))
                }
                Ok(None) => Ok(None),
                Err(error) => Err(format!("could not observe CLI root process: {error}")),
            }
        }

        #[cfg(test)]
        fn spawn_unassigned_for_test(mut command: Command) -> Result<Self, String> {
            let job = JobObject::new()?;
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW_FLAG | CREATE_SUSPENDED);
            let child = command
                .spawn()
                .map_err(|error| format!("could not spawn the test-owned suspended child: {error}"))?;
            Ok(Self {
                child,
                job,
                assigned: false,
                root_reaped: false,
                finished: false,
                fail_next_wait_observation: false,
            })
        }

        #[cfg(test)]
        fn inject_wait_observation_error_for_test(&mut self) {
            self.fail_next_wait_observation = true;
        }

        fn finish_root_exit(&mut self, status: ExitStatus) -> Result<ExitStatus, BackendError> {
            let cleanup = self.terminate_and_reap();
            match cleanup {
                Ok(()) => Ok(status),
                Err(error) => {
                    let force_close = self.force_close_job().err();
                    let details = force_close
                        .map(|detail| format!("; force-close cleanup was not confirmed: {detail}"))
                        .unwrap_or_default();
                    Err(BackendError::Failed(format!(
                        "CLI root exited but the contained process tree could not be confirmed stopped: {error}{details}"
                    )))
                }
            }
        }

        fn terminate_and_reap(&mut self) -> Result<(), String> {
            if self.finished {
                return Ok(());
            }
            let cleanup_deadline = Instant::now()
                .checked_add(CLEANUP_GRACE)
                .ok_or_else(|| "cleanup deadline is out of range".to_string())?;
            if self.assigned {
                let processes = self.job.process_handles()?;
                if !self.job.is_empty()? {
                    self.job.terminate()?;
                }
                if !self.job.wait_empty_until(&processes, cleanup_deadline)? {
                    return Err("Windows job still had live processes after the cleanup grace".into());
                }
            } else if !self.root_reaped {
                let first_observation = self.try_wait_root();
                self.terminate_unassigned_child(first_observation, cleanup_deadline)?;
            }
            if !self.root_reaped {
                loop {
                    match self.child.try_wait() {
                        Ok(Some(_)) => {
                            self.root_reaped = true;
                            break;
                        }
                        Ok(None) if Instant::now() < cleanup_deadline => {
                            let now = Instant::now();
                            std::thread::sleep(
                                cleanup_deadline.saturating_duration_since(now).min(POLL_INTERVAL),
                            );
                        }
                        Ok(None) => return Err("CLI root process was not reaped before cleanup deadline".into()),
                        Err(error) => return Err(format!("could not reap CLI child: {error}")),
                    }
                }
            }
            self.finished = true;
            Ok(())
        }

        fn terminate_unassigned_child(
            &mut self,
            first_observation: Result<Option<ExitStatus>, String>,
            cleanup_deadline: Instant,
        ) -> Result<(), String> {
            let mut issues = Vec::new();
            match first_observation {
                Ok(Some(_)) => {
                    self.root_reaped = true;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => issues.push(format!("root wait observation failed: {error}")),
            }

            if let Err(error) = self.child.kill() {
                issues.push(format!("could not terminate unassigned suspended CLI child: {error}"));
            }

            let mut reap_observation_error = None;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.root_reaped = true;
                        break;
                    }
                    Ok(None) if Instant::now() < cleanup_deadline => {}
                    Ok(None) => {
                        issues.push("unassigned CLI child was not reaped before cleanup deadline".into());
                        return Err(issues.join("; "));
                    }
                    Err(error) if Instant::now() < cleanup_deadline => {
                        reap_observation_error.get_or_insert_with(|| error.to_string());
                    }
                    Err(error) => {
                        issues.push(format!("could not reap unassigned CLI child: {error}"));
                        return Err(issues.join("; "));
                    }
                }
                let now = Instant::now();
                if now >= cleanup_deadline {
                    continue;
                }
                std::thread::sleep(cleanup_deadline.saturating_duration_since(now).min(POLL_INTERVAL));
            }

            if let Some(error) = reap_observation_error {
                issues.push(format!("root wait observation failed while reaping: {error}"));
            }
            if issues.is_empty() {
                Ok(())
            } else {
                issues.push("unassigned CLI child was terminated and reaped".into());
                Err(issues.join("; "))
            }
        }

        fn force_close_job(&mut self) -> Result<(), String> {
            // KILL_ON_JOB_CLOSE is the final fallback when explicit termination/wait failed.
            // Close the job before joining the stdin writer so every inherited pipe reader is
            // asked to exit and the synchronous write can unblock.
            self.job.0.close();
            let mut issues = Vec::new();
            match self.try_wait_root() {
                Ok(Some(_)) => {
                    self.root_reaped = true;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => issues.push(format!("could not observe CLI root after closing job: {error}")),
            }
            if let Err(error) = self.child.kill() {
                issues.push(format!("could not terminate CLI root after closing job: {error}"));
            }
            let deadline = Instant::now()
                .checked_add(CLEANUP_GRACE)
                .ok_or_else(|| "force-close deadline is out of range".to_string())?;
            let mut reap_observation_error = None;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.root_reaped = true;
                        if let Some(error) = reap_observation_error {
                            issues.push(format!("could not observe CLI root while reaping: {error}"));
                        }
                        if issues.is_empty() {
                            return Ok(());
                        }
                        issues.push("CLI root was terminated and reaped".into());
                        return Err(issues.join("; "));
                    }
                    Ok(None) if Instant::now() < deadline => {}
                    Ok(None) => {
                        issues.push("CLI root remained live after force-close cleanup grace".into());
                        return Err(issues.join("; "));
                    }
                    Err(error) if Instant::now() < deadline => {
                        reap_observation_error.get_or_insert_with(|| error.to_string());
                    }
                    Err(error) => {
                        issues.push(format!("could not reap CLI root after force-close: {error}"));
                        return Err(issues.join("; "));
                    }
                }
                let now = Instant::now();
                if now < deadline {
                    std::thread::sleep(deadline.saturating_duration_since(now).min(POLL_INTERVAL));
                } else {
                    issues.push("force-close cleanup deadline elapsed without observing CLI root exit".into());
                    return Err(issues.join("; "));
                }
            }
        }
    }

    impl Drop for CliProcessTree {
        fn drop(&mut self) {
            if self.finished {
                return;
            }
            if self.terminate_and_reap().is_err() {
                let _ = self.force_close_job();
            }
        }
    }

    #[test]
    fn isolated_empty_job_accounting_requires_signaled_process_handles() {
        use windows_sys::Win32::System::Threading::OpenProcess;
        let command = crate::subprocess::command_without_console(std::env::current_exe().unwrap());
        let mut tree = CliProcessTree::spawn_unassigned_for_test(command).unwrap();
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, tree.child.id()) };
        assert!(!handle.is_null());
        let handles = [OwnedHandle(handle)];
        // Force the observed interleaving: accounting says zero while the pinned process
        // handle is still unsignaled. A suspended child makes this deterministic.
        let stopped = wait_stopped_until(&handles, Instant::now(), || Ok(true)).unwrap();
        tree.terminate_and_reap().unwrap();
        assert!(!stopped, "empty accounting alone must not report a live process stopped");
        assert!(wait_stopped_until(&handles, Instant::now(), || Ok(true)).unwrap());
    }

    #[cfg(test)]
    pub(super) fn cleanup_unassigned_after_wait_error_for_test(
        force_close: bool,
    ) -> Result<(String, bool), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate the test executable: {error}"))?;
        let mut command = crate::subprocess::command_without_console(executable);
        command
            .args([
                "--ignored",
                "--exact",
                "backend::tests::isolated_cli_lifecycle_probe",
                "--nocapture",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut tree = CliProcessTree::spawn_unassigned_for_test(command)?;
        if !matches!(tree.child.try_wait(), Ok(None)) {
            return Err("test-owned suspended process was not alive before cleanup".into());
        }
        tree.inject_wait_observation_error_for_test();
        let cleanup = if force_close {
            tree.force_close_job()
        } else {
            tree.terminate_and_reap()
        };
        let diagnostic = cleanup
            .err()
            .ok_or_else(|| "injected wait error was not preserved by cleanup".to_string())?;
        let reaped = matches!(tree.child.try_wait(), Ok(Some(_)));
        Ok((diagnostic, reaped))
    }

    fn join_writer<'scope>(
        writer: ScopedJoinHandle<'scope, Result<(), String>>,
    ) -> Result<(), String> {
        writer
            .join()
            .map_err(|_| "stdin writer thread panicked".to_string())?
    }

    fn abort_and_join<'scope>(
        tree: &mut CliProcessTree,
        writer: Option<ScopedJoinHandle<'scope, Result<(), String>>>,
        reason: String,
    ) -> BackendError {
        let cleanup_error = tree.terminate_and_reap().err();
        let force_close_error = cleanup_error
            .as_ref()
            .and_then(|_| tree.force_close_job().err());
        let writer_error = writer.and_then(|writer| join_writer(writer).err());
        let details = cleanup_error
            .into_iter()
            .map(|error| format!("process-tree cleanup failed: {error}"))
            .chain(force_close_error.map(|error| format!("force-close cleanup failed: {error}")))
            .chain(writer_error.map(|error| format!("stdin writer cleanup failed: {error}")))
            .collect::<Vec<_>>();
        if details.is_empty() {
            BackendError::Failed(reason)
        } else {
            BackendError::Failed(format!("{reason}; cleanup detail: {}", details.join("; ")))
        }
    }

    pub(super) fn run(
        command: Command,
        prompt: &str,
        timeout_s: u64,
        command_name: &str,
    ) -> Result<ExitStatus, BackendError> {
        let timeout = Duration::from_secs(timeout_s.max(5).min(3600));
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| BackendError::Failed("isolated CLI timeout is out of range".into()))?;
        std::thread::scope(|scope| {
            let mut tree = CliProcessTree::spawn(command, deadline).map_err(|failure| match failure {
                SpawnFailure::Deadline(detail) => {
                    BackendError::Failed(format!("{command_name} 超时（{timeout_s}s）：{detail}"))
                }
                SpawnFailure::Failed(error) => BackendError::Failed(format!(
                    "启动隔离 {command_name} 失败：{error}"
                )),
            })?;
            if Instant::now() >= deadline {
                return Err(abort_and_join(
                    &mut tree,
                    None,
                    format!("{command_name} 超时（{timeout_s}s）"),
                ));
            }
            let Some(mut stdin) = tree.take_stdin() else {
                return Err(abort_and_join(
                    &mut tree,
                    None,
                    format!("{command_name} 没有可用的标准输入管道"),
                ));
            };
            let writer = std::thread::Builder::new()
                .name("isolated-cli-stdin".into())
                .spawn_scoped(scope, move || {
                    let result = stdin
                        .write_all(prompt.as_bytes())
                        .map_err(|error| error.to_string());
                    drop(stdin);
                    result
                });
            let mut writer = match writer {
                Ok(writer) => Some(writer),
                Err(error) => {
                    return Err(abort_and_join(
                        &mut tree,
                        None,
                        format!("启动 {command_name} 标准输入写入线程失败：{error}"),
                    ))
                }
            };

            loop {
                if Instant::now() >= deadline {
                    return Err(abort_and_join(
                        &mut tree,
                        writer.take(),
                        format!("{command_name} 超时（{timeout_s}s）"),
                    ));
                }
                if writer.as_ref().is_some_and(|writer| writer.is_finished()) {
                    let result = join_writer(writer.take().expect("finished writer handle"));
                    if let Err(error) = result {
                        return Err(abort_and_join(
                            &mut tree,
                            None,
                            format!("写入 {command_name} 标准输入失败：{error}"),
                        ));
                    }
                }
                match tree.try_wait_root() {
                    Ok(Some(status)) => {
                        if writer.is_some() {
                            let cleanup_error = tree.terminate_and_reap().err();
                            let force_close_error = cleanup_error
                                .as_ref()
                                .and_then(|_| tree.force_close_job().err());
                            let writer_error = writer.take().and_then(|writer| join_writer(writer).err());
                            let mut details = Vec::new();
                            if let Some(error) = cleanup_error {
                                details.push(format!("process-tree cleanup failed: {error}"));
                            }
                            if let Some(error) = force_close_error {
                                details.push(format!("force-close cleanup failed: {error}"));
                            }
                            if let Some(error) = writer_error {
                                details.push(format!("prompt input was incomplete: {error}"));
                            }
                            if !details.is_empty() {
                                return Err(BackendError::Failed(format!(
                                    "{command_name} exited before isolated stdin delivery completed; {}",
                                    details.join("; ")
                                )));
                            }
                        }
                        return tree.finish_root_exit(status);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(abort_and_join(
                            &mut tree,
                            writer.take(),
                            format!("等待 {command_name} 根进程失败：{error}"),
                        ))
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(POLL_INTERVAL));
            }
        })
    }
}

/// Output captures are unique candidate-root files and are removed on every exit path, including
/// spawn errors, stdin failures, wait failures, and timeout. This also prevents stale plaintext
/// prompts or model output from surviving a failed CLI attempt.
struct CliOutputFiles {
    context: RuntimeContext,
    isolated: bool,
    stdout: PathBuf,
    stderr: PathBuf,
    stdout_created: bool,
    stderr_created: bool,
}

impl CliOutputFiles {
    fn new(context: &RuntimeContext, stdout: PathBuf, stderr: PathBuf) -> Result<Self, BackendError> {
        Ok(Self {
            context: context.clone(),
            isolated: context.is_isolated(),
            stdout,
            stderr,
            stdout_created: false,
            stderr_created: false,
        })
    }

    fn open_stdout(&mut self) -> Result<Stdio, BackendError> {
        let capture = self.open_capture(&self.stdout)?;
        self.stdout_created = true;
        Ok(capture)
    }

    fn open_stderr(&mut self) -> Result<Stdio, BackendError> {
        let capture = self.open_capture(&self.stderr)?;
        self.stderr_created = true;
        Ok(capture)
    }

    fn open_capture(&self, path: &std::path::Path) -> Result<Stdio, BackendError> {
        if self.isolated {
            let relative = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                BackendError::Failed("runtime CLI capture path is unavailable".into())
            })?;
            let relative = format!("logs/{relative}");
            let checked = self
                .context
                .resolve_path(RuntimeArea::Logs, &relative, PathIntent::WriteFile)
                .map_err(|error| BackendError::Failed(error.into()))?;
            if checked != path {
                return Err(BackendError::Failed("runtime CLI capture path is unavailable".into()));
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(Stdio::from)
                .map_err(|error| BackendError::Failed(format!("创建候选 CLI 输出文件失败：{error}")))
        } else {
            std::fs::File::create(path)
                .map(Stdio::from)
                .map_err(|error| BackendError::Failed(format!("创建临时输出文件失败 {}: {error}", path.display())))
        }
    }

    fn read_texts(&self) -> Result<(String, String), BackendError> {
        if self.isolated {
            let read = |path: &PathBuf| {
                let relative = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                    BackendError::Failed("runtime CLI capture path is unavailable".into())
                })?;
                self.context
                    .read_text(RuntimeArea::Logs, &format!("logs/{relative}"))
                    .map_err(|error| BackendError::Failed(error.into()))
            };
            Ok((read(&self.stdout)?, read(&self.stderr)?))
        } else {
            Ok((
                std::fs::read_to_string(&self.stdout).unwrap_or_default(),
                std::fs::read_to_string(&self.stderr).unwrap_or_default(),
            ))
        }
    }
}

impl Drop for CliOutputFiles {
    fn drop(&mut self) {
        for (path, created) in [
            (&self.stdout, self.stdout_created),
            (&self.stderr, self.stderr_created),
        ] {
            if !created {
                continue;
            }
            if self.isolated {
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    let _ = self.context.remove_file(RuntimeArea::Logs, &format!("logs/{name}"));
                }
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Variables that name a user profile, credential source, or model gateway are owned by the
/// selected runtime in isolated mode. Candidate `backends[].env` remains available for explicit
/// tool-specific settings, including an explicitly selected API key, but cannot redirect those
/// protected paths or replace the fake model endpoint.
fn isolated_environment_key(key: &str) -> bool {
    matches!(
        key.to_ascii_uppercase().as_str(),
        "HOME"
            | "USERPROFILE"
            | "DSH_HOME"
            | "DSH_CREDENTIALS"
            | "APPDATA"
            | "LOCALAPPDATA"
            | "XDG_CONFIG_HOME"
            | "XDG_CACHE_HOME"
            | "XDG_DATA_HOME"
            | "TEMP"
            | "TMP"
            | "ANTHROPIC_BASE_URL"
            | "OPENAI_BASE_URL"
            | "HER_RUNTIME_ROOT"
            | "PATH"
            | "PATHEXT"
            | "SYSTEMROOT"
            | "WINDIR"
            | "COMSPEC"
            | "SYSTEMDRIVE"
    )
}

fn selected_dependency_url(context: &RuntimeContext, value: &str) -> bool {
    ["dsh", "model", "onebot", "onebot_ws"]
        .iter()
        .filter_map(|name| context.dependency_url(name))
        .any(|selected| value == selected)
        || context
            .dependency_url("model")
            .is_some_and(|model| value == format!("{}/v1", model.trim_end_matches('/')))
}

fn path_is_within_runtime(context: &RuntimeContext, value: &str) -> bool {
    let Some(root) = context.runtime_root() else { return false };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return false;
    }
    let root = match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let candidate = match std::fs::canonicalize(&path) {
        Ok(path) => path,
        Err(_) => {
            let Some(file_name) = path.file_name() else { return false };
            let Some(parent) = path.parent().and_then(|parent| std::fs::canonicalize(parent).ok()) else {
                return false;
            };
            parent.join(file_name)
        }
    };
    candidate.starts_with(root)
}

fn argument_path(value: &str) -> Option<&str> {
    let value = value.trim().trim_matches(|ch| ch == '"' || ch == '\'');
    if let Some((_, assigned)) = value.split_once('=') {
        let assigned = assigned.trim().trim_matches(|ch| ch == '"' || ch == '\'');
        if !assigned.is_empty() {
            return Some(assigned);
        }
    }
    Some(value)
}

fn is_windows_command_switch(value: &str) -> bool {
    // `cmd.exe /C <script>` is a supported legacy CLI invocation. Its slash-prefixed switches
    // are not filesystem paths and must not be canonicalized as drive-root paths on Windows.
    cfg!(windows)
        && ["/c", "/d", "/k", "/s"]
            .iter()
            .any(|switch| value.eq_ignore_ascii_case(switch))
}

fn contains_parent_component(value: &str) -> bool {
    value
        .split(['/', '\\'])
        .any(|component| component == "..")
}

fn validate_isolated_cli_settings(
    context: &RuntimeContext,
    args: &[String],
    env: &[(String, String)],
) -> Result<(), String> {
    for value in args {
        let value = argument_path(value).unwrap_or_default();
        if is_windows_command_switch(value) {
            continue;
        }
        if value.contains("://") && !selected_dependency_url(context, value) {
            return Err("isolated CLI arguments may only use selected runtime dependency URLs".into());
        }
        if contains_parent_component(value) {
            return Err("isolated CLI arguments cannot traverse outside the selected runtime".into());
        }
        if PathBuf::from(value).is_absolute() && !path_is_within_runtime(context, value) {
            return Err("isolated CLI arguments cannot reference paths outside the selected runtime".into());
        }
    }
    for (key, value) in env {
        if isolated_environment_key(key) {
            return Err(format!("isolated backend cannot override selected runtime environment variable {key}"));
        }
        if value.contains("://") && !selected_dependency_url(context, value) {
            return Err(format!("isolated backend environment variable {key} must use a selected runtime dependency"));
        }
        let key_upper = key.to_ascii_uppercase();
        if (key_upper.contains("PATH") || key_upper.contains("FILE") || key_upper.contains("DIR"))
            && PathBuf::from(value).is_absolute()
            && !path_is_within_runtime(context, value)
        {
            return Err(format!("isolated backend environment variable {key} cannot reference paths outside the selected runtime"));
        }
        if contains_parent_component(value) && (key_upper.contains("PATH") || key_upper.contains("FILE") || key_upper.contains("DIR")) {
            return Err(format!("isolated backend environment variable {key} cannot traverse outside the selected runtime"));
        }
    }
    Ok(())
}

fn configure_isolated_cli_environment(
    command: &mut Command,
    context: &RuntimeContext,
) -> Result<(), BackendError> {
    command.env_clear();
    let dsh_home = context
        .resolve_path(RuntimeArea::DshUser, "", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let appdata = context
        .resolve_path(RuntimeArea::DshUser, "AppData/Roaming", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let local_appdata = context
        .resolve_path(RuntimeArea::DshUser, "AppData/Local", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let xdg_config = context
        .resolve_path(RuntimeArea::DshUser, ".config", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let xdg_cache = context
        .resolve_path(RuntimeArea::DshUser, ".cache", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let xdg_data = context
        .resolve_path(RuntimeArea::DshUser, ".local/share", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let temp = context
        .resolve_path(RuntimeArea::Data, "tmp", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;
    let cwd = context
        .resolve_path(RuntimeArea::Resources, "", PathIntent::CreateDirectory)
        .map_err(|error| BackendError::Failed(error.into()))?;

    // Keep only host execution primitives. User profiles, app-data directories, credentials,
    // shell startup files, and model URLs are never inherited from the parent process.
    for key in ["PATH", "PATHEXT", "SystemRoot", "WINDIR", "COMSPEC", "SystemDrive"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let ordinary = |path: &PathBuf| crate::subprocess::external_path(path).map_err(BackendError::Config);
    command
        .env("HOME", ordinary(&dsh_home)?)
        .env("USERPROFILE", ordinary(&dsh_home)?)
        .env("DSH_HOME", ordinary(&dsh_home)?)
        .env("DSH_CREDENTIALS", ordinary(&dsh_home.join(".credentials.yaml"))?)
        .env("APPDATA", ordinary(&appdata)?)
        .env("LOCALAPPDATA", ordinary(&local_appdata)?)
        .env("XDG_CONFIG_HOME", ordinary(&xdg_config)?)
        .env("XDG_CACHE_HOME", ordinary(&xdg_cache)?)
        .env("XDG_DATA_HOME", ordinary(&xdg_data)?)
        .env("TEMP", ordinary(&temp)?)
        .env("TMP", ordinary(&temp)?)
        .current_dir(ordinary(&cwd)?);
    if let Some(root) = context.runtime_root() {
        command.env("HER_RUNTIME_ROOT", crate::subprocess::external_path(root).map_err(BackendError::Config)?);
    }
    Ok(())
}

/// 从 DSH 的凭据文件里取 opencode-go 的 key，展开成各 CLI 认的变量名。
///
/// 为什么不让每个后端在配置里写 key：那样 key 会出现在 config.json 里 —— 会被提交、
/// 可能被日志打印、轮换时要改好几处。这里只保留**一处真相**：
/// `~/.dsh/.credentials.yaml` 里的 `OPENCODE_GO_API_KEY`。
///
/// 读不到就返回空表，**不报错**：没配 key 的机器上，`--backends` 这类只读用法
/// 不该因为缺 key 直接失败。
pub fn credential_env() -> Vec<(String, String)> {
    let path = match std::env::var("DSH_CREDENTIALS") {
        Ok(path) => path,
        // Without an explicit path or USERPROFILE there is no credential file to read.
        Err(_) => match std::env::var_os("USERPROFILE") {
            Some(home) => std::path::PathBuf::from(home)
                .join(".dsh")
                .join(".credentials.yaml")
                .to_string_lossy()
                .into_owned(),
            None => return Vec::new(),
        },
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    credential_env_from_text(&text)
}

fn credential_env_from_text(text: &str) -> Vec<(String, String)> {
    let Some(key) = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("OPENCODE_GO_API_KEY:"))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|k| !k.is_empty())
    else {
        return Vec::new();
    };
    vec![
        ("OPENCODE_GO_API_KEY".into(), key.clone()),
        ("OPENCODE_API_KEY".into(), key.clone()),
        ("ANTHROPIC_AUTH_TOKEN".into(), key.clone()),
        ("ANTHROPIC_API_KEY".into(), key.clone()),
        ("OPENAI_API_KEY".into(), key),
        (
            "ANTHROPIC_BASE_URL".into(),
            "https://opencode.ai/inference/go".into(),
        ),
        (
            "OPENAI_BASE_URL".into(),
            "https://opencode.ai/inference/go/v1".into(),
        ),
    ]
}

pub fn credential_env_for_runtime(context: &RuntimeContext) -> Vec<(String, String)> {
    if !context.is_isolated() {
        return credential_env();
    }
    let Ok(text) = context.read_text(RuntimeArea::DshUser, ".credentials.yaml") else {
        return Vec::new();
    };
    credential_env_from_text(&text)
}

/// 按 id 跑指定的 CLI 后端（会话级后端覆盖用）。
///
/// 和 `cli_fallback` 的区别：那个按配置顺序挨个试，这个只要一个 id。
/// 找不到 / 不可用 / 跑失败都回 Err，调用方据此回落到 DSH 候选链。
pub fn cli_fallback_by_id(
    cfg: &Value,
    id: &str,
    key: &str,
    prompt: &str,
    timeout_s: u64,
) -> Result<(String, String), String> {
    cli_fallback_by_id_with_runtime(&RuntimeContext::legacy(), cfg, id, key, prompt, timeout_s)
}

pub fn cli_fallback_by_id_with_runtime(
    context: &RuntimeContext,
    cfg: &Value,
    id: &str,
    key: &str,
    prompt: &str,
    timeout_s: u64,
) -> Result<(String, String), String> {
    let reg = Registry::from_config_with_runtime(context, cfg).map_err(|e| e.message().to_string())?;
    let target = reg
        .chain
        .iter()
        .find(|b| b.id() == id)
        .ok_or_else(|| format!("backends 里没有 id={id} 的后端"))?;
    if target.kind() != "cli" {
        return Err(format!("后端 {id} 的 kind={} 不是 cli", target.kind()));
    }
    if !target.available() {
        return Err(format!("后端 {id} 不可用"));
    }
    let req = BackendRequest {
        key: key.to_string(),
        prompt: prompt.to_string(),
        timeout_s,
    };
    target
        .ask(&req)
        .map(|r| (id.to_string(), r.text))
        .map_err(|e| e.message().to_string())
}

/// 该会话的 agentToken，展开成 MCP 服务器认的环境变量。
///
/// 为什么要从文件读而不是让模型传：CLI 后端的模型不知道会话身份（它没有桥接的上下文），
/// 实测让它传 token 会得到「未授权」。这里从 social-v2.json 取，随调用注入。
/// 读不到就返回空表 —— 不报错：没有会话的后端（如 probe）本来就不需要。
pub fn session_env(key: &str) -> Vec<(String, String)> {
    session_env_for_runtime(&RuntimeContext::legacy(), key)
}

pub fn session_env_for_runtime(context: &RuntimeContext, key: &str) -> Vec<(String, String)> {
    if key.trim().is_empty() {
        return Vec::new();
    }
    let text = if context.is_isolated() {
        context.read_text(RuntimeArea::Data, "social-v2.json").ok()
    } else {
        std::fs::read_to_string("C:/qqbot/qq-bridge/state/social-v2.json").ok()
    };
    let Some(text) = text else { return Vec::new() };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let token = v
        .get("conversations")
        .and_then(|c| c.get(key))
        .and_then(|s| s.get("agentToken"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        return Vec::new();
    }
    vec![
        ("QQ_SESSION_KEY".to_string(), key.to_string()),
        ("QQ_SESSION_TOKEN".to_string(), token.to_string()),
    ]
}

/// 找到命令的真实路径（Windows 上 `opencode` 实际是 `opencode.cmd`）。
///
/// 为什么必须自己解析：`std::process::Command::new("opencode")` 在 Windows 上
/// **不会**按 PATHEXT 去找 .cmd/.ps1 —— 它只认 .exe，于是 npm 装的 CLI 一律
/// 报 `program not found`。实测就是这么翻车的：`--backends` 说 ✅，`--backend-ask`
/// 却说启动失败。**"在不在 PATH 里"和"能不能启动"是两回事。**
pub fn resolve_command(cmd: &str) -> Option<std::path::PathBuf> {
    if cmd.contains('/') || cmd.contains('\\') {
        let p = std::path::Path::new(cmd);
        return if p.is_file() { Some(p.to_path_buf()) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    let explicit_extension = std::path::Path::new(cmd).extension().is_some();
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(|s| s.to_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        if explicit_extension {
            let candidate = dir.join(cmd);
            if candidate.is_file() {
                return Some(candidate);
            }
            continue;
        }
        for ext in &exts {
            let candidate = dir.join(format!("{cmd}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 命令是否在 PATH 上（Windows 上还要认 .cmd/.bat/.exe）。
pub fn command_in_path(cmd: &str) -> bool {
    resolve_command(cmd).is_some()
}

/// 后端注册表：按配置顺序组成候选链。
pub struct Registry {
    pub chain: Vec<Box<dyn Backend>>,
}

impl Registry {
    /// 从 `config.json` 的 `backends` 段构建。
    ///
    /// 配置形态（两种都支持，保持向后兼容）：
    ///   "backends": {"default": "dsh", "list": [{"id": "...", "kind": "cli", "cmd": "..."}]}
    ///   "backends": [{"id": "...", ...}]
    /// 没配时回落到单元素 ["dsh"]。
    pub fn from_config(cfg: &Value) -> Result<Self, BackendError> {
        Self::from_config_with_runtime(&RuntimeContext::legacy(), cfg)
    }

    pub fn from_config_with_runtime(
        context: &RuntimeContext,
        cfg: &Value,
    ) -> Result<Self, BackendError> {
        let section = cfg.get("backends");
        let list: Vec<Value> = match section {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::Object(o)) => o
                .get("list")
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if list.is_empty() {
            return Ok(Self {
                chain: vec![Box::new(DshBackend { id: "dsh".into(), model: String::new() })],
            });
        }
        let mut chain: Vec<Box<dyn Backend>> = Vec::new();
        for item in list {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                return Err(BackendError::Config("backends 里有条目缺少 id".into()));
            }
            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("cli");
            match kind {
                "dsh" => chain.push(Box::new(DshBackend {
                    id,
                    model: item.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                })),
                "cli" => chain.push(Box::new(CliBackend::from_config_with_runtime(context, &id, &item)?)),
                other => {
                    return Err(BackendError::Config(format!(
                        "后端 {id} 的 kind={other} 不支持（只认 dsh / cli）"
                    )))
                }
            }
        }
        Ok(Self { chain })
    }

    pub fn ids(&self) -> Vec<String> {
        self.chain.iter().map(|b| b.id().to_string()).collect()
    }

    pub fn describe(&self) -> Value {
        Value::Array(self.chain.iter().map(|b| b.describe()).collect())
    }
}

/// DSH 后端：真正的实现在 `pipeline` 里（RPC + WS 事件流）。
/// 这里只做一层薄适配，避免在抽象层里重写一遍传输逻辑。
pub struct DshBackend {
    pub id: String,
    pub model: String,
}

impl Backend for DshBackend {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "dsh"
    }
    fn available(&self) -> bool {
        // DSH 是本地服务，真正的可达性检查在 pipeline 里；这里不当瓶颈。
        true
    }
    fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "kind": "dsh",
            "model": self.model,
            "available": true,
        })
    }
    fn ask(&self, _req: &BackendRequest) -> Result<BackendReply, BackendError> {
        // 需要在 async 运行时里跑 RPC；trait 保持同步签名，所以这里明确拒绝，
        // 由调用方（pipeline::ask_with_failover）直接走 DSH 原生通道。
        Err(BackendError::Config(
            "DSH 后端请在 pipeline 里直连，不要经由同步 trait".into(),
        ))
    }
}

/// DSH 全挂之后，按 `backends` 配置依次试 CLI 后端。
///
/// **同步**函数，故意不写成 async：`Backend::ask` 本来就是"等子进程跑完"，
/// 但这条路径本来就是"DSH 全挂了"的兜底，阻塞一小会儿比给整个 Registry
/// 加 Send 约束划算。正常回合走不到这里。
pub fn cli_fallback(
    cfg: &Value,
    key: &str,
    prompt: &str,
    timeout_s: u64,
) -> Result<(String, String), String> {
    cli_fallback_with_runtime(&RuntimeContext::legacy(), cfg, key, prompt, timeout_s)
}

pub fn cli_fallback_with_runtime(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    prompt: &str,
    timeout_s: u64,
) -> Result<(String, String), String> {
    cli_fallback_with_runtime_reporting(context, cfg, key, prompt, timeout_s, |_, _| {})
}

/// Report each failed CLI candidate before its identity is lost in aggregation.
pub fn cli_fallback_with_runtime_reporting(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    prompt: &str,
    timeout_s: u64,
    mut report: impl FnMut(&str, &str),
) -> Result<(String, String), String> {
    // Opt-in only (ticket 25, user decision): a CLI backend may run on another account, so the DSH-failed fallback
    // never starts one unless `backends.failoverFallback` is true.
    if let Some(refusal) = crate::failover::cli_fallback_refusal(cfg) {
        return Err(refusal);
    }
    let reg = Registry::from_config_with_runtime(context, cfg).map_err(|e| e.message().to_string())?;
    let mut notes: Vec<String> = Vec::new();
    for b in reg.chain.iter() {
        if b.kind() != "cli" {
            continue; // dsh 候选上面已经试过了
        }
        if !b.available() {
            report(b.id(), "CLI backend unavailable");
            notes.push(format!("{} 不可用", b.id()));
            continue;
        }
        let id = b.id().to_string();
        let req = BackendRequest {
            key: key.to_string(),
            prompt: prompt.to_string(),
            timeout_s,
        };
        match b.ask(&req) {
            Ok(r) => return Ok((id, r.text)),
            Err(e) => {
                report(&id, e.message());
                notes.push(format!("{}：{}", id, e.message()));
            }
        }
    }
    Err(if notes.is_empty() {
        "没有配置任何 CLI 后端".to_string()
    } else {
        notes.join("；")
    })
}
#[cfg(test)]
mod tests {
    #[test]
    fn nonzero_cli_exit_keeps_sensitive_stdout_and_stderr_out_of_errors() {
        let mut command =
            crate::subprocess::command_without_console(if cfg!(windows) { "cmd" } else { "sh" });
        if cfg!(windows) {
            command.args(["/C", "echo CHAT_SECRET synthetic-nickname token-fixture & echo CHAT_SECRET synthetic-nickname token-fixture 1>&2 & exit /b 7"]);
        } else {
            command.args(["-c", "printf 'CHAT_SECRET synthetic-nickname token-fixture'; printf 'CHAT_SECRET synthetic-nickname token-fixture' >&2; exit 7"]);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(7));
        let error = decode_cli_exit(
            &String::from_utf8(output.stdout).unwrap(),
            &String::from_utf8(output.stderr).unwrap(),
            false,
            "fixture-cli",
            output.status.code(),
        )
        .unwrap_err();
        for private in ["CHAT_SECRET", "synthetic-nickname", "token-fixture"] {
            assert!(
                !error.message().contains(private),
                "CLI error leaked {private}: {}",
                error.message()
            );
        }
        assert!(error.message().contains("backend"));
        assert!(error.message().contains("fixture-cli"));
        assert!(error.message().contains('7'));
    }

    #[test]
    fn cli_output_contract_keeps_answers_and_classifies_content_free_failures() {
        for (text, structured, answer) in [
            ("ordinary safety discussion", false, "ordinary safety discussion"),
            (r#"{"response":"ordinary answer"}"#, true, "ordinary answer"),
            (r#"{"result":"Claude answer"}"#, false, "Claude answer"),
            (r#"{"text":"plain client answer"}"#, false, "plain client answer"),
        ] {
            assert_eq!(decode_cli_output(text, structured).unwrap(), answer);
        }
        for (text, structured, class) in [
            ("The prompt could not be submitted: Prohibited Use Policy CHAT_SECRET", false, "refusal"),
            (r#"{"error":{"code":"SAFETY","message":"CHAT_SECRET"}}"#, true, "refusal"),
            (r#"{"candidates":[]}"#, false, "refusal"),
            (r#"{"candidates":[{"finishReason":"SAFETY"}]}"#, false, "refusal"),
            ("Error: quota exhausted CHAT_SECRET", false, "quota"),
            ("HTTP 503 Service Unavailable\n<html>CHAT_SECRET</html>", false, "backend"),
            ("Error: timed out CHAT_SECRET", false, "timeout"),
            ("   ", false, "empty-response"),
            (r#"{"error":{"code":"ECONNRESET","message":"CHAT_SECRET"}}"#, false, "network"),
            ("Traceback (most recent call last):\nRuntimeError: CHAT_SECRET", false, "backend"),
            ("{\"type\":\"start\"}\n{\"type\":\"error\",\"error\":{\"code\":\"SAFETY\",\"message\":\"CHAT_SECRET\"}}", false, "refusal"),
            (r#"{"ok":false,"message":"CHAT_SECRET"}"#, false, "backend"),
            (r#"{"unknown":"CHAT_SECRET"}"#, false, "backend"),
            ("unstructured agy prose CHAT_SECRET", true, "backend"),
        ] {
            let error = decode_cli_output(text, structured).unwrap_err();
            let failure = crate::model_failure::Failure::classify("", error.message());
            assert_eq!(failure.class, class, "{text}");
            assert!(!error.message().contains("CHAT_SECRET"));
        }
    }

    #[test]
    fn agy_configuration_requests_json_without_changing_other_clients() {
        for args in [json!([]), json!(["--output-format","text"]), json!(["--output-format=stream-json","--model","fixture"])] {
            let value = json!({"cmd":"agy","args":args});
            let backend = CliBackend::from_config_with_runtime(&RuntimeContext::legacy(), "agy-test", &value).unwrap();
            assert_eq!(&backend.args[backend.args.len()-2..], &["--output-format", "json"]);
            assert_eq!(backend.args.iter().filter(|a| a.starts_with("--output-format")).count(), 1);
        }
        let value = json!({"cmd":"other","args":["--output-format","text"]});
        let backend = CliBackend::from_config_with_runtime(&RuntimeContext::legacy(), "other", &value).unwrap();
        assert_eq!(backend.args, vec!["--output-format", "text"]);
    }

    use super::*;

    #[test]
    fn registry_defaults_to_dsh() {
        let cfg = json!({});
        let reg = Registry::from_config(&cfg).unwrap();
        assert_eq!(reg.ids(), vec!["dsh".to_string()]);
    }

    #[test]
    fn registry_parses_object_and_array_forms() {
        let obj = json!({"backends": {"default": "oc", "list": [
            {"id": "oc", "kind": "cli", "cmd": "opencode", "args": ["run"]},
            {"id": "dsh", "kind": "dsh", "model": "deepseek-flash"}
        ]}});
        let reg = Registry::from_config(&obj).unwrap();
        assert_eq!(reg.ids(), vec!["oc".to_string(), "dsh".to_string()]);

        let arr = json!({"backends": [{"id": "a", "kind": "cli", "cmd": "x"}]});
        assert_eq!(Registry::from_config(&arr).unwrap().ids(), vec!["a".to_string()]);
    }

    #[test]
    fn unknown_kind_is_config_error_and_not_retryable() {
        let cfg = json!({"backends": [{"id": "x", "kind": "quantum"}]});
        let err = Registry::from_config(&cfg).err().expect("应该报配置错");
        assert!(!err.retryable(), "配置错不该被 failover 吞掉");
    }

    #[test]
    fn cli_backend_requires_cmd() {
        let cfg = json!({"backends": [{"id": "x", "kind": "cli"}]});
        let err = Registry::from_config(&cfg).err().expect("应该报缺少 cmd");
        assert!(err.message().contains("缺少 cmd"));
    }

    #[test]
    fn missing_command_is_unavailable_but_retryable() {
        let b = CliBackend {
            id: "nope".into(),
            cmd: "definitely-not-a-real-command-xyz".into(),
            args: vec![],
            env: vec![],
            context: RuntimeContext::legacy(),
        };
        assert!(!b.available());
        let err = b.ask(&BackendRequest { key: "k".into(), prompt: "hi".into(), timeout_s: 5 }).unwrap_err();
        assert!(matches!(err, BackendError::Unavailable(_)));
        assert!(err.retryable());
    }

    #[test]
    fn dsh_failed_fallback_runs_no_cli_backend_unless_enabled() {
        // The backend's command does not exist, so nothing can run; the error shows whether the backend was consulted.
        let mut cfg = json!({"backends": {"list": [{"id": "oc", "kind": "cli", "cmd": "definitely-not-a-real-command-xyz"}]}});
        let off = cli_fallback_with_runtime(&RuntimeContext::legacy(), &cfg, "group:1", "hi", 10).unwrap_err();
        assert!(off.contains("failoverFallback") && !off.contains("oc"), "off by default, backend not consulted: {off}");
        cfg["backends"]["failoverFallback"] = json!(false);
        assert!(cli_fallback_with_runtime(&RuntimeContext::legacy(), &cfg, "group:1", "hi", 10).unwrap_err().contains("failoverFallback"));
        cfg["backends"]["failoverFallback"] = json!(true);
        let on = cli_fallback_with_runtime(&RuntimeContext::legacy(), &cfg, "group:1", "hi", 10).unwrap_err();
        assert!(!on.contains("failoverFallback") && on.contains("oc"), "enabled: the configured backend is consulted: {on}");
    }

    #[test]
    fn command_in_path_finds_known_binary() {
        // Windows 上一定有 cmd.exe；类 Unix 上一定有 sh。
        let probe = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(command_in_path(probe), "{probe} 应该在 PATH 里");
        if cfg!(windows) {
            assert!(command_in_path("cmd.exe"), "cmd.exe 应该在 PATH 里");
            assert!(
                command_in_path("powershell.exe"),
                "powershell.exe 应该在 PATH 里"
            );
        }
        assert!(!command_in_path("definitely-not-a-real-command-xyz"));
    }

    #[cfg(windows)]
    #[test]
    fn isolated_cli_uses_selected_environment_and_cleans_timeout_captures() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        const PARENT_ONLY: &str = "QQBOT_TICKET04_PARENT_ONLY";

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        struct EnvRestore(Option<std::ffi::OsString>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                if let Some(value) = self.0.take() {
                    std::env::set_var(PARENT_ONLY, value);
                } else {
                    std::env::remove_var(PARENT_ONLY);
                }
            }
        }

        let worktree_root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let root = worktree_root
            .parent()
            .expect("manifest is inside the ticket worktree")
            .join("backups/ticket 04 CLI fixture 中文")
            .join(format!("fake-child-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let root = crate::install_root::development_root(root);
        let _fixture = Fixture(root.clone());
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources")).unwrap();
        std::fs::create_dir_all(root.join("user/dsh")).unwrap();
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "OPENCODE_GO_API_KEY: fixture-selected-api-key\n",
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let selected_root = context.runtime_root().unwrap().to_path_buf();
        let selected_dsh_root = context
            .resolve_path(RuntimeArea::DshUser, "", PathIntent::CreateDirectory)
            .unwrap();
        let selected_roaming = context
            .resolve_path(RuntimeArea::DshUser, "AppData/Roaming", PathIntent::CreateDirectory)
            .unwrap();
        let selected_local = context
            .resolve_path(RuntimeArea::DshUser, "AppData/Local", PathIntent::CreateDirectory)
            .unwrap();
        let selected_xdg = context
            .resolve_path(RuntimeArea::DshUser, ".config", PathIntent::CreateDirectory)
            .unwrap();
        let selected_xdg_cache = context
            .resolve_path(RuntimeArea::DshUser, ".cache", PathIntent::CreateDirectory)
            .unwrap();
        let selected_xdg_data = context
            .resolve_path(RuntimeArea::DshUser, ".local/share", PathIntent::CreateDirectory)
            .unwrap();
        let selected_temp = context
            .resolve_path(RuntimeArea::Data, "tmp", PathIntent::CreateDirectory)
            .unwrap();
        let selected_resources = context
            .resolve_path(RuntimeArea::Resources, "", PathIntent::ReadDirectory)
            .unwrap();
        // The child must see ordinary spellings; the context above remains canonical.
        let ordinary = |path: PathBuf| PathBuf::from(crate::subprocess::external_path(path).unwrap());
        let selected_root = ordinary(selected_root);
        let selected_dsh_root = ordinary(selected_dsh_root);
        let selected_roaming = ordinary(selected_roaming);
        let selected_local = ordinary(selected_local);
        let selected_xdg = ordinary(selected_xdg);
        let selected_xdg_cache = ordinary(selected_xdg_cache);
        let selected_xdg_data = ordinary(selected_xdg_data);
        let selected_temp = ordinary(selected_temp);
        let selected_resources = ordinary(selected_resources);
        let selected_resources_text = selected_resources.to_string_lossy();
        let selected_resources_without_extended_prefix = selected_resources_text
            .strip_prefix(r"\\?\")
            .unwrap_or(&selected_resources_text);
        let expected_reply = format!(
            concat!(
                "stdin=fixture stdin marker\r\n",
                "home={}\r\nprofile={}\r\nappdata={}\r\nlocalappdata={}\r\n",
                "xdg={}\r\nxdg_cache={}\r\nxdg_data={}\r\ntemp={}\r\ntmp={}\r\n",
                "inherited=\r\nexplicit=candidate-config-value\r\n",
                "credential=candidate-config-api-key\r\ndshcredential=fixture-selected-api-key\r\n",
                "model=http://127.0.0.1:43102/v1\r\nruntime_root={}\r\ncwd={}\r\n",
            ),
            selected_dsh_root.display(),
            selected_dsh_root.display(),
            selected_roaming.display(),
            selected_local.display(),
            selected_xdg.display(),
            selected_xdg_cache.display(),
            selected_xdg_data.display(),
            selected_temp.display(),
            selected_temp.display(),
            selected_root.display(),
            selected_resources_without_extended_prefix,
        );
        std::fs::write(root.join("resources/inspect-child.out"), expected_reply.as_bytes()).unwrap();
        let shell = std::env::var_os("ComSpec")
            .or_else(|| std::env::var_os("COMSPEC"))
            .expect("Windows command processor is available");
        let shell = PathBuf::from(shell);
        assert!(shell.is_file(), "the selected test child executable exists");

        let script = root.join("resources").join("inspect-child.cmd");
        std::fs::write(
            &script,
            concat!(
                "@echo off\r\n",
                "set /p input=\r\n",
                "if not \"%input%\"==\"fixture stdin marker\" exit /b 30\r\n",
                "if \"%HER_RUNTIME_ROOT:~0,4%\"==\"\\\\?\\\" exit /b 49\r\n",
                "if /I not \"%HOME%\"==\"%HER_RUNTIME_ROOT%\\user\\dsh\" exit /b 31\r\n",
                "if /I not \"%USERPROFILE%\"==\"%HOME%\" exit /b 32\r\n",
                "if /I not \"%DSH_HOME%\"==\"%HOME%\" exit /b 33\r\n",
                "if /I not \"%DSH_CREDENTIALS%\"==\"%HOME%\\.credentials.yaml\" exit /b 34\r\n",
                "if /I not \"%APPDATA%\"==\"%HOME%\\AppData\\Roaming\" exit /b 35\r\n",
                "if /I not \"%LOCALAPPDATA%\"==\"%HOME%\\AppData\\Local\" exit /b 36\r\n",
                "if /I not \"%XDG_CONFIG_HOME%\"==\"%HOME%\\.config\" exit /b 37\r\n",
                "if /I not \"%XDG_CACHE_HOME%\"==\"%HOME%\\.cache\" exit /b 45\r\n",
                "if /I not \"%XDG_DATA_HOME%\"==\"%HOME%\\.local\\share\" exit /b 46\r\n",
                "if /I not \"%TEMP%\"==\"%HER_RUNTIME_ROOT%\\data\\tmp\" exit /b 47\r\n",
                "if /I not \"%TMP%\"==\"%TEMP%\" exit /b 48\r\n",
                "if defined QQBOT_TICKET04_PARENT_ONLY exit /b 38\r\n",
                "if not \"%QQBOT_TICKET04_EXPLICIT%\"==\"candidate-config-value\" exit /b 39\r\n",
                "if not \"%OPENAI_API_KEY%\"==\"candidate-config-api-key\" exit /b 40\r\n",
                "if not \"%OPENCODE_GO_API_KEY%\"==\"fixture-selected-api-key\" exit /b 41\r\n",
                "if not \"%OPENAI_BASE_URL%\"==\"http://127.0.0.1:43102/v1\" exit /b 42\r\n",
                "if not \"%HER_RUNTIME_ROOT%\"==\"%QQBOT_TICKET04_EXPECTED_ROOT%\" exit /b 43\r\n",
                "if /I not \"%CD%\"==\"%QQBOT_TICKET04_EXPECTED_CWD%\" exit /b 44\r\n",
                "type inspect-child.out\r\n",
            ),
        )
        .unwrap();

        let old_parent = std::env::var_os(PARENT_ONLY);
        let _restore = EnvRestore(old_parent);
        std::env::set_var(PARENT_ONLY, "ambient-parent-value");
        let backend = CliBackend {
            id: "fixture-cli".into(),
            cmd: shell.to_string_lossy().into_owned(),
            args: vec!["/C".into(), "call inspect-child.cmd".into()],
            env: vec![
                ("QQBOT_TICKET04_EXPLICIT".into(), "candidate-config-value".into()),
                ("OPENAI_API_KEY".into(), "candidate-config-api-key".into()),
                ("QQBOT_TICKET04_EXPECTED_ROOT".into(), selected_root.to_string_lossy().into_owned()),
                ("QQBOT_TICKET04_EXPECTED_CWD".into(), selected_resources_without_extended_prefix.into()),
            ],
            context: context.clone(),
        };

        // Capture the fake cmd.exe child's exact output bytes before CliOutputFiles' Drop
        // cleanup, so an invalid-UTF-8 failure can be attributed to this fixture or its
        // environment without weakening RuntimeContext's strict text contract.
        let mut raw_probe = crate::subprocess::command_without_console(&shell);
        raw_probe
            .args(&backend.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for key in [
            "DSH_HOME", "DSH_CREDENTIALS", "USERPROFILE", "HOME", "OPENCODE_GO_API_KEY",
            "OPENCODE_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "OPENAI_API_KEY",
            "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
        ] {
            raw_probe.env_remove(key);
        }
        configure_isolated_cli_environment(&mut raw_probe, &context).unwrap();
        for (key, value) in credential_env_for_runtime(&context) {
            raw_probe.env(key, value);
        }
        for (key, value) in session_env_for_runtime(&context, "group:123") {
            raw_probe.env(key, value);
        }
        raw_probe
            .env("DISABLE_AUTOUPDATER", "1")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CODEX_DISABLE_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env("CI", "1");
        for (key, value) in &backend.env {
            raw_probe.env(key, value);
        }
        let model_url = context.dependency_url("model").unwrap();
        raw_probe
            .env("ANTHROPIC_BASE_URL", model_url)
            .env("OPENAI_BASE_URL", format!("{}/v1", model_url.trim_end_matches('/')));
        let mut raw_child = raw_probe.spawn().expect("raw fake child starts");
        use std::io::{Read as _, Write as _};
        let mut raw_stdin = raw_child.stdin.take().unwrap();
        raw_stdin.write_all(b"fixture stdin marker").unwrap();
        drop(raw_stdin);
        let mut raw_stdout_reader = raw_child.stdout.take().unwrap();
        let mut raw_stderr_reader = raw_child.stderr.take().unwrap();
        let raw_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let raw_status = loop {
            match raw_child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < raw_deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                other => {
                    let _ = raw_child.kill();
                    let _ = raw_child.wait();
                    panic!("raw fake child did not finish within 5s: {other:?}");
                }
            }
        };
        let mut raw_stdout = Vec::new();
        let mut raw_stderr = Vec::new();
        raw_stdout_reader.read_to_end(&mut raw_stdout).unwrap();
        raw_stderr_reader.read_to_end(&mut raw_stderr).unwrap();
        assert!(raw_status.success(), "raw fake child status: {raw_status:?}; stderr bytes: {raw_stderr:?}");
        assert_eq!(raw_stdout, expected_reply.as_bytes());
        assert!(raw_stderr.is_empty(), "raw fake child stderr: {raw_stderr:?}");
        let bytes_hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        eprintln!(
            "fake_cmd_raw_probe status={raw_status:?} stdout_hex={} stdout_utf8={:?} stderr_hex={} stderr_utf8={:?}",
            bytes_hex(&raw_stdout),
            std::str::from_utf8(&raw_stdout),
            bytes_hex(&raw_stderr),
            std::str::from_utf8(&raw_stderr),
        );

        let reply = backend
            .ask(&BackendRequest {
                key: "group:123".into(),
                prompt: "fixture stdin marker".into(),
                timeout_s: 5,
            })
            .expect("the fake child exits successfully");
        assert!(reply.text.contains("stdin=fixture stdin marker"));
        assert!(reply.text.contains(&format!("home={}", selected_dsh_root.display())));
        assert!(reply.text.contains(&format!("profile={}", selected_dsh_root.display())));
        assert!(reply.text.contains(&format!("appdata={}", selected_roaming.display())));
        assert!(reply.text.contains(&format!("localappdata={}", selected_local.display())));
        assert!(reply.text.contains(&format!("xdg={}", selected_xdg.display())));
        assert!(reply.text.contains(&format!("xdg_cache={}", selected_xdg_cache.display())));
        assert!(reply.text.contains(&format!("xdg_data={}", selected_xdg_data.display())));
        assert!(reply.text.contains(&format!("temp={}", selected_temp.display())));
        assert!(reply.text.contains(&format!("tmp={}", selected_temp.display())));
        assert!(reply.text.contains("inherited=\r\n") || reply.text.contains("inherited=\n"));
        assert!(reply.text.contains("explicit=candidate-config-value"));
        assert!(reply.text.contains("credential=candidate-config-api-key"));
        assert!(reply.text.contains("dshcredential=fixture-selected-api-key"));
        assert!(reply.text.contains("model=http://127.0.0.1:43102/v1"));
        assert!(reply.text.contains(&format!("runtime_root={}", selected_root.display())));
        assert!(reply.text.contains(selected_resources_without_extended_prefix));
        let logs = root.join("data/logs");
        assert!(
            std::fs::read_dir(&logs).unwrap().next().is_none(),
            "successful child captures are removed"
        );

        assert!(validate_isolated_cli_settings(
            &context,
            &[format!("--endpoint={}", context.dependency_url("model").unwrap())],
            &[("OPENAI_API_KEY".into(), "selected-candidate-key".into())],
        )
        .is_ok(), "selected endpoint and candidate-config credentials are explicit inputs");
        assert!(validate_isolated_cli_settings(&context, &["/C".into()], &[]).is_ok());
        assert!(validate_isolated_cli_settings(
            &context,
            &["--endpoint=https://api.example.invalid".into()],
            &[],
        )
        .is_err(), "external model endpoints are not accepted from candidate args");
        assert!(validate_isolated_cli_settings(
            &context,
            &["--config=../outside.json".into()],
            &[],
        )
        .is_err(), "parent traversal in candidate args is rejected");
        assert!(validate_isolated_cli_settings(
            &context,
            &[],
            &[("OPENAI_BASE_URL".into(), "https://api.example.invalid".into())],
        )
        .is_err(), "candidate env cannot override the selected fake model URL");

        std::fs::write(&script, "@echo off\r\necho stdout-marker\r\necho stderr-marker 1>&2\r\nexit /b 7\r\n").unwrap();
        let error_backend = CliBackend { args: backend.args.clone(), ..backend };
        let error = error_backend
            .ask(&BackendRequest {
                key: "group:123".into(),
                prompt: "stderr marker".into(),
                timeout_s: 5,
            })
            .expect_err("the fake child exits unsuccessfully");
        assert_eq!(
            error.message(),
            "model/backend backend (BACKEND_ERROR) command=fixture-cli exit_code=Some(7)"
        );
        for captured in ["stdout-marker", "stderr-marker"] {
            assert!(
                !error.message().contains(captured),
                "CLI error leaked captured output: {}",
                error.message()
            );
        }
        assert!(
            std::fs::read_dir(&logs).unwrap().next().is_none(),
            "failed child captures are removed"
        );

        std::fs::write(
            &script,
            "@echo off\r\necho unused\r\n",
        )
        .unwrap();
        let powershell = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .map(PathBuf::from)
            .expect("Windows system root is available")
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        assert!(powershell.is_file(), "the fake timeout child is installed");
        let timed = CliBackend {
            cmd: powershell.to_string_lossy().into_owned(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 30".into(),
            ],
            ..error_backend
        };
        let timeout_started = std::time::Instant::now();
        let error = timed
            .ask(&BackendRequest {
                key: "group:123".into(),
                prompt: "timeout marker".into(),
                timeout_s: 5,
            })
            .expect_err("the fake child exceeds the bounded timeout");
        assert!(error.message().contains("超时"));
        assert!(timeout_started.elapsed() >= std::time::Duration::from_secs(5));
        assert!(
            std::fs::read_dir(&logs).unwrap().next().is_none(),
            "timeout capture files are removed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn isolated_cli_deadline_covers_blocked_stdin_and_reaps_descendant() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        const MODE_ENV: &str = "QQBOT_TICKET04_TREE_MODE";
        const ROOT_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_ROOT_PID_FILE";
        const CHILD_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_CHILD_PID_FILE";
        const READY_FILE_ENV: &str = "QQBOT_TICKET04_TREE_READY_FILE";
        const FAILURE_FILE_ENV: &str = "QQBOT_TICKET04_TREE_FAILURE_FILE";
        const MONITOR_ACK_FILE_ENV: &str = "QQBOT_TICKET04_TREE_MONITOR_ACK_FILE";

        struct FixtureRoot {
            path: PathBuf,
            parent: PathBuf,
        }

        impl Drop for FixtureRoot {
            fn drop(&mut self) {
                let Ok(path) = std::fs::canonicalize(&self.path) else {
                    return;
                };
                let Ok(parent) = std::fs::canonicalize(&self.parent) else {
                    return;
                };
                let has_test_prefix = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ticket04 CLI tree 中文-"));
                if path.parent() == Some(parent.as_path()) && has_test_prefix {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }

        let temp_parent = crate::install_root::development_root(std::env::temp_dir().join("her-ticket04-backend-lifecycle"));
        std::fs::create_dir_all(&temp_parent).unwrap();
        let temp_parent = std::fs::canonicalize(temp_parent).unwrap();
        let fixture_path = temp_parent.join(format!(
            "ticket04 CLI tree 中文-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&fixture_path).unwrap();
        let _fixture = FixtureRoot {
            path: fixture_path.clone(),
            parent: temp_parent,
        };

        for directory in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(fixture_path.join(directory)).unwrap();
        }
        std::fs::write(
            fixture_path.join("config/config.json"),
            r#"{"consoleToken":"ticket04-synthetic"}"#,
        )
        .unwrap();
        std::fs::write(
            fixture_path.join("data/social-v2.json"),
            r#"{"conversations":{}}"#,
        )
        .unwrap();
        std::fs::write(
            fixture_path.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        )
        .unwrap();
        std::fs::write(
            fixture_path.join("user/dsh/.credentials.yaml"),
            "OPENCODE_GO_API_KEY: fixture-aaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let context = RuntimeContext::from_root(&fixture_path).unwrap();
        let data = fixture_path.join("data");
        let root_pid_file = data.join("cli-root.pid");
        let child_pid_file = data.join("cli-descendant.pid");
        let ready_file = data.join("cli-descendant.ready");
        let failure_file = data.join("cli-child.failure");
        let monitor_ack_file = data.join("cli-processes-observed.ready");
        let logs = data.join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        let test_executable = std::env::current_exe().unwrap();
        let backend = CliBackend {
            id: "ticket04-lifecycle-fixture".into(),
            cmd: test_executable.to_string_lossy().into_owned(),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                "backend::tests::isolated_cli_lifecycle_probe".into(),
                "--nocapture".into(),
            ],
            env: vec![
                (MODE_ENV.into(), "root".into()),
                (
                    ROOT_PID_FILE_ENV.into(),
                    root_pid_file.to_string_lossy().into_owned(),
                ),
                (
                    CHILD_PID_FILE_ENV.into(),
                    child_pid_file.to_string_lossy().into_owned(),
                ),
                (
                    READY_FILE_ENV.into(),
                    ready_file.to_string_lossy().into_owned(),
                ),
                (
                    FAILURE_FILE_ENV.into(),
                    failure_file.to_string_lossy().into_owned(),
                ),
                (
                    MONITOR_ACK_FILE_ENV.into(),
                    monitor_ack_file.to_string_lossy().into_owned(),
                ),
            ],
            context,
        };

        let monitor_root = root_pid_file.clone();
        let monitor_child = child_pid_file.clone();
        let monitor_failure = failure_file.clone();
        let monitor_ack = monitor_ack_file.clone();
        let (process_probe_sender, process_probe_receiver) = std::sync::mpsc::sync_channel(1);
        let process_monitor = std::thread::spawn(move || {
            let result = wait_for_cli_probe_processes(
                &monitor_root,
                &monitor_child,
                &monitor_failure,
                Duration::from_secs(8),
            );
            let acknowledge = result.is_ok();
            // Publish the pinned handles before letting the fixture finish its setup.
            let sent = process_probe_sender.send(result).is_ok();
            if acknowledge && sent {
                if let Err(error) = write_cli_probe_marker(&monitor_ack, "recorded process handles are open\n") {
                    let _ = write_cli_probe_marker(
                        &monitor_failure,
                        &format!("parent could not acknowledge observed process handles: {error}"),
                    );
                }
            }
        });

        let timeout = Duration::from_secs(5);
        // The fake CLI never reads stdin. Eight MiB is intentionally much larger than a
        // Windows anonymous-pipe buffer, so this exercises a blocked write, not a fast error.
        let prompt = "x".repeat(8 * 1024 * 1024);
        let started = Instant::now();
        let outcome = backend.ask(&BackendRequest {
            key: "group:ticket04-synthetic".into(),
            prompt,
            timeout_s: timeout.as_secs(),
        });
        let elapsed = started.elapsed();
        // Probe the observer's pinned handles immediately after ask returns. Joining first can
        // hide a live descendant that exits during a delayed setup/observer wait.
        let process_probe_at_return = process_probe_receiver.try_recv().ok();
        let observer_handles_pinned_at_return = matches!(&process_probe_at_return, Some(Ok(_)));
        let (root_alive_at_return, child_alive_at_return) = process_probe_at_return
            .as_ref()
            .and_then(|probe| probe.as_ref().ok())
            .map(|(_, _, root_handle, child_handle, _)| {
                (Some(root_handle.is_running()), Some(child_handle.is_running()))
            })
            .unwrap_or((None, None));
        let outcome_summary = match &outcome {
            Ok(reply) => format!("success:{} bytes", reply.text.len()),
            Err(error) => format!("error:{}", error.message()),
        };
        let timeout_classified = matches!(&outcome, Err(error) if error.message().contains("超时"));
        let ready_pid = std::fs::read_to_string(&ready_file)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        let ready_seen = ready_pid.is_some();
        let captures_removed = std::fs::read_dir(&logs)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        let observer_joined = process_monitor.join().is_ok();
        let process_probe = match process_probe_at_return {
            Some(result) => result,
            None => process_probe_receiver
                .recv()
                .unwrap_or_else(|_| Err(format!("process monitor ended without a result; joined={observer_joined}"))),
        };
        let (root_pid, child_pid, root_handle, child_handle, mut setup_failure) = match process_probe {
            Ok(processes) => processes,
            Err(error) => {
                eprintln!(
                    "ticket04_cli_lifecycle outcome={outcome_summary:?} elapsed_ms={} ready_seen={ready_seen} observer_handles_pinned_at_return={observer_handles_pinned_at_return} observer_joined={observer_joined} captures_removed={captures_removed} process_probe_error={error:?}",
                    elapsed.as_millis()
                );
                panic!("the synthetic CLI process tree was not observed");
            }
        };
        if setup_failure.is_none() {
            setup_failure = std::fs::read_to_string(&failure_file).ok();
        }
        // The fake processes self-terminate even when this regression test is red. Wait for
        // those exact observed handles; never search for or signal unrelated process IDs.
        let root_stopped_within_bound = root_handle.wait_for_exit(Duration::from_secs(2));
        let child_stopped_within_bound = child_handle.wait_for_exit(Duration::from_secs(13));
        eprintln!(
            "ticket04_cli_lifecycle outcome={outcome_summary:?} elapsed_ms={} root_pid={root_pid} descendant_pid={child_pid} ready_pid={ready_pid:?} setup_failure={setup_failure:?} observer_handles_pinned_at_return={observer_handles_pinned_at_return} observer_joined={observer_joined} root_alive_at_return={root_alive_at_return:?} descendant_alive_at_return={child_alive_at_return:?} root_stopped_within_bound={root_stopped_within_bound:?} descendant_stopped_within_bound={child_stopped_within_bound:?} captures_removed={captures_removed}",
            elapsed.as_millis()
        );

        assert!(
            setup_failure.is_none(),
            "synthetic subprocess setup must reach its ready state: {setup_failure:?}"
        );
        assert!(
            timeout_classified,
            "the shared absolute deadline must cover stdin writes and report timeout: {outcome_summary}"
        );
        assert!(
            elapsed <= timeout + Duration::from_secs(2),
            "spawn, stdin write, and wait must share one bounded lifetime; elapsed={elapsed:?}"
        );
        assert!(ready_seen, "the recorded fake descendant reached its ready state");
        assert_eq!(ready_pid, Some(child_pid), "the ready marker belongs to the recorded descendant PID");
        assert!(observer_handles_pinned_at_return, "both exact PID handles were ready when ask returned");
        assert_eq!(root_alive_at_return, Some(Ok(false)), "the owned CLI child has exited at ask return");
        assert_eq!(
            child_alive_at_return,
            Some(Ok(false)),
            "the owned descendant is gone when ask returns"
        );
        assert_eq!(root_stopped_within_bound, Ok(true));
        assert_eq!(child_stopped_within_bound, Ok(true));
        assert!(captures_removed, "timeout capture files are removed");
    }

    #[cfg(windows)]
    #[test]
    fn isolated_cli_preserves_success_and_reaps_descendant_after_root_exit() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        const MODE_ENV: &str = "QQBOT_TICKET04_TREE_MODE";
        const ROOT_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_ROOT_PID_FILE";
        const CHILD_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_CHILD_PID_FILE";
        const READY_FILE_ENV: &str = "QQBOT_TICKET04_TREE_READY_FILE";
        const FAILURE_FILE_ENV: &str = "QQBOT_TICKET04_TREE_FAILURE_FILE";
        const MONITOR_ACK_FILE_ENV: &str = "QQBOT_TICKET04_TREE_MONITOR_ACK_FILE";

        struct FixtureRoot {
            path: PathBuf,
            parent: PathBuf,
        }

        impl Drop for FixtureRoot {
            fn drop(&mut self) {
                let Ok(path) = std::fs::canonicalize(&self.path) else {
                    return;
                };
                let Ok(parent) = std::fs::canonicalize(&self.parent) else {
                    return;
                };
                let valid_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ticket04 CLI root exit 中文-"));
                if path.parent() == Some(parent.as_path()) && valid_name {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }

        let temp_parent = crate::install_root::development_root(std::env::temp_dir().join("her-ticket04-backend-root-exit"));
        std::fs::create_dir_all(&temp_parent).unwrap();
        let temp_parent = std::fs::canonicalize(temp_parent).unwrap();
        let fixture_path = temp_parent.join(format!(
            "ticket04 CLI root exit 中文-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&fixture_path).unwrap();
        let _fixture = FixtureRoot {
            path: fixture_path.clone(),
            parent: temp_parent,
        };

        for directory in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(fixture_path.join(directory)).unwrap();
        }
        std::fs::write(
            fixture_path.join("config/config.json"),
            r#"{"consoleToken":"ticket04-synthetic"}"#,
        )
        .unwrap();
        std::fs::write(fixture_path.join("data/social-v2.json"), r#"{"conversations":{}}"#)
            .unwrap();
        std::fs::write(
            fixture_path.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        )
        .unwrap();
        std::fs::write(
            fixture_path.join("user/dsh/.credentials.yaml"),
            "OPENCODE_GO_API_KEY: fixture-aaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let context = RuntimeContext::from_root(&fixture_path).unwrap();
        let data = fixture_path.join("data");
        let root_pid_file = data.join("root.pid");
        let child_pid_file = data.join("descendant.pid");
        let ready_file = data.join("descendant.ready");
        let failure_file = data.join("child.failure");
        let monitor_ack_file = data.join("processes-observed.ready");
        let logs = data.join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        let test_executable = std::env::current_exe().unwrap();
        let backend = CliBackend {
            id: "ticket04-root-exit-fixture".into(),
            cmd: test_executable.to_string_lossy().into_owned(),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                "backend::tests::isolated_cli_lifecycle_probe".into(),
                "--nocapture".into(),
            ],
            env: vec![
                (MODE_ENV.into(), "normal-exit".into()),
                (ROOT_PID_FILE_ENV.into(), root_pid_file.to_string_lossy().into_owned()),
                (CHILD_PID_FILE_ENV.into(), child_pid_file.to_string_lossy().into_owned()),
                (READY_FILE_ENV.into(), ready_file.to_string_lossy().into_owned()),
                (FAILURE_FILE_ENV.into(), failure_file.to_string_lossy().into_owned()),
                (MONITOR_ACK_FILE_ENV.into(), monitor_ack_file.to_string_lossy().into_owned()),
            ],
            context,
        };

        let monitor_root = root_pid_file.clone();
        let monitor_child = child_pid_file.clone();
        let monitor_failure = failure_file.clone();
        let monitor_ack = monitor_ack_file.clone();
        let (probe_sender, probe_receiver) = std::sync::mpsc::sync_channel(1);
        let process_monitor = std::thread::spawn(move || {
            let result = wait_for_cli_probe_handles(
                &monitor_root,
                &monitor_child,
                &monitor_failure,
                Duration::from_secs(8),
            );
            let acknowledged = result.is_ok();
            let _ = probe_sender.send(result);
            if acknowledged {
                let _ = write_cli_probe_marker(&monitor_ack, "recorded process handles are open\n");
            }
        });

        let timeout = Duration::from_secs(10);
        let prompt = "normal-root-exit prompt";
        let started = Instant::now();
        let outcome = backend.ask(&BackendRequest {
            key: "group:ticket04-synthetic".into(),
            prompt: prompt.into(),
            timeout_s: timeout.as_secs(),
        });
        let elapsed = started.elapsed();
        let probe_at_return = probe_receiver.try_recv().ok();
        let observer_handles_pinned_at_return = matches!(&probe_at_return, Some(Ok(_)));
        let (root_alive_at_return, child_alive_at_return) = probe_at_return
            .as_ref()
            .and_then(|probe| probe.as_ref().ok())
            .map(|(_, _, root_handle, child_handle, _)| {
                (Some(root_handle.is_running()), Some(child_handle.is_running()))
            })
            .unwrap_or((None, None));
        let outcome_summary = match &outcome {
            Ok(reply) => format!("success:{} bytes", reply.text.len()),
            Err(error) => format!("error:{}", error.message()),
        };
        let captures_removed = std::fs::read_dir(&logs)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        let observer_joined = process_monitor.join().is_ok();
        let probe = match probe_at_return {
            Some(result) => result,
            None => probe_receiver
                .recv()
                .unwrap_or_else(|_| Err(format!("process monitor ended without a result; joined={observer_joined}"))),
        };
        let (root_pid, child_pid, root_handle, child_handle, setup_failure) = match probe {
            Ok(processes) => processes,
            Err(error) => {
                eprintln!(
                    "ticket04_cli_root_exit outcome={outcome_summary:?} elapsed_ms={} observer_handles_pinned_at_return={observer_handles_pinned_at_return} observer_joined={observer_joined} captures_removed={captures_removed} process_probe_error={error:?}",
                    elapsed.as_millis()
                );
                panic!("the synthetic CLI process tree was not observed");
            }
        };
        let root_stopped_within_bound = root_handle.wait_for_exit(Duration::from_secs(2));
        let child_stopped_within_bound = child_handle.wait_for_exit(Duration::from_secs(13));
        eprintln!(
            "ticket04_cli_root_exit outcome={outcome_summary:?} elapsed_ms={} root_pid={root_pid} descendant_pid={child_pid} setup_failure={setup_failure:?} observer_handles_pinned_at_return={observer_handles_pinned_at_return} observer_joined={observer_joined} root_alive_at_return={root_alive_at_return:?} descendant_alive_at_return={child_alive_at_return:?} root_stopped_within_bound={root_stopped_within_bound:?} descendant_stopped_within_bound={child_stopped_within_bound:?} captures_removed={captures_removed}",
            elapsed.as_millis()
        );

        let reply = outcome.expect("the root CLI process returns a successful exit code");
        assert!(setup_failure.is_none(), "the descendant setup completed: {setup_failure:?}");
        assert!(elapsed < timeout - Duration::from_secs(2), "root exit should not wait for the full CLI deadline: {elapsed:?}");
        assert!(observer_handles_pinned_at_return, "the exact root and descendant handles were ready before root exit");
        assert_eq!(root_alive_at_return, Some(Ok(false)), "the CLI root exited before ask returned");
        assert_eq!(child_alive_at_return, Some(Ok(false)), "the contained descendant is gone when ask returns");
        assert_eq!(root_stopped_within_bound, Ok(true));
        assert_eq!(child_stopped_within_bound, Ok(true));
        assert!(reply.text.contains("stdin=normal-root-exit prompt"), "the root consumed the complete prompt before exiting");
        assert!(reply.text.contains("root-success-capture-marker"), "successful stdout capture is returned");
        assert!(captures_removed, "successful output capture files are removed");
    }

    #[cfg(windows)]
    #[test]
    fn isolated_cli_cleanup_kills_unassigned_child_after_wait_observation_error() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        for force_close in [false, true] {
            let (diagnostic, reaped) =
                windows_cli_lifecycle::cleanup_unassigned_after_wait_error_for_test(force_close)
                    .expect("cleanup should confirm termination of its own suspended child");
            assert!(
                diagnostic.contains("injected one-shot root wait observation error"),
                "wait failure must remain visible in cleanup diagnostics: {diagnostic}"
            );
            assert!(
                diagnostic.contains("terminated and reaped"),
                "cleanup should report successful termination despite preserving the observation error: {diagnostic}"
            );
            assert!(reaped, "the exact owned suspended child handle reports process exit");
        }
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "spawned as a bounded synthetic CLI subprocess by its parent lifecycle test"]
    fn isolated_cli_lifecycle_probe() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        const MODE_ENV: &str = "QQBOT_TICKET04_TREE_MODE";
        const ROOT_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_ROOT_PID_FILE";
        const CHILD_PID_FILE_ENV: &str = "QQBOT_TICKET04_TREE_CHILD_PID_FILE";
        const READY_FILE_ENV: &str = "QQBOT_TICKET04_TREE_READY_FILE";
        const FAILURE_FILE_ENV: &str = "QQBOT_TICKET04_TREE_FAILURE_FILE";
        const MONITOR_ACK_FILE_ENV: &str = "QQBOT_TICKET04_TREE_MONITOR_ACK_FILE";

        match std::env::var(MODE_ENV).as_deref() {
            Ok("normal-exit") => {
                use std::io::Read as _;

                let root_pid_file = PathBuf::from(
                    std::env::var_os(ROOT_PID_FILE_ENV).expect("selected root PID marker path"),
                );
                let child_pid_file = PathBuf::from(
                    std::env::var_os(CHILD_PID_FILE_ENV).expect("selected descendant PID marker path"),
                );
                let ready_file = PathBuf::from(
                    std::env::var_os(READY_FILE_ENV).expect("selected descendant ready marker path"),
                );
                let failure_file = PathBuf::from(
                    std::env::var_os(FAILURE_FILE_ENV).expect("selected failure marker path"),
                );
                let monitor_ack_file = PathBuf::from(
                    std::env::var_os(MONITOR_ACK_FILE_ENV).expect("selected monitor acknowledgement path"),
                );
                write_cli_probe_marker(&root_pid_file, &format!("{}\n", std::process::id()))
                    .expect("record the exact fake CLI root PID");

                let mut prompt = String::new();
                std::io::stdin()
                    .read_to_string(&mut prompt)
                    .expect("read the complete synthetic prompt to EOF");

                let executable = std::env::current_exe().expect("test executable path");
                let mut descendant = crate::subprocess::command_without_console(executable);
                descendant
                    .args([
                        "--ignored",
                        "--exact",
                        "backend::tests::isolated_cli_lifecycle_probe",
                        "--nocapture",
                    ])
                    .env(MODE_ENV, "descendant")
                    .env(CHILD_PID_FILE_ENV, &child_pid_file)
                    .env(READY_FILE_ENV, &ready_file)
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut descendant = descendant.spawn().expect("start the fake descendant holding stdin");
                let child_pid = descendant.id();
                if let Err(error) = write_cli_probe_marker(&child_pid_file, &format!("{child_pid}\n")) {
                    let _ = write_cli_probe_marker(&failure_file, &format!("could not record descendant PID: {error}"));
                    let _ = descendant.wait();
                    return;
                }

                let ready_deadline = Instant::now() + Duration::from_secs(3);
                while !ready_file.is_file() && Instant::now() < ready_deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !ready_file.is_file() {
                    write_cli_probe_marker(&failure_file, "normal-exit descendant did not become ready")
                        .expect("record synthetic helper failure");
                    let _ = descendant.wait();
                    return;
                }
                let monitor_deadline = Instant::now() + Duration::from_secs(3);
                while !monitor_ack_file.is_file() && Instant::now() < monitor_deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !monitor_ack_file.is_file() {
                    write_cli_probe_marker(&failure_file, "parent did not pin normal-exit process handles")
                        .expect("record synthetic monitor failure");
                    let _ = descendant.wait();
                    return;
                }
                drop(descendant);
                println!("root-success-capture-marker; stdin={prompt}");
            }
            Ok("root") => {
                let root_pid_file = PathBuf::from(
                    std::env::var_os(ROOT_PID_FILE_ENV).expect("selected root PID marker path"),
                );
                let child_pid_file = PathBuf::from(
                    std::env::var_os(CHILD_PID_FILE_ENV).expect("selected descendant PID marker path"),
                );
                let ready_file = PathBuf::from(
                    std::env::var_os(READY_FILE_ENV).expect("selected descendant ready marker path"),
                );
                let failure_file = PathBuf::from(
                    std::env::var_os(FAILURE_FILE_ENV).expect("selected failure marker path"),
                );
                let monitor_ack_file = PathBuf::from(
                    std::env::var_os(MONITOR_ACK_FILE_ENV).expect("selected process monitor acknowledgement path"),
                );
                write_cli_probe_marker(&root_pid_file, &format!("{}\n", std::process::id()))
                    .expect("record the exact fake CLI child PID");

                let executable = std::env::current_exe().expect("test executable path");
                let mut child = crate::subprocess::command_without_console(executable);
                child
                    .args([
                        "--ignored",
                        "--exact",
                        "backend::tests::isolated_cli_lifecycle_probe",
                        "--nocapture",
                    ])
                    .env(MODE_ENV, "descendant")
                    .env(CHILD_PID_FILE_ENV, &child_pid_file)
                    .env(READY_FILE_ENV, &ready_file)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut descendant = child.spawn().expect("start the owned fake descendant");
                let child_pid = descendant.id();
                if let Err(error) = write_cli_probe_marker(&child_pid_file, &format!("{child_pid}\n")) {
                    let _ = write_cli_probe_marker(
                        &failure_file,
                        &format!("could not record descendant PID: {error}"),
                    );
                    let _ = descendant.wait();
                    return;
                }

                let ready_deadline = Instant::now() + Duration::from_secs(2);
                while !ready_file.is_file() && Instant::now() < ready_deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !ready_file.is_file() {
                    write_cli_probe_marker(&failure_file, "descendant did not reach ready state")
                        .expect("record synthetic helper failure");
                    let _ = descendant.wait();
                    return;
                }
                let monitor_deadline = Instant::now() + Duration::from_secs(2);
                while !monitor_ack_file.is_file() && Instant::now() < monitor_deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !monitor_ack_file.is_file() {
                    write_cli_probe_marker(&failure_file, "parent test did not pin recorded process handles")
                        .expect("record synthetic monitor failure");
                    let _ = descendant.wait();
                    return;
                }
                drop(descendant);

                // No reads from stdin: the CliBackend writer must block on the full pipe until
                // its deadline kills this process tree. This self-termination bounds the red run.
                std::thread::sleep(Duration::from_secs(7));
            }
            Ok("descendant") => {
                let ready_file = PathBuf::from(
                    std::env::var_os(READY_FILE_ENV).expect("selected descendant ready marker path"),
                );
                write_cli_probe_marker(&ready_file, &format!("{}\n", std::process::id()))
                    .expect("record the descendant ready state");
                std::thread::sleep(Duration::from_secs(12));
            }
            _ => {}
        }
    }

    #[cfg(windows)]
    fn write_cli_probe_marker(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
        use std::io::Write as _;

        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(format!(".{}.tmp", std::process::id()));
        let temporary = PathBuf::from(temporary);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temporary, path)
    }

    #[cfg(windows)]
    struct ProcessWaitHandle(usize);

    #[cfg(windows)]
    impl ProcessWaitHandle {
        fn open(pid: u32) -> Result<Self, String> {
            const SYNCHRONIZE: u32 = 0x0010_0000;
            let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                return Err(format!(
                    "OpenProcess({pid}) failed with Win32 error {}",
                    unsafe { GetLastError() }
                ));
            }
            Ok(Self(handle as usize))
        }

        fn is_running(&self) -> Result<bool, String> {
            self.wait_for_exit(std::time::Duration::ZERO)
                .map(|exited| !exited)
        }

        fn wait_for_exit(&self, timeout: std::time::Duration) -> Result<bool, String> {
            const WAIT_OBJECT_0: u32 = 0;
            const WAIT_TIMEOUT: u32 = 258;
            const WAIT_FAILED: u32 = u32::MAX;
            let milliseconds = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
            let result = unsafe { WaitForSingleObject(self.0 as *mut std::ffi::c_void, milliseconds) };
            match result {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                WAIT_FAILED => Err(format!("WaitForSingleObject failed with Win32 error {}", unsafe {
                    GetLastError()
                })),
                other => Err(format!("unexpected process wait status {other}")),
            }
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessWaitHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0 as *mut std::ffi::c_void);
            }
        }
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[cfg(windows)]
    fn wait_for_cli_probe_processes(
        root_pid_file: &std::path::Path,
        child_pid_file: &std::path::Path,
        failure_file: &std::path::Path,
        timeout: std::time::Duration,
    ) -> Result<(u32, u32, ProcessWaitHandle, ProcessWaitHandle, Option<String>), String> {
        use std::time::Instant;

        let deadline = Instant::now() + timeout;
        loop {
            let root_pid = std::fs::read_to_string(root_pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            let child_pid = std::fs::read_to_string(child_pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if let (Some(root_pid), Some(child_pid)) = (root_pid, child_pid) {
                let root_handle = ProcessWaitHandle::open(root_pid)?;
                let child_handle = ProcessWaitHandle::open(child_pid)?;
                let setup_failure = std::fs::read_to_string(failure_file).ok();
                return Ok((root_pid, child_pid, root_handle, child_handle, setup_failure));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for this test's child PID markers; root_exists={} descendant_exists={} setup_failure={:?}",
                    root_pid_file.is_file(),
                    child_pid_file.is_file(),
                    std::fs::read_to_string(failure_file).ok()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    fn wait_for_cli_probe_handles(
        root_pid_file: &std::path::Path,
        child_pid_file: &std::path::Path,
        failure_file: &std::path::Path,
        timeout: std::time::Duration,
    ) -> Result<(u32, u32, ProcessWaitHandle, ProcessWaitHandle, Option<String>), String> {
        use std::time::Instant;

        let deadline = Instant::now() + timeout;
        loop {
            let root_pid = std::fs::read_to_string(root_pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            let child_pid = std::fs::read_to_string(child_pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if let (Some(root_pid), Some(child_pid)) = (root_pid, child_pid) {
                let root_handle = ProcessWaitHandle::open(root_pid)?;
                let child_handle = ProcessWaitHandle::open(child_pid)?;
                let setup_failure = std::fs::read_to_string(failure_file).ok();
                return Ok((root_pid, child_pid, root_handle, child_handle, setup_failure));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for this test's child PID markers; root_exists={} descendant_exists={} setup_failure={:?}",
                    root_pid_file.is_file(),
                    child_pid_file.is_file(),
                    std::fs::read_to_string(failure_file).ok()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
