//! Runtime path and dependency selection for the bridge.
//!
//! The legacy context preserves the existing production paths. An explicit runtime root confines
//! console and MCP file access to a selected fixture tree. The path resolver is request-scoped:
//! callers pass `&RuntimeContext` through every operation rather than keeping a mutable process
//! global, which also makes two contexts safe to use concurrently in one process.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

const LEGACY_CONFIG: &str = "C:/qqbot/qq-bridge/config.json";
const LEGACY_DATA: &str = "C:/qqbot/qq-bridge/state";
const LEGACY_RESOURCES: &str = "C:/qqbot/qq-bridge";
const LEGACY_SOCIAL_STATE: &str = "C:/qqbot/qq-bridge/state/social-v2.json";
const REQUIRED_DEPENDENCIES: [&str; 3] = ["dsh", "model", "onebot"];
const OPTIONAL_DEPENDENCIES: [&str; 2] = ["onebot_ws", "relay"];
static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    pub(crate) static AUTHORITY_WRITE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = std::cell::RefCell::new(None);
    static ROOT_RESOLUTION_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

/// A stable logical path area. `relative` passed to `resolve_path` must contain only normal
/// components; callers should use these areas instead of assembling runtime-root paths themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeArea {
    Config,
    Data,
    Resources,
    DshUser,
    /// DSH's working folder for supervisor-launched DSH (selected runtimes only).
    DshWork,
    Memory,
    Logs,
}

/// Expected final-node behavior for a resolved path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathIntent {
    ReadFile,
    ReadOptionalFile,
    ReadDirectory,
    WriteFile,
    CreateDirectory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeMode {
    Legacy,
    Isolated,
}

#[derive(Clone, Debug)]
pub struct RuntimeContext {
    mode: RuntimeMode,
    runtime_root: Option<PathBuf>,
    config_file: PathBuf,
    data_dir: PathBuf,
    resources_dir: PathBuf,
    dsh_user_dir: Option<PathBuf>,
    console_port: Option<u16>,
    dependencies: BTreeMap<String, String>,
    config: Option<Value>,
}

// Thread-local and compiled only for unit tests: each fixture controls its own append.
#[cfg(test)]
thread_local! {
    pub(crate) static APPEND_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str, &[u8]) -> Result<(), String>>>> = std::cell::RefCell::new(None);
    pub(crate) static APPEND_IO_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str) -> std::io::Result<()>>>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn append_io_test_hook(stage: &str) -> std::io::Result<()> {
    APPEND_IO_TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(stage),
        None => Ok(()),
    })
}

#[cfg(test)]
pub(crate) fn append_test_hook(stage: &str, bytes: &[u8]) -> Result<(), String> {
    APPEND_TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(stage, bytes),
        None => Ok(()),
    })
}

/// True when a non-empty file does not end in a newline, i.e. its last line was cut short.
/// Append-only JSONL writers start a new line first so a torn tail stays its own line.
pub(crate) fn ends_mid_line(file: &mut fs::File) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Parse an append-only JSONL evidence file. A line whose parse fails only because the input ended early
/// (serde_json `Category::Eof`: a truncated prefix of a record, left by a crash mid-append; the next append
/// terminates it) is skipped with a warning naming `file` and the line. Any other malformed line is an error:
/// the file is evidence. A last line without a newline that parses is a whole record and is kept.
pub(crate) fn parse_evidence_jsonl(text: &str, file: &str, damaged: &str) -> Result<Vec<serde_json::Value>, String> {
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(record) => records.push(record),
            Err(error) if error.classify() == serde_json::error::Category::Eof => {
                eprintln!("[ledger] warning: {file} line {}: truncated record skipped ({} bytes)", index + 1, line.len());
            }
            Err(error) => return Err(format!("{damaged}：{error}")),
        }
    }
    Ok(records)
}

impl RuntimeContext {
    /// The legacy context is kept for callers which have not opted into a runtime root.
    pub fn legacy() -> Self {
        Self {
            mode: RuntimeMode::Legacy,
            runtime_root: None,
            config_file: PathBuf::from(LEGACY_CONFIG),
            data_dir: PathBuf::from(LEGACY_DATA),
            resources_dir: PathBuf::from(LEGACY_RESOURCES),
            dsh_user_dir: None,
            console_port: None,
            dependencies: BTreeMap::new(),
            config: None,
        }
    }

    /// Resolve the CLI/environment selection before any legacy command can read production paths.
    pub fn from_process_args(args: &[String]) -> Result<Self, &'static str> {
        let environment_root = std::env::var_os("HER_RUNTIME_ROOT").map(PathBuf::from);
        Self::from_process_args_with_environment(args, environment_root)
    }

    fn from_process_args_with_environment(
        args: &[String],
        environment_root: Option<PathBuf>,
    ) -> Result<Self, &'static str> {
        if environment_root.is_none() && !args.iter().any(|arg| arg == "--runtime-root") {
            return Ok(Self::legacy());
        }
        let (root, port) = Self::parse_isolated_startup_args(args, environment_root)?;
        let context = Self::isolated(&root)?;
        if args.iter().skip(1).any(|arg| arg == "--live")
            && context.dependency_url("onebot_ws").is_none()
        {
            return Err("isolated live runtime requires a selected OneBot WebSocket endpoint");
        }
        Ok(context.with_console_port(port))
    }

    // Keep this as the single isolated dispatch gate. Live cannot fall through into legacy
    // startup unless selected-root inputs and its explicit group set are valid.
    fn parse_isolated_startup_args(args: &[String], mut root: Option<PathBuf>) -> Result<(PathBuf, u16), &'static str> {
        let mut startup_mode = None;
        let mut port = None;
        let mut live_groups: Option<&str> = None;
        let mut no_reconnect = false;
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "--runtime-root" => {
                    if root.is_some() { return Err("runtime root was selected more than once"); }
                    root = Some(PathBuf::from(args
                        .get(index + 1)
                        .filter(|value| !value.starts_with("--") && !value.trim().is_empty())
                        .ok_or("runtime root value is missing")?));
                    index += 1;
                }
                "--console-serve" | "--mcp-serve" | "--mcp-web-search-serve" | "--mcp-host-serve" | "--live"
                    if startup_mode.is_none() =>
                {
                    startup_mode = Some(args[index].as_str());
                }
                "--console-serve" | "--mcp-serve" | "--mcp-web-search-serve" | "--mcp-host-serve" | "--live" => {
                    return Err("isolated startup mode was selected more than once");
                }
                "--console-port" if port.is_none() => {
                    let value = args
                        .get(index + 1)
                        .filter(|value| !value.starts_with("--"))
                        .ok_or("isolated runtime requires an explicit non-zero console port")?;
                    let parsed = value
                        .parse::<u16>()
                        .ok()
                        .filter(|value| *value != 0)
                        .ok_or("isolated runtime requires an explicit non-zero console port")?;
                    port = Some(parsed);
                    index += 1;
                }
                "--console-port" => return Err("console port was selected more than once"),
                "--live-groups" if live_groups.is_none() => {
                    live_groups = Some(
                        args.get(index + 1)
                            .filter(|value| !value.starts_with("--"))
                            .map(String::as_str)
                            .ok_or("isolated live runtime requires a non-empty canonical --live-groups list")?,
                    );
                    index += 1;
                }
                "--live-groups" => return Err("live groups were selected more than once"),
                "--no-reconnect" if !no_reconnect => no_reconnect = true,
                "--no-reconnect" => return Err("--no-reconnect was selected more than once"),
                _ => return Err("isolated runtime arguments are not supported by this startup mode"),
            }
            index += 1;
        }

        match startup_mode {
            Some("--live") => {
                let value = live_groups.ok_or(
                    "isolated live runtime requires a non-empty canonical --live-groups list",
                )?;
                validate_canonical_live_groups(value)?;
            }
            Some("--console-serve" | "--mcp-serve" | "--mcp-web-search-serve" | "--mcp-host-serve") if live_groups.is_none() && !no_reconnect => {}
            Some("--console-serve" | "--mcp-serve" | "--mcp-web-search-serve" | "--mcp-host-serve") => {
                return Err("--live-groups and --no-reconnect are only valid with --live");
            }
            Some(_) => unreachable!("startup modes are restricted above"),
            None => return Err("runtime root requires exactly one console, MCP, or live startup mode"),
        }
        Ok((root.ok_or("runtime root value is missing")?,
            port.ok_or("isolated runtime requires an explicit non-zero console port")?))
    }

    fn isolated(root: &Path) -> Result<Self, &'static str> {
        if !root.is_absolute() {
            return Err("runtime root is unavailable: --runtime-root must be an absolute path");
        }
        crate::install_root::check_runtime(root, false)
            .map_err(|_| "development runtime root is under the production install root")?;
        let root_metadata =
            fs::symlink_metadata(root).map_err(|_| "runtime root is unavailable")?;
        if is_reparse_point(&root_metadata) || !root_metadata.is_dir() {
            return Err("runtime root is unavailable");
        }
        #[cfg(test)]
        ROOT_RESOLUTION_TEST_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() { hook(); }
        });
        let root = fs::canonicalize(root).map_err(|_| "runtime root is unavailable")?;
        // Recheck the resolved identity before config/data lookup. An ancestor may have been
        // redirected since the parsed-path check (accident boundary, not attacker isolation).
        crate::install_root::check_runtime(&root, false)
            .map_err(|_| "development runtime root is under the production install root")?;
        if !root.is_dir() {
            return Err("runtime root is unavailable");
        }

        let config_file = contained_path(&root, "config/config.json", false)?;
        let data_dir = contained_path(&root, "data", true)?;
        let resources_dir = contained_path(&root, "resources", true)?;
        // This directory is a selected fixture root, never inherited from DSH_HOME/USERPROFILE.
        // It may be created lazily by an operation that explicitly writes a user-scoped fixture.
        let dsh_user_dir = root.join("user/dsh");
        let dependencies_file = contained_path(&root, "dependencies.json", false)?;
        let state_file = contained_path(&root, "data/social-v2.json", false)?;

        let config_text =
            fs::read_to_string(&config_file).map_err(|_| "runtime config is unavailable")?;
        let raw_config: Value = serde_json::from_str(config_text.trim_start_matches('\u{feff}'))
            .map_err(|_| "runtime config is invalid")?;
        let (config, _) = crate::config::canonical_config(&raw_config)
            .map_err(|_| "runtime config is invalid")?;
        crate::backup::Settings::from_config(&root, &config)
            .map_err(|_| "backup destination is invalid or inside install-root releases, deps or runtime")?;
        // An empty `consoleToken` means a generated token kept in `data/console-token` (JS
        // `loadOrCreateConsoleToken`); with neither the console would have no credential.
        let configured = config
            .get("consoleToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .is_some();
        let from_file = || {
            contained_path(&root, "data/console-token", false)
                .ok()
                .and_then(|path| fs::read_to_string(path).ok())
                .is_some_and(|token| !token.trim().is_empty())
        };
        if !config.is_object() || !(configured || from_file()) {
            return Err("runtime config is invalid");
        }

        let state_text =
            fs::read_to_string(&state_file).map_err(|_| "runtime data is unavailable")?;
        let state: Value =
            serde_json::from_str(&state_text).map_err(|_| "runtime data is invalid")?;
        if !state.is_object() {
            return Err("runtime data is invalid");
        }

        let dependency_text = fs::read_to_string(&dependencies_file)
            .map_err(|_| "runtime dependencies are unavailable")?;
        let dependency_value: Value =
            serde_json::from_str(dependency_text.trim_start_matches('\u{feff}'))
                .map_err(|_| "runtime dependencies are invalid")?;
        let dependency_object = dependency_value
            .as_object()
            .filter(|object| {
                (REQUIRED_DEPENDENCIES.len()
                    ..=REQUIRED_DEPENDENCIES.len() + OPTIONAL_DEPENDENCIES.len())
                    .contains(&object.len())
                    && REQUIRED_DEPENDENCIES
                        .iter()
                        .all(|name| object.contains_key(*name))
                    && object.keys().all(|name| {
                        REQUIRED_DEPENDENCIES.contains(&name.as_str())
                            || OPTIONAL_DEPENDENCIES.contains(&name.as_str())
                    })
            })
            .ok_or("runtime dependencies are invalid")?;
        let mut dependencies = BTreeMap::new();
        for name in REQUIRED_DEPENDENCIES {
            let url = dependency_object
                .get(name)
                .and_then(Value::as_str)
                .filter(|url| is_loopback_http_endpoint(url))
                .ok_or("runtime dependencies must use local test endpoints")?;
            dependencies.insert(name.to_string(), url.to_string());
        }
        if let Some(value) = dependency_object.get("relay") {
            let endpoint = value.as_str().filter(|url| is_loopback_http_endpoint(url))
                .ok_or("runtime relay dependency must use a local endpoint")?;
            dependencies.insert("relay".to_string(), endpoint.to_string());
        }
        if let Some(value) = dependency_object.get("onebot_ws") {
            let url = value
                .as_str()
                .filter(|url| is_loopback_websocket_endpoint(url))
                .ok_or("runtime WebSocket dependency must use a local test endpoint")?;
            dependencies.insert("onebot_ws".to_string(), url.to_string());
        }

        Ok(Self {
            mode: RuntimeMode::Isolated,
            runtime_root: Some(root.clone()),
            config_file,
            data_dir,
            resources_dir,
            dsh_user_dir: Some(dsh_user_dir),
            console_port: None,
            dependencies,
            config: Some(config),
        })
    }

    pub(crate) fn from_root(root: &Path) -> Result<Self, &'static str> {
        Self::isolated(root)
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn resources_dir(&self) -> &Path {
        &self.resources_dir
    }

    pub fn runtime_root(&self) -> Option<&Path> {
        self.runtime_root.as_deref()
    }

    pub fn with_console_port(mut self, port: u16) -> Self {
        self.console_port = Some(port);
        self
    }

    pub fn console_port(&self) -> Option<u16> {
        self.console_port
    }

    /// Resolve an internal relative path under the selected runtime area. Existing symlinks,
    /// junctions/reparse points, non-directory parents, and paths outside the pinned root fail
    /// closed. `WriteFile` creates missing parent directories one component at a time and allows
    /// a missing leaf; it never creates or follows a leaf link.
    pub fn resolve_path(
        &self,
        area: RuntimeArea,
        relative: &str,
        intent: PathIntent,
    ) -> Result<PathBuf, &'static str> {
        let base = self.area_base(area)?;
        if !self.is_isolated() {
            return resolve_legacy_path(&base, relative, intent);
        }
        let root = self.runtime_root.as_deref().ok_or("runtime path is unavailable")?;
        let base_rel = match area {
            RuntimeArea::Config => "config",
            RuntimeArea::Data | RuntimeArea::Memory => "data",
            RuntimeArea::Resources => "resources",
            RuntimeArea::DshUser => "user/dsh",
            RuntimeArea::DshWork => "dsh/work",
            RuntimeArea::Logs => "data",
        };
        resolve_contained_path(root, base_rel, relative, intent).map_err(|_| "runtime path is unavailable")
    }

    fn resolve_path_detailed(&self, area: RuntimeArea, relative: &str, intent: PathIntent) -> Result<PathBuf, String> {
        if !self.is_isolated() { return self.resolve_path(area, relative, intent).map_err(str::to_string); }
        let root = self.runtime_root.as_deref().ok_or("runtime root is unavailable")?;
        let base = match area {
            RuntimeArea::Config => "config",
            RuntimeArea::Data | RuntimeArea::Memory | RuntimeArea::Logs => "data",
            RuntimeArea::Resources => "resources",
            RuntimeArea::DshUser => "user/dsh",
            RuntimeArea::DshWork => "dsh/work",
        };
        resolve_contained_path(root, base, relative, intent)
    }

    pub fn read_file(&self, area: RuntimeArea, relative: &str) -> Result<Vec<u8>, &'static str> {
        let path = self.resolve_path(area, relative, PathIntent::ReadFile)?;
        // Re-resolve immediately before opening to catch a replaced parent/leaf between a prior
        // authorization check and this read. The std filesystem API cannot pin every ancestor
        // handle across Windows junction replacement, so callers still need a trusted runtime root.
        let checked = self.resolve_path(area, relative, PathIntent::ReadFile)?;
        if checked != path {
            return Err("runtime path is unavailable");
        }
        fs::read(path).map_err(|_| "runtime path is unavailable")
    }

    pub fn read_text(&self, area: RuntimeArea, relative: &str) -> Result<String, &'static str> {
        String::from_utf8(self.read_file(area, relative)?).map_err(|_| "runtime file is invalid")
    }

    /// Read an optional selected-root text file. A missing leaf or parent is `Ok(None)`; an
    /// invalid parent, reparse point, non-file leaf, or failed read stays an error. This never
    /// creates directories or consults an ambient or legacy location.
    pub fn read_optional_text(
        &self,
        area: RuntimeArea,
        relative: &str,
    ) -> Result<Option<String>, &'static str> {
        if !self.is_isolated() {
            return Err("optional runtime reads require an isolated context");
        }
        let path = self.resolve_path(area, relative, PathIntent::ReadOptionalFile)?;
        let checked = self.resolve_path(area, relative, PathIntent::ReadOptionalFile)?;
        if checked != path {
            return Err("runtime path is unavailable");
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Ok(metadata) if !is_reparse_point(&metadata) && metadata.is_file() => self
                .read_text(area, relative)
                .map(Some),
            _ => Err("runtime path is unavailable"),
        }
    }

    /// The length of an optional selected-root file (`Ok(0)` when absent), with the guards of
    /// `read_optional_text`.
    pub fn optional_file_len(&self, area: RuntimeArea, relative: &str) -> Result<u64, &'static str> {
        if !self.is_isolated() {
            return Err("optional runtime reads require an isolated context");
        }
        let path = self.resolve_path(area, relative, PathIntent::ReadOptionalFile)?;
        if self.resolve_path(area, relative, PathIntent::ReadOptionalFile)? != path {
            return Err("runtime path is unavailable");
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Ok(metadata) if !is_reparse_point(&metadata) && metadata.is_file() => Ok(metadata.len()),
            _ => Err("runtime path is unavailable"),
        }
    }

    /// Read an optional selected-root file from byte `offset` to its end, with the guards of
    /// `read_optional_text`. Returns where the read started and the bytes from there: `offset`,
    /// or 0 when the file is now shorter than `offset` (it was replaced). Absent is `Ok(None)`.
    pub fn read_optional_tail(
        &self,
        area: RuntimeArea,
        relative: &str,
        offset: u64,
    ) -> Result<Option<(u64, Vec<u8>)>, &'static str> {
        if !self.is_isolated() {
            return Err("optional runtime reads require an isolated context");
        }
        self.read_optional_tail_io(area, relative, offset)
            .map_err(|_| "runtime path is unavailable")
    }

    /// `read_optional_tail` keeping the typed OS error, so a caller with a conflict budget can
    /// retry Windows sharing/lock violations (32/33) on the read.
    pub(crate) fn read_optional_tail_io(
        &self,
        area: RuntimeArea,
        relative: &str,
        offset: u64,
    ) -> std::io::Result<Option<(u64, Vec<u8>)>> {
        use std::io::{Read, Seek, SeekFrom};
        let unavailable = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "runtime path is unavailable");
        let path = self.resolve_path(area, relative, PathIntent::ReadOptionalFile).map_err(|_| unavailable())?;
        if self.resolve_path(area, relative, PathIntent::ReadOptionalFile).map_err(|_| unavailable())? != path {
            return Err(unavailable());
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(metadata) if !is_reparse_point(&metadata) && metadata.is_file() => {}
            Err(error) => return Err(error),
            _ => return Err(unavailable()),
        }
        let mut options = OpenOptions::new();
        options.read(true);
        set_open_no_follow(&mut options);
        let mut file = options.open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || opened_file_is_reparse(&metadata)
            || self.resolve_path(area, relative, PathIntent::ReadOptionalFile).map_err(|_| unavailable())? != path
        {
            return Err(unavailable());
        }
        let start = if metadata.len() < offset { 0 } else { offset };
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some((start, bytes)))
    }

    /// Append a complete caller-provided record to a selected-root file without truncating it.
    /// The caller owns record framing (for example, adding a trailing newline). A whole-file OS
    /// lock serializes cooperating appenders across threads and processes for the full record
    /// write. Isolated paths are resolved before and after opening so a pre-existing leaf or
    /// ancestor reparse point is rejected. `append` and `create` are one open operation. This uses
    /// `std::fs::File::lock` (stable since Rust 1.89):
    /// <https://doc.rust-lang.org/std/fs/struct.File.html#method.lock>
    pub fn append_bytes(
        &self,
        area: RuntimeArea,
        relative: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.append_bytes_inner(area, relative, bytes, false, None)
    }

    /// Append and flush a record before returning, while retaining the append lock.
    pub fn append_bytes_durable(
        &self,
        area: RuntimeArea,
        relative: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.append_bytes_inner(area, relative, bytes, true, None)
    }

    /// Turn-ledger-only conflict policy. Callers under shared control locks must disable retry.
    pub(crate) fn append_turn_ledger_bytes(
        &self,
        bytes: &[u8],
        durable: bool,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), String> {
        self.append_bytes_inner(
            RuntimeArea::Data, crate::turn_ledger::ISOLATED_LEDGER_FILE, bytes, durable, deadline,
        )
    }

    fn append_bytes_inner(
        &self,
        area: RuntimeArea,
        relative: &str,
        bytes: &[u8],
        durable: bool,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), String> {
        let target = self
            .resolve_path(area, relative, PathIntent::WriteFile)
            .map_err(str::to_string)?;
        if self
            .resolve_path(area, relative, PathIntent::WriteFile)
            .map_err(str::to_string)?
            != target
        {
            return Err("runtime path is unavailable".to_string());
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).append(true).create(true);
        set_open_no_follow(&mut options);
        let mut file = append_io(deadline, || {
            #[cfg(test)]
            append_io_test_hook("open")?;
            // Revalidate containment for each open, including after a sharing conflict.
            if self.resolve_path(area, relative, PathIntent::WriteFile).ok().as_ref() != Some(&target) {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "runtime path is unavailable"));
            }
            options.open(&target)
        }).map_err(|error| format!("cannot append runtime file: {}", runtime_io_error("append/open", &error)))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot verify runtime file: {}", runtime_io_error("append/metadata", &error)))?;
        if !metadata.is_file()
            || opened_file_is_reparse(&metadata)
            || self
                .resolve_path(area, relative, PathIntent::WriteFile)
                .map_err(str::to_string)?
                != target
        {
            return Err("runtime path is unavailable".to_string());
        }
        lock_append_file(&file, deadline)
            .map_err(|error| format!("cannot lock runtime append file: {}", runtime_io_error("append/lock", &error)))?;
        #[cfg(test)]
        if durable && relative == crate::send_outbox::ISOLATED_OUTBOX_FILE {
            append_test_hook("locked", bytes)?;
        }
        // A crash can leave a partial last line; the next record must not be glued onto it.
        let broken = append_io(deadline, || ends_mid_line(&mut file))
            .map_err(|error| format!("cannot inspect runtime append file: {}", runtime_io_error("append/tail", &error)))?;
        let write = if broken {
            let mut framed = Vec::with_capacity(bytes.len() + 1);
            framed.push(b'\n');
            framed.extend_from_slice(bytes);
            write_append_all(&mut file, &framed, deadline)
        } else {
            write_append_all(&mut file, bytes, deadline)
        };
        write.map_err(|error| format!("cannot append runtime file: {}", runtime_io_error("append/write", &error)))?;
        if durable {
            #[cfg(test)]
            if matches!(relative, crate::send_outbox::ISOLATED_OUTBOX_FILE | crate::turn_ledger::ISOLATED_LEDGER_FILE) {
                append_test_hook("sync", bytes)?;
            }
            append_io(deadline, || file.sync_all())
                .map_err(|error| format!("cannot sync runtime append file: {}", runtime_io_error("append/sync", &error)))?;
        }
        Ok(())
    }

    /// Create a selected-root file exclusively. Existing files, links, or directories fail in the
    /// atomic `create_new` open; callers can use this for lock acquisition without an
    /// exists-then-create race. Parent directories are created through the contained resolver.
    pub fn create_new_file(
        &self,
        area: RuntimeArea,
        relative: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let target = self
            .resolve_path(area, relative, PathIntent::WriteFile)
            .map_err(str::to_string)?;
        if self
            .resolve_path(area, relative, PathIntent::WriteFile)
            .map_err(str::to_string)?
            != target
        {
            return Err("runtime path is unavailable".to_string());
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_open_no_follow(&mut options);
        let mut file = options
            .open(&target)
            .map_err(|error| format!("cannot exclusively create runtime file: {error}"))?;
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                drop(file);
                let cleanup = self.remove_file(area, relative).err();
                return Err(format!(
                    "cannot verify runtime file: {error}{}",
                    cleanup
                        .map(|message| format!("; cleanup failed: {message}"))
                        .unwrap_or_default()
                ));
            }
        };
        let path_matches = self
            .resolve_path(area, relative, PathIntent::ReadFile)
            .map(|checked| checked == target)
            .unwrap_or(false);
        if !metadata.is_file()
            || opened_file_is_reparse(&metadata)
            || !path_matches
        {
            drop(file);
            let _ = self.remove_file(area, relative);
            return Err("runtime path is unavailable".to_string());
        }
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let cleanup = self.remove_file(area, relative).err();
            return Err(format!(
                "cannot initialize runtime file: {error}{}",
                cleanup
                    .map(|message| format!("; cleanup failed: {message}"))
                    .unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// Open a persistent selected-root gate/marker file and take a nonblocking OS lock.
    ///
    /// Callers must keep the returned handle alive for the ownership period and must not unlink
    /// or atomically replace this file. The OS releases the lock when its handle/process closes,
    /// so a crashed owner can be recovered by opening the same persistent file; marker validation
    /// and updates must happen only while holding the returned handle. Legacy contexts are
    /// rejected so this helper cannot silently select a user or production path. This uses
    /// `std::fs::File::try_lock` (stable since Rust 1.89):
    /// <https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock>
    pub fn open_locked_file(&self, area: RuntimeArea, relative: &str) -> Result<fs::File, String> {
        self.open_file_lock(area, relative, false)
    }

    /// Shared ownership uses the same selected-root path and handle validation as exclusive gates.
    pub fn open_shared_locked_file(
        &self,
        area: RuntimeArea,
        relative: &str,
    ) -> Result<fs::File, String> {
        self.open_file_lock(area, relative, true)
    }

    fn open_file_lock(
        &self,
        area: RuntimeArea,
        relative: &str,
        shared: bool,
    ) -> Result<fs::File, String> {
        if !self.is_isolated() {
            return Err("runtime file locks require an isolated context".to_string());
        }
        let target = self.resolve_path_detailed(area, relative, PathIntent::WriteFile)?;
        if self.resolve_path_detailed(area, relative, PathIntent::WriteFile)? != target {
            return Err("runtime path is unavailable".to_string());
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_open_no_follow(&mut options);
        let file = options
            .open(&target)
            .map_err(|error| runtime_io_error("lock/open", &error))?;
        let metadata = file
            .metadata()
            .map_err(|error| runtime_io_error("lock/metadata", &error))?;
        let path_matches = self
            .resolve_path_detailed(area, relative, PathIntent::ReadFile)
            .map(|checked| checked == target)
            .unwrap_or(false);
        if !metadata.is_file() || opened_file_is_reparse(&metadata) || !path_matches {
            return Err("runtime path is unavailable".to_string());
        }
        match if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        } {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err("runtime file is already locked".to_string());
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(runtime_io_error("lock/acquire", &error));
            }
        }
        let still_selected = self
            .resolve_path_detailed(area, relative, PathIntent::ReadFile)
            .map(|checked| checked == target)
            .unwrap_or(false);
        let locked_metadata = file
            .metadata()
            .map_err(|error| runtime_io_error("lock/metadata", &error))?;
        if !still_selected || opened_file_is_reparse(&locked_metadata) || !locked_metadata.is_file() {
            return Err("runtime path is unavailable".to_string());
        }
        Ok(file)
    }


    /// Coordinate authority publication with selected sticker request transmission.
    pub(crate) fn authority_gate(&self) -> Result<std::fs::File, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match self.open_locked_file(RuntimeArea::Data, "locks/authority.lock") {
                Ok(file) => return Ok(file),
                Err(error) if error == "runtime file is already locked" && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Same-directory temp file plus atomic replace. A process-wide sequence only names temporary
    /// files; the runtime selection itself remains an explicit argument on every call.
    pub fn write_atomic(
        &self,
        area: RuntimeArea,
        relative: &str,
        contents: &[u8],
    ) -> Result<(), String> {
        self.write_atomic_from(area, relative, &mut &contents[..])
    }

    /// Stream large restore payloads through the same fsynced, confined replacement boundary.
    pub(crate) fn write_atomic_from(
        &self,
        area: RuntimeArea,
        relative: &str,
        input: &mut dyn std::io::Read,
    ) -> Result<(), String> {
        let target = self
            .resolve_path_detailed(area, relative, PathIntent::WriteFile)?;
        let parent = target.parent().ok_or_else(|| "runtime path is unavailable".to_string())?;
        let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file_name = target
            .file_name()
            .ok_or_else(|| "runtime path is unavailable".to_string())?
            .to_string_lossy();
        let temp = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), seq));
        self.resolve_path_detailed(area, relative, PathIntent::WriteFile)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| runtime_io_error("temp/create", &e))?;
        if let Err(error) = std::io::copy(input, &mut file).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(runtime_io_error("temp/write", &error));
        }
        drop(file);
        let checked_target = self
            .resolve_path_detailed(area, relative, PathIntent::WriteFile)?;
        let checked_temp = fs::symlink_metadata(&temp)
            .map_err(|e| runtime_io_error("temp/metadata", &e))?;
        if checked_target != target || is_reparse_point(&checked_temp) || !checked_temp.is_file() {
            let _ = fs::remove_file(&temp);
            return Err("runtime path is unavailable".to_string());
        }
        // Social writers already own social-v2.lock. The authority gate is always acquired
        // after it; submission never acquires social-v2.lock while holding authority.
        let authority_file = matches!((area, relative),
            (RuntimeArea::Config, "config.json") |
            (RuntimeArea::Data, "social-v2.json" | "mode.json" | "current-role.json"));
        let _authority = if self.is_isolated() && authority_file {
            #[cfg(test)]
            AUTHORITY_WRITE_TEST_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow_mut().as_mut() { hook(); }
            });
            match self.authority_gate() {
                Ok(gate) => Some(gate),
                Err(error) => { let _ = fs::remove_file(&temp); return Err(error); }
            }
        } else { None };
        // Windows readers can briefly deny deletion of the old leaf. Retry only replacement,
        // retaining the already-fsynced temp; never repeat a mutation or external send.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            // Revalidate confinement on every attempt; path-policy failures are never retried.
            if self.resolve_path_detailed(area, relative, PathIntent::WriteFile)? != target {
                let _ = fs::remove_file(&temp);
                return Err("runtime path changed class=leaf".into());
            }
            match atomic_replace(&temp, &target) {
                Ok(()) => break,
                Err(error) if transient_replace_error(&error) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = fs::remove_file(&temp);
                    return Err(runtime_io_error("leaf/replace", &error));
                }
            }
        }
        // Verify the committed leaf still resolves inside the pinned root.
        self.resolve_path_detailed(area, relative, PathIntent::ReadFile)?;
        Ok(())
    }

    pub fn remove_file(&self, area: RuntimeArea, relative: &str) -> Result<(), String> {
        let path = self
            .resolve_path(area, relative, PathIntent::ReadFile)
            .map_err(str::to_string)?;
        let checked = self
            .resolve_path(area, relative, PathIntent::ReadFile)
            .map_err(str::to_string)?;
        if checked != path {
            return Err("runtime path is unavailable".to_string());
        }
        fs::remove_file(path).map_err(|e| format!("cannot remove runtime file: {e}"))
    }

    pub fn rename_file(
        &self,
        area: RuntimeArea,
        from: &str,
        to: &str,
    ) -> Result<(), String> {
        let source = self
            .resolve_path(area, from, PathIntent::ReadFile)
            .map_err(str::to_string)?;
        let target = self
            .resolve_path(area, to, PathIntent::WriteFile)
            .map_err(str::to_string)?;
        if target.exists() {
            return Err("runtime destination already exists".to_string());
        }
        if self.resolve_path(area, from, PathIntent::ReadFile).map_err(str::to_string)? != source
            || self.resolve_path(area, to, PathIntent::WriteFile).map_err(str::to_string)? != target
        {
            return Err("runtime path is unavailable".to_string());
        }
        fs::rename(source, target).map_err(|e| format!("cannot rename runtime file: {e}"))
    }

    pub fn dependency_url(&self, name: &str) -> Option<&str> {
        self.dependencies.get(name).map(String::as_str)
    }
    pub fn is_isolated(&self) -> bool {
        self.mode == RuntimeMode::Isolated
    }

    /// The console credential this runtime authorizes with (JS `startConsoleServer` and
    /// `readConsoleToken`): the configured `consoleToken`, or, when it is empty in a selected
    /// runtime, the generated token kept only in `data/console-token`. Used for console
    /// authentication, the MCP and wake-snapshot clients and secret redaction; never written back.
    pub fn effective_console_token(&self, cfg: &Value) -> String {
        let configured = cfg.get("consoleToken").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !configured.is_empty() || !self.is_isolated() {
            return configured.to_string();
        }
        self.read_optional_text(RuntimeArea::Data, "console-token")
            .ok()
            .flatten()
            .map(|token| token.trim().to_string())
            .unwrap_or_default()
    }

    pub fn load_config(&self) -> Result<Value, &'static str> {
        if self.is_isolated() {
            let text = self.read_text(RuntimeArea::Config, "config.json")?;
            let raw: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|_| "runtime config is invalid")?;
            if !raw.is_object() {
                return Err("runtime config is invalid");
            }
            // `consoleToken` stays as configured ("" for a generated token, JS): readers that
            // authenticate use `effective_console_token`.
            let (mut config, _) = crate::config::canonical_config(&raw)
                .map_err(|_| "runtime config is invalid")?;
            crate::backup::Settings::from_config(self.runtime_root().ok_or("backup requires a runtime root")?, &config)
                .map_err(|_| "backup destination is invalid or inside install-root releases, deps or runtime")?;
            // The live closed-agent process mirrors DSH's qq-mode owner override here so the
            // console and the turn worker enforce the same current administrator policy.
            let closed = self.read_optional_text(RuntimeArea::Data, "mode.json")?.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .is_some_and(|v| v["mode"] == "closed-agent");
            if closed {
                if let Some(owner) = self.read_optional_text(RuntimeArea::Data, "closed-agent-owner.json")?
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.get("ownerQQ").cloned()) {
                    let (overlay, _) = crate::config::canonical_config(&json!({"ownerQQ":owner})).map_err(|_| "runtime owner policy is invalid")?;
                    if let Some(owner) = overlay.get("ownerQQ") { config["ownerQQ"] = owner.clone(); }
                }
            }
            return Ok(config);
        }

        let text = fs::read_to_string(self.config_file()).unwrap_or_default();
        Ok(serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_else(|_| json!({})))
    }

    pub fn read_social_state(&self) -> Result<Value, String> {
        let bytes = if self.is_isolated() {
            let path = self.resolve_path_detailed(RuntimeArea::Data, "social-v2.json", PathIntent::ReadFile)?;
            if self.resolve_path_detailed(RuntimeArea::Data, "social-v2.json", PathIntent::ReadFile)? != path {
                return Err("path changed class=leaf".into());
            }
            fs::read(path).map_err(|e| runtime_io_error("leaf/read", &e))?
        } else {
            fs::read(LEGACY_SOCIAL_STATE).map_err(|e| runtime_io_error("leaf/read", &e))?
        };
        let text = String::from_utf8(bytes).map_err(|_| "kind=InvalidData class=encoding".to_string())?;
        serde_json::from_str(&text).map_err(|e| format!("social-state/parse kind=InvalidData line={} column={}", e.line(), e.column()))
    }

    pub fn supports_console_request(&self, method: &str, path: &str) -> bool {
        let memory_action = path.strip_prefix("/api/memory/");
        let isolated_memory_route = matches!(method, "GET" | "POST")
            && matches!(
                memory_action,
                Some("write" | "search" | "forget" | "index" | "preview" | "acl" | "settings-get" | "settings-set")
            );
        !self.is_isolated()
            || matches!(
                (method, path),
                ("GET", "/api/social")
                    | ("GET", "/api/dependencies")
                    | ("POST", "/api/dependencies/check" | "/api/dependencies/approve")
                    | ("GET", "/api/releases")
                    | ("GET", "/api/releases/settings")
                    | ("POST", "/api/releases/settings" | "/api/releases/approve")
                    | ("GET", "/api/qq/login")
                    | ("GET", "/api/backups/settings")
                    | ("GET", "/api/backups")
                    | ("POST", "/api/backups/settings")
                    | ("POST", "/api/backups/restore/preview")
                    | ("POST", "/api/backups/restore")
                    | ("POST", "/api/social")
                    | ("POST", "/api/social/state")
                    | ("POST", "/api/social/flush")
                    | ("POST", "/api/socialV2/send-message")
                    | ("POST", "/api/socialV2/send-burst")
                    | ("POST", "/api/socialV2/send-poke")
                    | ("POST", "/api/socialV2/send-voice")
                    | ("POST", "/api/voice/send")
                    | ("POST", "/api/send/voice")
                    | ("POST", "/api/socialV2/mark-read")
                    | ("POST", "/api/socialV2/wake-config")
                    | ("POST", "/api/socialV2/wait")
                    | ("GET", "/api/socialV2/message-detail")
                    | ("GET", "/api/images/message")
                    | ("GET", "/api/socialV2/forward-message")
                    | ("POST", "/api/socialV2/forward-media")
                    | ("GET", "/api/socialV2/sticker-image")
                    | ("GET", "/api/socialV2/self-image")
                    | ("POST", "/api/socialV2/send-sticker")
                    | ("POST", "/api/socialV2/collect-sticker")
                    | ("POST", "/api/send/group")
                    | ("POST", "/api/send/private")
                    | ("POST", "/api/send/reply")
                    | ("GET", "/api/health")
                    | ("GET", "/")
                    | ("GET", "/favicon.ico")
                    | ("GET", "/apple-touch-icon.png")
                    | ("GET", "/api/status")
                    | ("POST", "/api/restart")
                    | ("GET", "/api/pending")
                    | ("GET", "/api/whitelist")
                    | ("POST", "/api/whitelist")
                    | ("GET", "/api/security")
                    | ("POST", "/api/security")
                    | ("POST", "/api/console/token")
                    | ("POST", "/api/authorize/read")
                    | ("GET", "/api/audit/recent")
                    | ("POST", "/api/audit/undo")
                    | ("POST", "/api/socialV2/reset")
                    | ("POST", "/api/socialV2/wake")
                    | ("GET", "/api/model-chain")
                    | ("GET", "/api/session-models")
                    | ("POST", "/api/session-models")
                    | ("GET", "/api/voice/list")
                    | ("GET", "/api/socialV2/voices")
                    | ("GET", "/api/socialV2/tool-log")
                    | ("POST", "/api/socialV2/tool-log/clear")
                    | ("GET", "/api/roles")
                    | ("GET", "/api/roles/content")
                    | ("POST", "/api/roles/create")
                    | ("POST", "/api/roles/update")
                    | ("POST", "/api/roles/rename")
                    | ("POST", "/api/roles/delete")
                    | ("POST", "/api/roles/limit")
                    | ("POST", "/api/role")
                    | ("POST", "/api/role-mode")
                    | ("POST", "/api/mode")
                    | ("GET", "/api/presets")
                    | ("GET", "/api/preset/sim-prompt")
                    | ("POST", "/api/preset/sim-prompt")
                    | ("POST", "/api/preset/sim-prompt/restore")
                    | ("GET", "/api/preset/sim-prompt/backups")
                    | ("GET", "/api/stickers")
                    | ("POST", "/api/stickers/note")
                    | ("POST", "/api/stickers/sync")
                    | ("POST", "/api/stickers/remark")
                    | ("GET", "/api/socialV2/sticker-list")
                    | ("POST", "/api/socialV2/sticker-list")
                    | ("POST", "/api/socialV2/sticker-note")
                    | ("POST", "/api/socialV2/sticker-remark")
                    | ("GET", "/api/tokens/by-provider")
                    | ("GET", "/api/usage/attribution")
                    | ("GET", "/api/tokens/summary")
                    | ("GET", "/api/tokens/series")
                    | ("GET", "/api/tokens/turns")
                    | ("GET", "/api/tokens/recent")
                    | ("GET", "/api/socialV2/memory")
                    | ("POST", "/api/socialV2/memory-append")
                    | ("POST", "/api/socialV2/memory-update")
                    | ("POST", "/api/socialV2/memory-remove")
                    | ("POST", "/api/socialV2/memory-clear")
                    | ("GET", "/api/socialV2/config")
                    | ("POST", "/api/socialV2/config")
                    | ("GET", "/api/socialV2/state")
                    | ("GET", "/api/socialV2/prompt")
                    | ("GET", "/api/socialV2/slang/query")
                    | ("POST", "/api/socialV2/slang/submit")
                    | ("GET", "/api/slang")
                    | ("POST", "/api/slang")
                    | ("POST", "/api/slang/clear")
                    | ("POST", "/api/slang/config")
                    | ("POST", "/api/slang/batch-delete")
                    | ("POST", "/api/slang/batch-confirm")
                    | ("POST", "/api/slang/batch-reject")
                    | ("POST", "/api/slang/research")
                    | ("POST", "/api/slang/extract")
                    | ("GET", "/api/socialV2/initial-prompt")
                    | ("POST", "/api/socialV2/initial-prompt")
                    | ("GET", "/api/socialV2/states")
                    | ("GET", "/api/socialV2/activity")
                    | ("POST", "/api/socialV2/activity")
                    | ("GET", "/api/socialV2/feedback")
                    | ("POST", "/api/socialV2/feedback")
                    | ("POST", "/api/socialV2/feedback-clear")
                    | ("GET", "/api/socialV2/unread")
                    | ("POST", "/api/socialV2/wake-snapshot")
                    | ("GET", "/api/socialV2/recent")
                    | ("GET", "/api/socialV2/my-recent")
                    | ("GET", "/api/socialV2/active-members")
                    | ("GET", "/api/inbound/gaps")
                    | ("POST", "/api/inbound/gaps/retry")
                    | ("GET", "/api/socialV2/send-outbox")
                    | ("GET", "/api/sessions")
                    | ("POST", "/api/session/reset")
                    | ("GET", "/api/turns")
                    | ("POST", "/api/turns/resolve")
                    | ("GET", "/api/recovery-record")
            )
            || (method == "GET" && matches!(path, "/tasks" | "/api/tasks"))
            || (matches!(method, "GET" | "POST") && path.starts_with("/api/tasks/"))
            || (method == "POST" && path == "/api/memory/archive")
            || (method == "GET" && path == "/personas")
            || (matches!(method, "GET" | "POST") && (path == "/api/personas" || path.starts_with("/api/personas/")))
            || path.strip_prefix("/api/slang/").is_some_and(|tail| {
                let parts: Vec<&str> = tail.split('/').collect();
                match (method, parts.as_slice()) {
                    ("PATCH" | "DELETE", [id]) => !id.is_empty(),
                    ("POST", [id, "confirm" | "reject"]) => !id.is_empty(),
                    _ => false,
                }
            })
            || isolated_memory_route
    }

    pub fn health_metadata(&self) -> Value {
        if !self.is_isolated() {
            return Value::Null;
        }
        let mut dependency_names: Vec<&str> = REQUIRED_DEPENDENCIES
            .iter()
            .copied()
            .filter(|name| self.dependency_url(name).is_some())
            .collect();
        for name in OPTIONAL_DEPENDENCIES {
            if self.dependency_url(name).is_some() { dependency_names.push(name); }
        }
        json!({
            "mode": "isolated",
            "configLoaded": self.config.is_some(),
            "dataConfigured": true,
            "resourcesConfigured": self.resources_dir().is_dir(),
            "dependencyNames": dependency_names,
        })
    }

    fn area_base(&self, area: RuntimeArea) -> Result<PathBuf, &'static str> {
        let base = match (self.mode, area) {
            (RuntimeMode::Legacy, RuntimeArea::Config) => PathBuf::from("C:/qqbot/qq-bridge"),
            (RuntimeMode::Legacy, RuntimeArea::Data) => PathBuf::from(LEGACY_DATA),
            (RuntimeMode::Legacy, RuntimeArea::Resources) => PathBuf::from(LEGACY_RESOURCES),
            (RuntimeMode::Legacy, RuntimeArea::DshUser) => match dsh_home() {
                Some(home) => home,
                None => return Err("runtime path is unavailable"),
            },
            (RuntimeMode::Legacy, RuntimeArea::DshWork) => return Err("runtime path is unavailable"),
            (RuntimeMode::Legacy, RuntimeArea::Memory) => PathBuf::from("C:/qqbot/data"),
            (RuntimeMode::Legacy, RuntimeArea::Logs) => PathBuf::from("C:/qqbot/logs"),
            (RuntimeMode::Isolated, RuntimeArea::Config) => self.runtime_root.as_ref().unwrap().join("config"),
            (RuntimeMode::Isolated, RuntimeArea::Data | RuntimeArea::Memory) => self.runtime_root.as_ref().unwrap().join("data"),
            (RuntimeMode::Isolated, RuntimeArea::Resources) => self.resources_dir.clone(),
            (RuntimeMode::Isolated, RuntimeArea::DshUser) => self.dsh_user_dir.clone().unwrap(),
            (RuntimeMode::Isolated, RuntimeArea::DshWork) => self.runtime_root.as_ref().unwrap().join("dsh/work"),
            (RuntimeMode::Isolated, RuntimeArea::Logs) => self.runtime_root.as_ref().unwrap().join("data"),
        };
        Ok(base)
    }
}

/// The DSH home comes only from an explicit `DSH_HOME` or `USERPROFILE`. There is no
/// personal default: when neither is set, callers must fail instead of reading another home.
pub(crate) fn dsh_home() -> Option<PathBuf> {
    dsh_home_from(std::env::var_os("DSH_HOME"), std::env::var_os("USERPROFILE"))
}

fn dsh_home_from(dsh_home: Option<OsString>, user_profile: Option<OsString>) -> Option<PathBuf> {
    dsh_home
        .map(PathBuf::from)
        .or_else(|| user_profile.map(|home| PathBuf::from(home).join(".dsh")))
}

fn resolve_legacy_path(base: &Path, relative: &str, intent: PathIntent) -> Result<PathBuf, &'static str> {
    let components = validated_components(relative)?;
    let mut path = base.to_path_buf();
    for component in components {
        path.push(component);
    }
    match intent {
        PathIntent::ReadFile if !path.is_file() => Err("runtime path is unavailable"),
        PathIntent::ReadDirectory if !path.is_dir() => Err("runtime path is unavailable"),
        PathIntent::CreateDirectory => {
            fs::create_dir_all(&path).map_err(|_| "runtime path is unavailable")?;
            Ok(path)
        }
        PathIntent::WriteFile => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|_| "runtime path is unavailable")?;
            }
            if path.exists() && !path.is_file() {
                return Err("runtime path is unavailable");
            }
            Ok(path)
        }
        _ => Ok(path),
    }
}

fn validated_components(relative: &str) -> Result<Vec<&std::ffi::OsStr>, &'static str> {
    if relative.is_empty() {
        return Ok(Vec::new());
    }
    let path = Path::new(relative);
    let components: Vec<_> = path.components().collect();
    if components.is_empty() {
        return Err("runtime path is unavailable");
    }
    components
        .into_iter()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err("runtime path is unavailable"),
        })
        .collect()
}

pub(crate) fn resolve_contained_path(
    pinned_root: &Path,
    area_relative: &str,
    relative: &str,
    intent: PathIntent,
) -> Result<PathBuf, String> {
    if relative.is_empty()
        && matches!(
            intent,
            PathIntent::ReadFile | PathIntent::ReadOptionalFile | PathIntent::WriteFile
        )
    {
        return Err("runtime path policy rejected class=path".into());
    }
    let root_meta = fs::symlink_metadata(pinned_root).map_err(|e| runtime_io_error("root/metadata", &e))?;
    if is_reparse_point(&root_meta) || !root_meta.is_dir() || fs::canonicalize(pinned_root).ok().as_deref() != Some(pinned_root) {
        return Err("runtime path policy rejected class=path".into());
    }
    let root_components = validated_components(area_relative)?;
    let leaf_components = validated_components(relative)?;
    let leaf_is_area_root = leaf_components.is_empty();
    let root_component_count = root_components.len();
    let mut candidate = pinned_root.to_path_buf();
    let mut all_components = root_components;
    all_components.extend(leaf_components);
    let create_directories = matches!(intent, PathIntent::WriteFile | PathIntent::CreateDirectory);
    for (index, component) in all_components.iter().enumerate() {
        candidate.push(component);
        let is_final = index + 1 == all_components.len();
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err("runtime path policy rejected class=path".into());
                }
                let want_dir = !is_final || leaf_is_area_root || matches!(intent, PathIntent::ReadDirectory | PathIntent::CreateDirectory);
                if want_dir && !metadata.is_dir() || !want_dir && !metadata.is_file() {
                    return Err("runtime path has non-file leaf or non-directory parent".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_directories && !is_final => {
                fs::create_dir(&candidate).or_else(|create_error| {
                    if create_error.kind() == std::io::ErrorKind::AlreadyExists { Ok(()) } else { Err(create_error) }
                }).map_err(|e| runtime_io_error("parent/create", &e))?;
                let metadata = fs::symlink_metadata(&candidate).map_err(|e| runtime_io_error("parent/metadata", &e))?;
                if is_reparse_point(&metadata) || !metadata.is_dir() {
                    return Err("runtime path policy rejected class=path".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && is_final && intent == PathIntent::WriteFile => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && intent == PathIntent::ReadOptionalFile
                    && index >= root_component_count =>
            {
                for remaining in all_components.iter().skip(index + 1) {
                    candidate.push(remaining);
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && is_final && intent == PathIntent::CreateDirectory => {
                fs::create_dir(&candidate).map_err(|e| runtime_io_error("directory/create", &e))?;
                let metadata = fs::symlink_metadata(&candidate).map_err(|e| runtime_io_error("parent/metadata", &e))?;
                if is_reparse_point(&metadata) || !metadata.is_dir() {
                    return Err("runtime path policy rejected class=path".into());
                }
            }
            Err(error) => return Err(runtime_io_error(if is_final {"leaf/metadata"} else {"parent/metadata"}, &error)),
        }
    }
    let canonical = if matches!(intent, PathIntent::WriteFile | PathIntent::ReadOptionalFile)
        && !candidate.exists()
    {
        candidate.clone()
    } else {
        fs::canonicalize(&candidate).map_err(|e| runtime_io_error("path/canonicalize", &e))?
    };
    if !canonical.starts_with(pinned_root) {
        return Err("runtime path policy rejected class=path".into());
    }
    let root_meta = fs::symlink_metadata(pinned_root).map_err(|e| runtime_io_error("root/metadata", &e))?;
    if is_reparse_point(&root_meta) || fs::canonicalize(pinned_root).ok().as_deref() != Some(pinned_root) {
        return Err("runtime path policy rejected class=path".into());
    }
    Ok(canonical)
}

#[cfg(windows)]
fn set_open_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(windows))]
fn set_open_no_follow(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn opened_file_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn opened_file_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn runtime_io_error(class: &str, error: &std::io::Error) -> String {
    format!("class={class} kind={:?} os={:?}", error.kind(), error.raw_os_error())
}

fn transient_replace_error(error: &std::io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

pub(crate) const APPEND_CONFLICT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Pure Windows retry policy, also exercised with injected OS errors on other hosts.
/// Retry only a single I/O operation that has not committed bytes, never a whole record.
#[cfg(any(windows, test))]
pub(crate) fn retry_append_io<T>(
    deadline: std::time::Instant,
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    loop {
        match operation() {
            Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    eprintln!("[turn-ledger] append conflict retry budget exhausted: {}", runtime_io_error("append/conflict", &error));
                    return Err(error);
                }
                #[cfg(test)]
                append_io_test_hook("sleep")?;
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
                if std::time::Instant::now() >= deadline {
                    eprintln!("[turn-ledger] append conflict retry budget exhausted: {}", runtime_io_error("append/conflict", &error));
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

pub(crate) fn append_io<T>(
    deadline: Option<std::time::Instant>,
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    #[cfg(any(windows, test))]
    if let Some(deadline) = deadline {
        // Tests can exercise Windows policy through the real caller on a Unix host.
        #[cfg(test)]
        let windows_policy = cfg!(windows) || APPEND_IO_TEST_HOOK.with(|hook| hook.borrow().is_some());
        #[cfg(not(test))]
        let windows_policy = true;
        if windows_policy { return retry_append_io(deadline, operation); }
    }
    let _ = deadline;
    operation()
}

pub(crate) fn lock_append_file(file: &fs::File, deadline: Option<std::time::Instant>) -> std::io::Result<()> {
    #[cfg(windows)]
    if deadline.is_some() {
        // Only retrying turn-ledger writes replace the base blocking lock with bounded polling.
        return append_io(deadline, || file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => std::io::Error::from_raw_os_error(33),
            fs::TryLockError::Error(error) => error,
        }));
    }
    let _ = deadline;
    file.lock()
}

pub(crate) fn write_append_all(
    file: &mut impl Write,
    mut bytes: &[u8],
    deadline: Option<std::time::Instant>,
) -> std::io::Result<()> {
    if deadline.is_none() { return file.write_all(bytes); }
    while !bytes.is_empty() {
        match append_io(deadline, || file.write(bytes)) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn hf134_control_and_diagnostic_appends_do_not_retry() {
        let root = fixture_root("hf134-control-fast");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap().without_retry();
        let operations: Vec<Box<dyn Fn() -> Result<(), String>>> = vec![
            Box::new(|| turn.end("cancelled", "turnTimeout")),
            Box::new(|| turn.verified("cancel-requested", "turnTimeout")),
            Box::new(|| crate::turn_ledger::record_reset(&context, "group:1")),
            Box::new(|| crate::turn_ledger::record_reset_why(&context, "*", "socialV2-reset")),
            Box::new(|| crate::turn_ledger::record_policy_reset(&context, &json!({}), "reserved2", "access")),
            Box::new(|| crate::turn_ledger::append_record_without_retry(&context, &json!({"k":"begun","turn":"restored"}))),
            Box::new(|| context.append_bytes(RuntimeArea::Data, "logs/rbridge-live.jsonl", b"{}\n")),
        ];
        for operation in operations {
            let opens = std::rc::Rc::new(std::cell::Cell::new(0));
            let sleeps = std::rc::Rc::new(std::cell::Cell::new(0));
            let (seen_opens, seen_sleeps) = (opens.clone(), sleeps.clone());
            APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "sleep" { seen_sleeps.set(seen_sleeps.get() + 1); }
                if stage == "open" {
                    seen_opens.set(seen_opens.get() + 1);
                    if seen_opens.get() == 1 { return Err(std::io::Error::from_raw_os_error(33)); }
                }
                Ok(())
            })));
            let result = operation();
            APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
            assert!(result.is_err(), "control and diagnostic writes return the first error");
            assert_eq!(opens.get(), 1);
            assert_eq!(sleeps.get(), 0);
        }
        assert_eq!(crate::turn_ledger::read_records(&context).unwrap().len(), 1);
        cleanup(&root);
    }

    #[test]
    fn hf134_append_retry_stops_at_deadline_and_returns_other_errors_immediately() {
        for (code, deadline) in [
            (32, std::time::Instant::now()),
            (33, std::time::Instant::now()),
            (5, std::time::Instant::now() + std::time::Duration::from_secs(1)),
        ] {
            let mut calls = 0;
            let error = retry_append_io(deadline, || {
                calls += 1;
                Err::<(), _>(std::io::Error::from_raw_os_error(code))
            }).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
            assert_eq!(calls, 1);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn hf134_non_windows_append_does_not_treat_broken_pipe_as_sharing() {
        let mut calls = 0;
        let error = append_io(Some(std::time::Instant::now() + APPEND_CONFLICT_WAIT), || {
            calls += 1;
            Err::<(), _>(std::io::Error::from_raw_os_error(32))
        }).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!(calls, 1);
    }

    #[test]
    fn hf134_append_keeps_successful_prefixes_on_short_sharing_and_interrupted_writes() {
        struct ShortWriter(Vec<u8>, usize);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.1 += 1;
                match self.1 {
                    2 => return Err(std::io::Error::from_raw_os_error(32)),
                    3 => return Err(std::io::Error::from_raw_os_error(33)),
                    4 => return Err(std::io::ErrorKind::Interrupted.into()),
                    _ => {}
                }
                let written = bytes.len().min(3);
                self.0.extend_from_slice(&bytes[..written]);
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let mut writer = ShortWriter(Vec::new(), 0);
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(|_| Ok(()))));
        let result = write_append_all(&mut writer, b"completed\n", Some(std::time::Instant::now() + APPEND_CONFLICT_WAIT));
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        result.unwrap();
        assert_eq!(writer.0, b"completed\n");
    }

    #[cfg(windows)]
    #[test]
    fn hf134_completed_survives_a_reader_denying_write_sharing() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture_root("hf134-sharing");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap();
        turn.attempt("rpc-hf134", "session-hf134").unwrap();
        let path = root.join("data/turn-ledger.jsonl");
        let holder = OpenOptions::new().read(true).share_mode(1 | 4).open(&path).unwrap();
        let error = OpenOptions::new().read(true).append(true).open(&path).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32), "the second handle must reproduce the reported conflict");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(holder);
        });
        let result = crate::turn_ledger::append_record(&context, &json!({"k":"ended","turn":turn.id,"t":crate::turn_ledger::now_ms(),"outcome":"completed","reason":"synthetic completion"}));
        release.join().unwrap();
        result.unwrap();
        let records = crate::turn_ledger::read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
        let (turns, _) = crate::turn_ledger::fold(&records);
        assert_eq!(turns[0].state, crate::turn_ledger::State::Completed);
        cleanup(&root);
    }

    #[cfg(windows)]
    #[test]
    fn hf134_permanent_sharing_denial_is_bounded_and_content_free() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture_root("hf134-sharing-timeout");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap();
        let holder = OpenOptions::new().read(true).share_mode(1 | 4)
            .open(root.join("data/turn-ledger.jsonl")).unwrap();
        let started = std::time::Instant::now();
        let error = crate::turn_ledger::append_record(&context, &json!({"k":"ended","turn":turn.id,"t":crate::turn_ledger::now_ms(),"outcome":"completed","reason":"private completion contents"})).unwrap_err();
        assert!(started.elapsed() >= APPEND_CONFLICT_WAIT);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(error.contains("class=append/open") && error.contains("os=Some(32)"), "{error}");
        assert!(!error.contains("private completion contents") && !error.contains(&root.to_string_lossy().to_string()));
        drop(holder);
        turn.end("completed", "verified completion").unwrap();
        let records = crate::turn_ledger::read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
        cleanup(&root);
    }

    #[cfg(windows)]
    #[test]
    fn hf134_append_lock_wait_is_bounded() {
        let root = fixture_root("hf134-lock-timeout");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap();
        let holder = OpenOptions::new().read(true).write(true)
            .open(root.join("data/turn-ledger.jsonl")).unwrap();
        holder.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(holder);
        });
        let started = std::time::Instant::now();
        let result = crate::turn_ledger::append_record(&context, &json!({"k":"ended","turn":turn.id,"t":crate::turn_ledger::now_ms(),"outcome":"completed","reason":"synthetic completion"}));
        let elapsed = started.elapsed();
        release.join().unwrap();
        let error = result.unwrap_err();
        assert!(elapsed >= APPEND_CONFLICT_WAIT && elapsed < std::time::Duration::from_secs(3));
        assert!((error.contains("class=turn/read") || error.contains("class=append/lock")) && error.contains("os=Some(33)"), "{error}");
        turn.end("completed", "verified completion").unwrap();
        let records = crate::turn_ledger::read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
        cleanup(&root);
    }


    #[test]
    fn hf134_terminal_sync_failure_is_returned_without_rewriting_the_record() {
        let root = fixture_root("hf134-terminal-sync");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap();
        APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(|stage, bytes| {
            let record: Value = serde_json::from_slice(bytes).unwrap();
            if stage == "sync" && record["k"] == "ended" {
                return Err("injected terminal sync failure".to_string());
            }
            Ok(())
        })));
        let result = turn.end("completed", "synthetic completion");
        APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(result, Err("injected terminal sync failure".to_string()));
        let records = crate::turn_ledger::read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
        cleanup(&root);
    }


    #[test]
    fn hf136_fix1_terminal_decision_read_retries_only_for_workers() {
        for retry in [true, false] {
            let root = fixture_root("hf136-fix1-read");
            let context = RuntimeContext::from_root(&root).unwrap();
            let mut turn = crate::turn_ledger::Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
            if !retry { turn = turn.without_retry(); }
            let reads = std::rc::Rc::new(std::cell::Cell::new(0));
            let seen = reads.clone();
            APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "turn-read" {
                    seen.set(seen.get() + 1);
                    if seen.get() <= 2 { return Err(std::io::Error::from_raw_os_error(if seen.get() == 1 { 32 } else { 33 })); }
                }
                Ok(())
            })));
            let result = turn.end("completed", "ok");
            APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
            cleanup(&root);
            assert_eq!(reads.get(), if retry { 3 } else { 1 });
            assert_eq!(result.is_ok(), retry, "terminal decision read follows the worker's conflict policy");
        }
    }

    #[test]
    fn hf134_outbox_sharing_contention_fails_fast_without_sleep() {
        let root = fixture_root("hf134-outbox-fast");
        let context = RuntimeContext::from_root(&root).unwrap();
        let opens = std::rc::Rc::new(std::cell::Cell::new(0));
        let sleeps = std::rc::Rc::new(std::cell::Cell::new(0));
        let (seen_opens, seen_sleeps) = (opens.clone(), sleeps.clone());
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == "sleep" { seen_sleeps.set(seen_sleeps.get() + 1); }
            if stage == "open" {
                seen_opens.set(seen_opens.get() + 1);
                if seen_opens.get() == 1 { return Err(std::io::Error::from_raw_os_error(32)); }
            }
            Ok(())
        })));
        let result = crate::send_outbox::append_record(&context, &json!({"k":"attempt","batch":"synthetic","idx":0}));
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        cleanup(&root);
        assert!(result.is_err(), "outbox sharing contention must return the first error");
        assert_eq!(opens.get(), 1);
        assert_eq!(sleeps.get(), 0);
    }


    #[test]
    fn hf134_turn_ledger_retries_injected_windows_sharing_and_lock_errors() {
        let root = fixture_root("hf134-turn-sharing");
        let context = RuntimeContext::from_root(&root).unwrap();
        let turn = crate::turn_ledger::Recorder::begin(
            &context, "group:1", "dsh", "message", "epoch", &[], None,
        ).unwrap();
        let opens = std::rc::Rc::new(std::cell::Cell::new(0));
        let sleeps = std::rc::Rc::new(std::cell::Cell::new(0));
        let (seen_opens, seen_sleeps) = (opens.clone(), sleeps.clone());
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == "sleep" { seen_sleeps.set(seen_sleeps.get() + 1); }
            if stage == "open" {
                seen_opens.set(seen_opens.get() + 1);
                match seen_opens.get() {
                    1 => return Err(std::io::Error::from_raw_os_error(32)),
                    2 => return Err(std::io::Error::from_raw_os_error(33)),
                    _ => {}
                }
            }
            Ok(())
        })));
        let result = turn.end("completed", "synthetic completion");
        APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        result.unwrap();
        assert_eq!(opens.get(), 3);
        assert_eq!(sleeps.get(), 2);
        let records = crate::turn_ledger::read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
        assert_eq!(crate::turn_ledger::fold(&records).0[0].state, crate::turn_ledger::State::Completed);
        cleanup(&root);
    }


    #[cfg(windows)]
    #[test]
    fn canonical_runtime_recheck_refuses_ancestor_redirected_into_install_root() {
        if let Some(base) = std::env::var_os("HER_04_CANONICAL_FIXTURE") {
            let base = PathBuf::from(base);
            let selected = base.join("selected");
            let install = base.join("production");
            let original = base.join("original");
            let runtime = selected.join("runtime");
            ROOT_RESOLUTION_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&selected, &original).unwrap();
                create_hidden_junction(&selected, &install).unwrap();
            })));
            assert_eq!(RuntimeContext::from_root(&runtime).err(),
                Some("development runtime root is under the production install root"));
            assert!(!base.join("production/runtime/data/logs").exists());
            return;
        }
        let base = fixture_root("canonical-production-recheck");
        fs::create_dir_all(base.join("selected/runtime")).unwrap();
        fs::create_dir_all(base.join("production/runtime")).unwrap();
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
        let output = cmd.args(["--exact", "runtime::tests::canonical_runtime_recheck_refuses_ancestor_redirected_into_install_root", "--nocapture"])
            .env("HER_04_CANONICAL_FIXTURE", &base)
            .env("HER_INSTALL_ROOT", base.join("production"))
            .output().unwrap();
        cleanup(&base);
        assert!(output.status.success(), "{output:?}");
    }

    #[cfg(windows)]
    #[test]
    fn review_atomic_replace_sharing_failure_is_bounded_and_does_not_change_state() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture_root("review-sharing-bound");
        let context = RuntimeContext::from_root(&root).unwrap();
        let path = root.join("data/social-v2.json");
        let original = fs::read(&path).unwrap();
        let reader = OpenOptions::new().read(true).share_mode(1 | 2).open(&path).unwrap();
        let started = std::time::Instant::now();
        let error = context.write_atomic(RuntimeArea::Data, "social-v2.json", b"{}").unwrap_err();
        println!("sharing-fault: {error}");
        assert!(error.contains("class=leaf/replace") && error.contains("kind=") && error.contains("os=Some("), "I/O cause must survive: {error}");
        assert!(started.elapsed() >= std::time::Duration::from_secs(1));
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "a permanent lock must not hang the state gate");
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!fs::read_dir(root.join("data")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        drop(reader);
        cleanup(&root);
    }

    #[test]
    fn dsh_home_never_defaults_to_a_personal_directory() {
        // Explicit DSH_HOME and USERPROFILE stay the only sources for the DSH home.
        assert_eq!(dsh_home_from(None, None), None);
        assert_eq!(
            dsh_home_from(Some(OsString::from("D:/dsh-home")), None),
            Some(PathBuf::from("D:/dsh-home"))
        );
        assert_eq!(
            dsh_home_from(None, Some(OsString::from("D:/people/alice"))),
            Some(PathBuf::from("D:/people/alice").join(".dsh"))
        );
        assert_ne!(dsh_home_from(None, None), Some(PathBuf::from("C:/Users/me/.dsh")));
    }

    /// Ticket 24 decision B: an empty `consoleToken` falls back to the hardened `data/console-token`
    /// (JS `loadOrCreateConsoleToken`); with neither the runtime refuses to start.
    #[test]
    fn ticket24_empty_config_token_falls_back_to_the_token_file() {
        let root = fixture_root("t24-token-file");
        fs::write(root.join("config/config.json"), r#"{"consoleToken":""}"#).unwrap();
        assert!(RuntimeContext::from_root(&root).is_err(), "no token anywhere must refuse to start");
        fs::write(root.join("data/console-token"), "   \n").unwrap();
        assert!(RuntimeContext::from_root(&root).is_err(), "a blank token file is no token");
        fs::write(root.join("data/console-token"), "file-console-token-abcdefghijabcdef\n").unwrap();
        let context = RuntimeContext::from_root(&root).expect("the token file supplies the console token");
        let cfg = context.load_config().unwrap();
        assert_eq!(cfg["consoleToken"], "", "the configuration keeps \"\" (JS): the file token is never injected");
        assert_eq!(context.effective_console_token(&cfg), "file-console-token-abcdefghijabcdef");
        fs::write(root.join("config/config.json"), r#"{"consoleToken":"config-console-token-abcdefghij"}"#).unwrap();
        let cfg = context.load_config().unwrap();
        assert_eq!(context.effective_console_token(&cfg), "config-console-token-abcdefghij", "a configured token wins");
        cleanup(&root);
    }

    fn fixture_root(label: &str) -> PathBuf {
        // Synthetic runtime data stays under this worktree's backups tree. Chinese characters and
        // spaces exercise the same wide-path behavior used by Windows deployments.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../backups/ticket 03 路径 fixtures")
            .join(format!("{label}-{}-{}", std::process::id(), FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed)));
        let root = crate::install_root::development_root(root);
        fs::create_dir_all(root.join("config")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("resources")).unwrap();
        fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
        fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        ).unwrap();
        root
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }

    fn wait_for_worker(mut child: std::process::Child) -> std::process::Output {
        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            match child.try_wait().expect("poll runtime worker") {
                Some(_) => return child.wait_with_output().expect("collect runtime worker output"),
                None if started.elapsed() < timeout => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                None => {
                    let _ = child.kill();
                    let output = child.wait_with_output().expect("collect timed-out worker output");
                    panic!(
                        "runtime worker exceeded {timeout:?}: stdout={} stderr={}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }
    }

    fn run_locked_file_worker(test_name: &str, root: &Path) -> std::process::Output {
        let mut command = crate::subprocess::command_without_console(
            std::env::current_exe().expect("test executable path"),
        );
        let child = command
            .args(["--ignored", "--exact", test_name])
            .env("HER_LOCK_WORKER_ROOT", root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn runtime lock worker");
        wait_for_worker(child)
    }

    #[cfg(windows)]
    fn create_hidden_junction(link: &Path, target: &Path) -> Result<(), String> {
        use std::os::windows::process::CommandExt;

        // Pass each `mklink` token as its own argument. A pre-quoted command string is
        // re-quoted by `std::process::Command` on Windows before `cmd /c` parses it, which
        // makes paths with spaces unreliable and can fail before the reparse-point assertion.
        let output = std::process::Command::new("cmd.exe")
            .arg("/d")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .creation_flags(0x08000000)
            .output()
            .map_err(|error| format!("cannot launch junction fixture helper: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "junction fixture command failed with {:?}; stdout={:?} (raw={:02X?}); stderr={:?} (raw={:02X?})",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                output.stdout,
                String::from_utf8_lossy(&output.stderr),
                output.stderr,
            ))
        }
    }

    #[test]
    fn isolated_optional_reads_do_not_create_missing_paths_and_reject_reparse_ancestors() {
        let root = fixture_root("optional-read");
        let outside = root.with_extension("external");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("usage.json"), "outside sentinel").unwrap();
        let context = RuntimeContext::isolated(&root).unwrap();

        assert_eq!(
            context
                .read_optional_text(RuntimeArea::Data, "missing/nested.json")
                .unwrap(),
            None
        );
        assert!(!root.join("data/missing").exists());

        let link = root.join("data/external-user-dir");
        #[cfg(windows)]
        create_hidden_junction(&link, &outside).expect("create hidden synthetic junction");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        assert!(context
            .read_optional_text(RuntimeArea::Data, "external-user-dir/usage.json")
            .is_err());
        assert_eq!(fs::read_to_string(outside.join("usage.json")).unwrap(), "outside sentinel");

        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
        cleanup(&root);
        cleanup(&outside);
    }

    #[test]
    fn append_after_a_crash_torn_last_line_starts_a_new_line_in_every_ledger() {
        use serde_json::json;
        let root = fixture_root("torn-ledgers");
        let context = RuntimeContext::isolated(&root).unwrap();
        let torn = "{\"k\":\"intent\",\"batch\":\"b0\"}\n{\"k\":\"att";
        let record = json!({"k": "confirmed", "batch": "b1", "t": 7});
        let ledgers: [(&str, Box<dyn Fn() -> Result<(), String>>); 3] = [
            ("send-outbox.jsonl", Box::new(|| crate::send_outbox::append_record(&context, &record))),
            ("turn-ledger.jsonl", Box::new(|| crate::turn_ledger::append_record(&context, &record))),
            ("token-usage-rust.jsonl", Box::new(|| crate::ledger::append_record_for_runtime(&context, &record))),
        ];
        for (file, append) in &ledgers {
            fs::write(root.join("data").join(file), torn).unwrap();
            append().unwrap();
            append().unwrap();
            let text = fs::read_to_string(root.join("data").join(file)).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 4, "{file}: torn line stays its own line, no blank line is added: {text:?}");
            assert_eq!(lines[1], "{\"k\":\"att", "{file}: the torn fragment is kept as it was");
            for line in &lines[2..] {
                assert_eq!(serde_json::from_str::<serde_json::Value>(line).unwrap(), record, "{file}: {text:?}");
            }
        }
        // A file that already ends in a newline, and an absent file, get no extra line.
        for (file, append) in &ledgers {
            fs::remove_file(root.join("data").join(file)).unwrap();
            append().unwrap();
            append().unwrap();
            let text = fs::read_to_string(root.join("data").join(file)).unwrap();
            assert_eq!(text.lines().count(), 2, "{file}: {text:?}");
            assert!(!text.contains("\n\n"), "{file}: {text:?}");
        }
        cleanup(&root);
    }

    #[test]
    fn legacy_ledger_append_after_a_torn_last_line_starts_a_new_line() {
        let dir = fixture_root("torn-legacy");
        let path = dir.join("data/token-usage-legacy-test.jsonl");
        fs::write(&path, "{\"k\":\"turn\"}\n{\"k\":\"tu").unwrap();
        crate::ledger::append_record_to(path.to_str().unwrap(), &serde_json::json!({"k": "next"})).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\"k\":\"turn\"}\n{\"k\":\"tu\n{\"k\":\"next\"}\n");
        cleanup(&dir);
    }

    #[test]
    fn isolated_append_preserves_records_and_keeps_two_roots_independent() {
        let root_a = fixture_root("append-a");
        let root_b = fixture_root("append-b");
        let context_a = RuntimeContext::isolated(&root_a).unwrap();
        let context_b = RuntimeContext::isolated(&root_b).unwrap();
        context_a
            .append_bytes(RuntimeArea::Logs, "logs/records.jsonl", b"seed\n")
            .unwrap();

        let mut workers = Vec::new();
        for (context, marker) in [(context_a.clone(), "A"), (context_b.clone(), "B")] {
            for worker in 0..4 {
                let context = context.clone();
                let marker = marker.to_string();
                workers.push(std::thread::spawn(move || {
                    for record in 0..40 {
                        let line = format!("{marker}-{worker:02}-{record:04}\n");
                        context
                            .append_bytes(RuntimeArea::Logs, "logs/records.jsonl", line.as_bytes())
                            .unwrap();
                    }
                }));
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let read_lines = |root: &Path| {
            fs::read_to_string(root.join("data/logs/records.jsonl"))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let lines_a = read_lines(&root_a);
        let lines_b = read_lines(&root_b);
        assert_eq!(lines_a.len(), 161);
        assert_eq!(lines_b.len(), 160);
        assert_eq!(lines_a[0], "seed");
        assert!(lines_a[1..].iter().all(|line| line.starts_with("A-")));
        assert!(lines_b.iter().all(|line| line.starts_with("B-")));
        assert_eq!(lines_a[1..].iter().collect::<std::collections::HashSet<_>>().len(), 160);
        assert_eq!(lines_b.iter().collect::<std::collections::HashSet<_>>().len(), 160);

        cleanup(&root_a);
        cleanup(&root_b);
    }

    #[test]
    #[ignore = "spawned only by isolated_append_serializes_large_records_across_processes"]
    fn append_subprocess_worker() {
        let root = std::env::var_os("HER_APPEND_WORKER_ROOT")
            .expect("parent test supplies the synthetic runtime root");
        let marker = std::env::var("HER_APPEND_WORKER_MARKER")
            .expect("parent test supplies a one-byte record marker")
            .into_bytes();
        assert_eq!(marker.len(), 1);
        let count = std::env::var("HER_APPEND_WORKER_COUNT")
            .expect("parent test supplies the record count")
            .parse::<usize>()
            .unwrap();
        let size = std::env::var("HER_APPEND_WORKER_SIZE")
            .expect("parent test supplies the record size")
            .parse::<usize>()
            .unwrap();
        let context = RuntimeContext::isolated(Path::new(&root)).unwrap();
        let mut record = vec![marker[0]; size];
        record.push(b'\n');
        for _ in 0..count {
            context
                .append_bytes(RuntimeArea::Logs, "logs/process-records.jsonl", &record)
                .unwrap();
        }
    }

    #[test]
    #[ignore = "spawned only by runtime_lock_rejects_live_cross_process_owner"]
    fn locked_file_contention_worker() {
        let root = std::env::var_os("HER_LOCK_WORKER_ROOT")
            .expect("parent test supplies the synthetic runtime root");
        let context = RuntimeContext::isolated(Path::new(&root)).unwrap();
        let error = context
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .expect_err("a live owner's file lock must not be reclaimed");
        assert!(error.contains("already locked"), "unexpected lock error: {error}");
    }

    #[test]
    #[ignore = "spawned only by runtime_lock_recovers_after_process_exit"]
    fn locked_file_crash_worker() {
        let root = std::env::var_os("HER_LOCK_WORKER_ROOT")
            .expect("parent test supplies the synthetic runtime root");
        let context = RuntimeContext::isolated(Path::new(&root)).unwrap();
        let mut file = context
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .expect("child process acquires the gate");
        file.set_len(0).unwrap();
        file.write_all(b"crashed-owner-marker\n").unwrap();
        file.sync_all().unwrap();
        std::mem::forget(file);
        // Exit without Rust Drop so the OS must release the handle and its lock.
        std::process::exit(0);
    }

    #[test]
    fn runtime_lock_rejects_live_cross_process_owner() {
        let root = fixture_root("live-file-lock");
        let context = RuntimeContext::isolated(&root).unwrap();
        let owner = context
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .expect("first owner acquires the persistent gate");
        let output = run_locked_file_worker("runtime::tests::locked_file_contention_worker", &root);
        assert!(
            output.status.success(),
            "competing worker failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(root.join("data/locks/session-owner.json").is_file());
        drop(owner);

        let recovered = context
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .expect("dropping a live owner's handle releases the OS lock");
        drop(recovered);
        assert!(root.join("data/locks/session-owner.json").is_file());
        cleanup(&root);
    }

    #[test]
    fn runtime_lock_recovers_after_process_exit_without_unlinking_gate() {
        use std::io::Read as _;
        use std::io::Seek;

        let root = fixture_root("crash-file-lock");
        let output = run_locked_file_worker("runtime::tests::locked_file_crash_worker", &root);
        assert!(
            output.status.success(),
            "crash worker failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let context = RuntimeContext::isolated(&root).unwrap();
        let mut recovered = context
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .expect("OS releases crashed process lock without an application TTL");
        recovered.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut marker = String::new();
        recovered.read_to_string(&mut marker).unwrap();
        assert_eq!(marker, "crashed-owner-marker\n");
        drop(recovered);
        assert!(root.join("data/locks/session-owner.json").is_file());
        cleanup(&root);
    }

    #[test]
    fn isolated_append_serializes_large_records_across_processes() {
        use std::process::Stdio;

        const PROCESS_COUNT: usize = 4;
        const RECORDS_PER_PROCESS: usize = 6;
        const RECORD_SIZE: usize = 128 * 1024;

        let root = fixture_root("append-processes");
        let executable = std::env::current_exe().expect("test executable path");
        let mut children = Vec::new();
        for marker in ["A", "B", "C", "D"] {
            let mut command = crate::subprocess::command_without_console(&executable);
            let child = command
                .args([
                    "--ignored",
                    "--exact",
                    "runtime::tests::append_subprocess_worker",
                ])
                .env("HER_APPEND_WORKER_ROOT", &root)
                .env("HER_APPEND_WORKER_MARKER", marker)
                .env("HER_APPEND_WORKER_COUNT", RECORDS_PER_PROCESS.to_string())
                .env("HER_APPEND_WORKER_SIZE", RECORD_SIZE.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn synthetic append worker");
            children.push(child);
        }
        for child in children {
            let output = wait_for_worker(child);
            assert!(
                output.status.success(),
                "append worker failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let bytes = fs::read(root.join("data/logs/process-records.jsonl")).unwrap();
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|record| !record.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), PROCESS_COUNT * RECORDS_PER_PROCESS);
        let mut markers = std::collections::HashMap::<u8, usize>::new();
        for record in records {
            assert_eq!(record.len(), RECORD_SIZE, "record was truncated or interleaved");
            let marker = record[0];
            assert!(record.iter().all(|byte| *byte == marker), "record bytes interleaved");
            *markers.entry(marker).or_default() += 1;
        }
        assert_eq!(markers.get(&b'A'), Some(&RECORDS_PER_PROCESS));
        assert_eq!(markers.get(&b'B'), Some(&RECORDS_PER_PROCESS));
        assert_eq!(markers.get(&b'C'), Some(&RECORDS_PER_PROCESS));
        assert_eq!(markers.get(&b'D'), Some(&RECORDS_PER_PROCESS));
        cleanup(&root);
    }

    #[test]
    fn isolated_create_new_file_is_atomic_exclusive_and_rejects_unsafe_paths() {
        let root = fixture_root("create-new");
        let context = RuntimeContext::isolated(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let context = context.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let marker = format!("owner-{index}");
                context
                    .create_new_file(RuntimeArea::Data, "locks/claim.lock", marker.as_bytes())
                    .map(|_| marker)
            }));
        }
        let winners = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1, "create_new must admit exactly one caller");
        assert_eq!(
            fs::read_to_string(root.join("data/locks/claim.lock")).unwrap(),
            winners[0]
        );
        assert!(context
            .create_new_file(RuntimeArea::Data, "locks/claim.lock", b"replacement")
            .is_err());
        assert_eq!(
            fs::read_to_string(root.join("data/locks/claim.lock")).unwrap(),
            winners[0]
        );
        assert!(context
            .append_bytes(RuntimeArea::Data, "../outside.json", b"bad")
            .is_err());
        assert!(context
            .create_new_file(RuntimeArea::Data, "../outside.lock", b"bad")
            .is_err());

        cleanup(&root);
    }

    #[test]
    fn append_and_create_reject_data_area_replacement_with_external_junction() {
        let root = fixture_root("append-junction");
        let outside = root.with_extension("append-outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("sentinel.txt"), "outside sentinel").unwrap();
        let context = RuntimeContext::isolated(&root).unwrap();
        let original_data = root.join("data-original");
        fs::rename(root.join("data"), &original_data).unwrap();
        let data_path = root.join("data");
        #[cfg(windows)]
        create_hidden_junction(&data_path, &outside).expect("create hidden synthetic junction");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &data_path).unwrap();

        assert!(context
            .append_bytes(RuntimeArea::Data, "logs/escaped.jsonl", b"must not escape\n")
            .is_err());
        assert!(context
            .create_new_file(RuntimeArea::Data, "locks/escaped.lock", b"must not escape")
            .is_err());
        assert!(context
            .open_locked_file(RuntimeArea::Data, "locks/escaped-owner.json")
            .is_err());
        assert_eq!(fs::read_to_string(outside.join("sentinel.txt")).unwrap(), "outside sentinel");
        assert!(!outside.join("logs/escaped.jsonl").exists());
        assert!(!outside.join("locks/escaped.lock").exists());
        assert!(!outside.join("locks/escaped-owner.json").exists());

        #[cfg(windows)]
        fs::remove_dir(&data_path).unwrap();
        #[cfg(unix)]
        fs::remove_file(&data_path).unwrap();
        fs::rename(original_data, root.join("data")).unwrap();
        cleanup(&root);
        cleanup(&outside);
    }

    #[test]
    fn open_locked_file_requires_an_explicit_isolated_context() {
        assert!(RuntimeContext::legacy()
            .open_locked_file(RuntimeArea::Data, "locks/session-owner.json")
            .is_err());
    }

    #[test]
    fn optional_onebot_websocket_preserves_three_key_roots_and_health_names() {
        let root = fixture_root("onebot-websocket-schema");

        let legacy_shape = RuntimeContext::from_root(&root)
            .expect("the existing three-key dependency fixture remains valid");
        assert_eq!(
            legacy_shape.dependency_url("onebot"),
            Some("http://127.0.0.1:43103")
        );
        assert_eq!(legacy_shape.dependency_url("onebot_ws"), None);
        assert_eq!(
            legacy_shape.health_metadata()["dependencyNames"],
            json!(["dsh", "model", "onebot"])
        );

        fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103","onebot_ws":"ws://127.0.0.1:43104"}"#,
        )
        .unwrap();
        let with_websocket = RuntimeContext::from_root(&root)
            .expect("a literal loopback WebSocket dependency with an explicit port is valid");
        assert_eq!(
            with_websocket.dependency_url("onebot_ws"),
            Some("ws://127.0.0.1:43104")
        );
        assert_eq!(
            with_websocket.health_metadata()["dependencyNames"],
            json!(["dsh", "model", "onebot", "onebot_ws"])
        );
        assert!(!with_websocket.health_metadata().to_string().contains("43104"));

        fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103","onebot_ws":"ws://[::1]:43104"}"#,
        )
        .unwrap();
        let ipv6_websocket = RuntimeContext::from_root(&root)
            .expect("bracketed IPv6 loopback WebSocket dependencies are valid");
        assert_eq!(
            ipv6_websocket.dependency_url("onebot_ws"),
            Some("ws://[::1]:43104")
        );

        cleanup(&root);
    }

    #[test]
    fn optional_onebot_websocket_rejects_nonlocal_or_malformed_urls() {
        let root = fixture_root("onebot-websocket-invalid");
        for endpoint in [
            "http://127.0.0.1:43104",
            "wss://127.0.0.1:43104",
            "ws://localhost:43104",
            "ws://192.0.2.1:43104",
            "ws://127.0.0.1",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1:65536",
            "ws://127.0.0.1:port",
            "ws://127.0.0.1:43104/path",
            "ws://127.0.0.1:43104?query",
            "ws://127.0.0.1:43104#fragment",
            "ws://user@127.0.0.1:43104",
            "ws://::1:43104",
        ] {
            fs::write(
                root.join("dependencies.json"),
                json!({
                    "dsh": "http://127.0.0.1:43101",
                    "model": "http://127.0.0.1:43102",
                    "onebot": "http://127.0.0.1:43103",
                    "onebot_ws": endpoint,
                })
                .to_string(),
            )
            .unwrap();
            assert!(
                RuntimeContext::from_root(&root).is_err(),
                "invalid onebot_ws endpoint was accepted: {endpoint}"
            );
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_dependency_schema_rejects_unknown_keys() {
        let root = fixture_root("onebot-websocket-unknown-key");
        fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103","onebot_ws":"ws://127.0.0.1:43104","extra":"http://127.0.0.1:43105"}"#,
        )
        .unwrap();
        assert!(RuntimeContext::from_root(&root).is_err());
        cleanup(&root);
    }

    #[test]
    fn isolated_send_message_route_allowlist_is_exact() {
        let root = fixture_root("send-message-route-boundary");
        let isolated = RuntimeContext::from_root(&root)
            .expect("the selected synthetic runtime is valid");

        assert!(isolated.supports_console_request("POST", "/api/socialV2/send-message"));
        assert!(isolated.supports_console_request("GET", "/api/socialV2/sticker-image"));
        // Ticket 21 opened the voice sends (exact method and path).
        for path in ["/api/socialV2/send-voice", "/api/voice/send", "/api/send/voice"] {
            assert!(isolated.supports_console_request("POST", path), "{path}");
        }
        for (method, path) in [
            ("GET", "/api/socialV2/send-message"),
            ("POST", "/api/socialV2/send-message/"),
            ("POST", "/api/socialV2/send-message-extra"),
            ("POST", "/api/socialV2/send-poke/"),
            ("GET", "/api/socialV2/send-poke"),
            ("GET", "/api/send/group"),
            ("GET", "/api/send/voice"),
        ] {
            assert!(
                !isolated.supports_console_request(method, path),
                "isolated route unexpectedly allowed {method} {path}"
            );
        }

        let legacy = RuntimeContext::legacy();
        assert!(legacy.supports_console_request("GET", "/api/socialV2/send-message"));
        assert!(legacy.supports_console_request("POST", "/api/socialV2/send-poke"));
        cleanup(&root);
    }

    #[test]
    fn isolated_context_selection_rejects_early_legacy_probe_dispatch() {
        let root = fixture_root("dispatch-guard");
        let args = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--console-serve".to_string(),
            "--console-port".to_string(),
            "43150".to_string(),
            "--send-probe".to_string(),
        ];

        // This exercises selection only; it never invokes main or any legacy probe branch.
        assert!(
            RuntimeContext::from_process_args_with_environment(&args, None).is_err(),
            "isolated startup must reject arguments that can dispatch to a legacy probe"
        );
        cleanup(&root);
    }

    #[test]
    fn isolated_context_selection_requires_one_console_mode_and_explicit_nonzero_port() {
        let root = fixture_root("dispatch-allowlist");
        let valid = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--console-serve".to_string(),
            "--console-port".to_string(),
            "43151".to_string(),
        ];
        let context = RuntimeContext::from_process_args_with_environment(&valid, None)
            .expect("an isolated console invocation with an explicit port is valid");
        assert_eq!(context.console_port(), Some(43151));

        let valid_mcp = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--mcp-serve".to_string(),
            "--console-port".to_string(),
            "43152".to_string(),
        ];
        let mcp_context = RuntimeContext::from_process_args_with_environment(&valid_mcp, None)
            .expect("an isolated MCP invocation with an explicit non-zero port is valid");
        assert_eq!(mcp_context.console_port(), Some(43152));

        let mut mixed_mode = valid.clone();
        mixed_mode.push("--mcp-serve".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&mixed_mode, None).is_err());

        let mut mcp_with_legacy_probe = valid_mcp.clone();
        mcp_with_legacy_probe.push("--send-probe".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&mcp_with_legacy_probe, None).is_err());

        let mcp_without_port = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--mcp-serve".to_string(),
        ];
        assert!(RuntimeContext::from_process_args_with_environment(&mcp_without_port, None).is_err());

        let no_startup_mode = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--console-port".to_string(),
            "43153".to_string(),
        ];
        assert!(RuntimeContext::from_process_args_with_environment(&no_startup_mode, None).is_err());

        let mut missing_port = valid.clone();
        missing_port.pop();
        assert!(RuntimeContext::from_process_args_with_environment(&missing_port, None).is_err());

        let mut zero_port = valid.clone();
        *zero_port.last_mut().unwrap() = "0".to_string();
        assert!(RuntimeContext::from_process_args_with_environment(&zero_port, None).is_err());

        let mut duplicate_port = valid.clone();
        duplicate_port.extend([
            "--console-port".to_string(),
            "43152".to_string(),
        ]);
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_port, None).is_err());
        cleanup(&root);
    }

    #[test]
    fn isolated_load_config_rejects_invalid_documents_and_recovers_per_root() {
        let root_a = fixture_root("config-reload-a");
        let root_b = fixture_root("config-reload-b");
        let context_a = RuntimeContext::isolated(&root_a).unwrap();
        let context_b = RuntimeContext::isolated(&root_b).unwrap();
        let config_a = root_a.join("config/config.json");

        for invalid in ["{ malformed", "null", "[]", "7", "\"scalar\""] {
            fs::write(&config_a, invalid).unwrap();
            assert_eq!(
                context_a.load_config(),
                Err("runtime config is invalid"),
                "isolated config must reject invalid JSON and non-object roots: {invalid}"
            );
            assert_eq!(
                context_b.load_config().unwrap()["consoleToken"],
                "fixture-token",
                "a malformed A config must not affect B"
            );
        }

        fs::write(
            &config_a,
            r#"{"consoleToken":"repaired-a","ownerQQ":9001}"#,
        )
        .unwrap();
        assert_eq!(context_a.load_config().unwrap()["consoleToken"], "repaired-a");
        assert_eq!(
            context_b.load_config().unwrap()["consoleToken"],
            "fixture-token",
            "repairing A must not alter B"
        );

        cleanup(&root_a);
        cleanup(&root_b);
    }

    #[test]
    fn an_invalid_turn_cap_override_never_fails_config_loading() {
        let root = fixture_root("turn-budget-config");
        let context = RuntimeContext::isolated(&root).unwrap();
        // Saves refuse these; a file that has one anyway keeps the bot running on the derived cap.
        for invalid in [json!(454999), json!(-1), json!(500000.5), json!("515000"), json!(null), json!(i64::MAX)] {
            fs::write(root.join("config/config.json"), json!({"consoleToken":"fixture-token",
                "socialV2":{"wake":{"turnHardCapMs":invalid}}}).to_string()).unwrap();
            let config = context.load_config().unwrap_or_else(|e| panic!("turnHardCapMs={invalid} failed loading: {e}"));
            assert_eq!(crate::wake_cfg_of(&config).turn_hard_cap_ms, 515000, "turnHardCapMs={invalid}");
        }
        // cap = max(120000, observation + max_quiet + 5000 + 90000); max_quiet = max(minQuietAfterNewMs, min(120000, wait.maxMs)).
        for (social, expected) in [
            (json!({"wake":{"turnHardCapMs":455000}}),455000),
            (json!({"wake":{"preSleepWaitMs":450000}}),665000),
            // The prompt reads 0 as the 300000 default, so the budget does too.
            (json!({"wake":{"preSleepWaitMs":0}}),515000),
            // The prompt still asks for the observation when the gate is disabled.
            (json!({"wake":{"preSleepWaitEnabled":false}}),515000),
            (json!({"wait":{"maxMs":60000}}),455000),
            (json!({"wait":{"minQuietAfterNewMs":150000}}),545000),
        ] {
            fs::write(root.join("config/config.json"), json!({"consoleToken":"fixture-token",
                "socialV2":social}).to_string()).unwrap();
            let config = context.load_config().unwrap();
            assert_eq!(crate::wake_cfg_of(&config).turn_hard_cap_ms,expected, "{social}");
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_live_selector_requires_canonical_groups_and_selected_websocket() {
        fn live_args(root: Option<&Path>, groups: &str, port: &str) -> Vec<String> {
            let mut args = vec!["her.exe".to_string()];
            if let Some(root) = root {
                args.extend(["--runtime-root".to_string(), root.to_string_lossy().into_owned()]);
            }
            args.extend([
                "--live".to_string(),
                "--live-groups".to_string(),
                groups.to_string(),
                "--console-port".to_string(),
                port.to_string(),
            ]);
            args
        }
        fn write_dependencies(root: &Path, websocket: Option<&str>) {
            let mut value = serde_json::json!({
                "dsh": "http://127.0.0.1:43101",
                "model": "http://127.0.0.1:43102",
                "onebot": "http://127.0.0.1:43103",
            });
            if let Some(websocket) = websocket {
                value["onebot_ws"] = serde_json::json!(websocket);
            }
            fs::write(root.join("dependencies.json"), value.to_string()).unwrap();
        }

        let root = fixture_root("live-selector");
        write_dependencies(&root, Some("ws://127.0.0.1:43104"));
        let valid = live_args(Some(&root), "group:123,private:456", "43154");
        let context = RuntimeContext::from_process_args_with_environment(&valid, None)
            .expect("explicit live mode, canonical groups, port and selected WS are accepted");
        assert_eq!(context.console_port(), Some(43154));
        assert_eq!(context.dependency_url("onebot"), Some("http://127.0.0.1:43103"));
        assert_eq!(context.dependency_url("onebot_ws"), Some("ws://127.0.0.1:43104"));
        let expected_root = fs::canonicalize(&root).unwrap();
        assert_eq!(context.runtime_root(), Some(expected_root.as_path()));

        // The same selector accepts flags in either order and accepts a root from the environment.
        let mut reversed = vec![
            "her.exe".to_string(),
            "--live-groups".to_string(),
            "group:123,private:456".to_string(),
            "--console-port".to_string(),
            "43157".to_string(),
            "--live".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
        ];
        let reversed_context = RuntimeContext::from_process_args_with_environment(&reversed, None)
            .expect("argument order does not change the selected mode or root");
        assert_eq!(reversed_context.runtime_root(), Some(expected_root.as_path()));
        assert_eq!(reversed_context.console_port(), Some(43157));
        let mut live_no_reconnect = reversed.clone();
        let live_flag = live_no_reconnect.iter().position(|arg| arg == "--live").unwrap();
        live_no_reconnect.insert(live_flag, "--no-reconnect".to_string());
        let no_reconnect_context = RuntimeContext::from_process_args_with_environment(&live_no_reconnect, None)
            .expect("one --no-reconnect before --live is valid for an isolated live invocation");
        assert_eq!(no_reconnect_context.console_port(), Some(43157));
        reversed.pop();
        reversed.pop();
        let env_context = RuntimeContext::from_process_args_with_environment(&reversed, Some(root.clone()))
            .expect("HER_RUNTIME_ROOT selection supports the same isolated live arguments");
        assert_eq!(env_context.runtime_root(), Some(expected_root.as_path()));

        for maximum in ["group:9223372036854775807", "private:9223372036854775807"] {
            let args = live_args(Some(&root), maximum, "43154");
            assert!(
                RuntimeContext::from_process_args_with_environment(&args, None).is_ok(),
                "i64::MAX should remain a serviceable canonical target: {maximum}"
            );
        }
        for overflow in [
            "group:9223372036854775808",
            "private:9223372036854775808",
            "group:18446744073709551615",
            "private:18446744073709551615",
            "group:18446744073709551616",
            "private:18446744073709551616",
        ] {
            let args = live_args(Some(&root), overflow, "43154");
            assert!(
                RuntimeContext::from_process_args_with_environment(&args, None).is_err(),
                "unserviceable i64 overflow target was accepted: {overflow}"
            );
        }

        for invalid in [
            "",
            "123",
            "group:0",
            "group:01",
            "group:+1",
            "group:1 ",
            "group:1,",
            "group:1, group:2",
            "user:1",
            "group:1,group:1",
        ] {
            let args = live_args(Some(&root), invalid, "43154");
            assert!(
                RuntimeContext::from_process_args_with_environment(&args, None).is_err(),
                "non-canonical or duplicate live list was accepted: {invalid:?}"
            );
        }

        let mut missing_groups = valid.clone();
        let groups_flag = missing_groups.iter().position(|arg| arg == "--live-groups").unwrap();
        missing_groups.drain(groups_flag..groups_flag + 2);
        assert!(RuntimeContext::from_process_args_with_environment(&missing_groups, None).is_err());
        let mut missing_port = valid.clone();
        let port_flag = missing_port.iter().position(|arg| arg == "--console-port").unwrap();
        missing_port.drain(port_flag..port_flag + 2);
        assert!(RuntimeContext::from_process_args_with_environment(&missing_port, None).is_err());
        let mut zero_port = valid.clone();
        let port_value = zero_port.iter().position(|arg| arg == "--console-port").unwrap() + 1;
        zero_port[port_value] = "0".to_string();
        assert!(RuntimeContext::from_process_args_with_environment(&zero_port, None).is_err());
        let mut duplicate_port = valid.clone();
        duplicate_port.extend(["--console-port".to_string(), "43158".to_string()]);
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_port, None).is_err());

        let mut mixed_console = valid.clone();
        mixed_console.push("--console-serve".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&mixed_console, None).is_err());
        let mut mixed_mcp = valid.clone();
        mixed_mcp.push("--mcp-serve".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&mixed_mcp, None).is_err());
        let mut duplicate_live = valid.clone();
        duplicate_live.push("--live".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_live, None).is_err());
        let mut duplicate_groups = valid.clone();
        duplicate_groups.extend(["--live-groups".to_string(), "group:789".to_string()]);
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_groups, None).is_err());
        let mut duplicate_no_reconnect = valid.clone();
        duplicate_no_reconnect.extend(["--no-reconnect".to_string(), "--no-reconnect".to_string()]);
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_no_reconnect, None).is_err());

        let mut duplicate_root = valid.clone();
        duplicate_root.extend(["--runtime-root".to_string(), root.to_string_lossy().into_owned()]);
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_root, None).is_err());
        assert!(RuntimeContext::from_process_args_with_environment(&valid, Some(root.clone())).is_err());
        let duplicate_console_mode = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--console-serve".to_string(),
            "--console-serve".to_string(),
            "--console-port".to_string(),
            "43159".to_string(),
        ];
        assert!(RuntimeContext::from_process_args_with_environment(&duplicate_console_mode, None).is_err());

        let console = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--console-serve".to_string(),
            "--console-port".to_string(),
            "43160".to_string(),
        ];
        for option in ["--no-reconnect", "--live-groups"] {
            let mut args = console.clone();
            args.push(option.to_string());
            if option == "--live-groups" {
                args.push("group:123".to_string());
            }
            assert!(RuntimeContext::from_process_args_with_environment(&args, None).is_err());
        }
        let mcp_no_reconnect = vec![
            "her.exe".to_string(),
            "--runtime-root".to_string(),
            root.to_string_lossy().into_owned(),
            "--mcp-serve".to_string(),
            "--console-port".to_string(),
            "43161".to_string(),
            "--no-reconnect".to_string(),
        ];
        assert!(RuntimeContext::from_process_args_with_environment(&mcp_no_reconnect, None).is_err());
        let mut mcp_live_groups = mcp_no_reconnect;
        mcp_live_groups.pop();
        mcp_live_groups.extend(["--live-groups".to_string(), "group:123".to_string()]);
        assert!(RuntimeContext::from_process_args_with_environment(&mcp_live_groups, None).is_err());

        // These flags dispatch through legacy probes before the live service loop in main.rs.
        for early_probe in [
            "--send-probe", "--ask", "--memory-probe", "--memory-write-probe",
            "--hot-render", "--index-hint", "--price-probe", "--role-probe",
            "--prompt-wrapped", "--rpc-probe", "--slang-parse-test", "--selftest-401",
            "--ab-probe", "--failover-probe", "--sticker-sync", "--wake-replay",
            "--build-prompt", "--dsh-dump", "--dsh-watch", "--dsh-probe", "--dsh-demo",
            "--question-probe", "--split-probe", "--sensitive-probe", "--backend-ask",
            "--backends", "--replay", "--live-all", "--ab-full", "--shadow-turn",
        ] {
            let mut args = valid.clone();
            args.push(early_probe.to_string());
            assert!(
                RuntimeContext::from_process_args_with_environment(&args, None).is_err(),
                "legacy early dispatch flag was accepted in isolated live mode: {early_probe}"
            );
        }

        for websocket in [
            "wss://127.0.0.1:43104",
            "ws://example.invalid:43104",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1",
            "ws://127.0.0.1:43104/path",
        ] {
            write_dependencies(&root, Some(websocket));
            let args = live_args(Some(&root), "group:123", "43154");
            assert!(
                RuntimeContext::from_process_args_with_environment(&args, None).is_err(),
                "invalid or non-loopback selected onebot_ws was accepted: {websocket}"
            );
        }

        write_dependencies(&root, Some("ws://127.0.0.1:43104"));
        let no_ws_root = fixture_root("live-selector-missing-ws");
        let no_ws = live_args(Some(&no_ws_root), "group:123", "43155");
        assert!(
            RuntimeContext::from_process_args_with_environment(&no_ws, None).is_err(),
            "isolated live must fail closed without selected onebot_ws"
        );
        let mut no_reconnect_console = console;
        no_reconnect_console.push("--no-reconnect".to_string());
        assert!(RuntimeContext::from_process_args_with_environment(&no_reconnect_console, None).is_err());
        cleanup(&root);
        cleanup(&no_ws_root);
    }

    #[test]
    fn no_runtime_root_keeps_legacy_probe_arguments_accepted() {
        let args = vec!["her.exe".to_string(), "--send-probe".to_string()];
        let context = RuntimeContext::from_process_args_with_environment(&args, None)
            .expect("no-root arguments continue to select the legacy context");
        assert!(!context.is_isolated());
    }

    #[test]
    fn isolated_memory_http_allowlist_exposes_only_context_safe_actions() {
        let root = fixture_root("memory-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        for action in [
            "write",
            "search",
            "forget",
            "index",
            "preview",
            "acl",
            "settings-get",
            "settings-set",
        ] {
            assert!(context.supports_console_request("POST", &format!("/api/memory/{action}")));
            assert!(context.supports_console_request("GET", &format!("/api/memory/{action}")));
        }
        for (method, path) in [
            ("GET", "/api/memory/archive"),
            ("GET", "/api/memory/settings"),
            ("GET", "/api/memory/acl/"),
            ("GET", "/api/memory/"),
            ("DELETE", "/api/memory/write"),
            ("PUT", "/api/memory/acl"),
        ] {
            assert!(!context.supports_console_request(method, path), "unexpectedly opened {method} {path}");
        }
        assert!(context.supports_console_request("POST", "/api/memory/archive"));
        cleanup(&root);
    }

    #[test]
    fn isolated_send_message_http_allowlist_is_method_and_path_exact() {
        let root = fixture_root("send-message-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        assert!(context.supports_console_request("POST", "/api/socialV2/send-message"));
        for (method, path) in [
            ("GET", "/api/socialV2/send-message"),
            ("POST", "/api/socialV2/send-message/"),
            ("POST", "/api/socialV2/send-burst/"),
            ("POST", "/api/socialV2/feedback/"),
            ("POST", "/api/socialV2/check-send"),
        ] {
            assert!(
                !context.supports_console_request(method, path),
                "unexpectedly opened {method} {path}"
            );
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_sticker_list_http_allowlist_is_method_path_and_query_exact() {
        let root = fixture_root("sticker-list-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();

        for path in ["/api/stickers", "/api/socialV2/sticker-list"] {
            assert!(context.supports_console_request("GET", path));
        }
        assert!(context.supports_console_request("POST", "/api/stickers/note"));
        assert!(context.supports_console_request("POST", "/api/stickers/sync"));
        assert!(context.supports_console_request("POST", "/api/stickers/remark"));
        for path in [
            "/api/socialV2/sticker-list",
            "/api/socialV2/sticker-note",
            "/api/socialV2/sticker-remark",
            "/api/socialV2/send-sticker",
            "/api/socialV2/collect-sticker",
        ] {
            assert!(context.supports_console_request("POST", path), "POST {path}");
        }
        // parse_request separates query data from Request.path before this policy check.
        // A raw query-bearing string is not itself an allowlisted route path.
        for (method, path) in [
            ("POST", "/api/stickers"),
            ("HEAD", "/api/stickers"),
            ("OPTIONS", "/api/stickers"),
            ("HEAD", "/api/socialV2/sticker-list"),
            ("OPTIONS", "/api/socialV2/sticker-list"),
            ("GET", "/api/stickers/"),
            ("GET", "/api/socialV2/sticker-list/"),
            ("GET", "/api/stickers/sync"),
            ("GET", "/api/stickers/note"),
            ("GET", "/api/stickers/remark"),
            ("POST", "/api/stickers/remark/"),
            ("POST", "/api/stickers/remark?x=1"),
            ("GET", "/api/stickers/unknown"),
            ("GET", "/api/stickers2"),
            ("GET", "/api/socialV2/sticker-note"),
            ("GET", "/api/stickers?query=cat&count=12"),
            ("GET", "/api/socialV2/sticker-list?query=cat&count=12"),
            ("GET", "/api/socialV2/sticker-lists"),
            ("GET", "/api/socialV2/sticker-list-extra"),
            ("GET", "/api/socialV2/unknown"),
        ] {
            assert!(
                !context.supports_console_request(method, path),
                "unexpectedly opened {method} {path}"
            );
        }

        let legacy = RuntimeContext::legacy();
        assert!(legacy.supports_console_request("POST", "/api/stickers/note"));
        assert!(legacy.supports_console_request("POST", "/api/socialV2/sticker-list"));
        cleanup(&root);
    }

    #[test]
    fn isolated_mode_switch_route_is_post_only() {
        let root = fixture_root("mode-switch-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        assert!(context.supports_console_request("POST", "/api/mode"));
        for (method, path) in [("GET", "/api/mode"), ("PUT", "/api/mode"), ("POST", "/api/mode/"), ("POST", "/api/mode/extra")] {
            assert!(!context.supports_console_request(method, path), "unexpectedly opened {method} {path}");
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_role_management_http_allowlist_is_method_and_path_exact() {
        let root = fixture_root("role-management-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        for path in [
            "/api/roles/update",
            "/api/roles/rename",
            "/api/roles/delete",
            "/api/roles/limit",
            "/api/role",
            "/api/role-mode",
        ] {
            assert!(
                context.supports_console_request("POST", path),
                "expected exact role-management route POST {path} to be allowed"
            );
        }
        for (method, path) in [
            ("GET", "/api/roles/update"),
            ("PUT", "/api/roles/update"),
            ("GET", "/api/roles/rename"),
            ("POST", "/api/roles/rename/"),
            ("POST", "/api/roles/delete/extra"),
            ("GET", "/api/roles/limit"),
            ("POST", "/api/role/"),
            ("GET", "/api/role-mode"),
            ("POST", "/api/role-mode/extra"),
        ] {
            assert!(
                !context.supports_console_request(method, path),
                "unexpectedly opened role-management route {method} {path}"
            );
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_usage_and_social_memory_state_allowlist_is_method_and_path_exact() {
        let root = fixture_root("usage-social-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        for (method, path) in [
            ("GET", "/api/tokens/by-provider"),
            ("GET", "/api/usage/attribution"),
            ("GET", "/api/tokens/summary"),
            ("GET", "/api/tokens/series"),
            ("GET", "/api/tokens/turns"),
            ("GET", "/api/tokens/recent"),
            ("GET", "/api/socialV2/memory"),
            ("POST", "/api/socialV2/memory-append"),
            ("POST", "/api/socialV2/memory-update"),
            ("POST", "/api/socialV2/memory-remove"),
            ("POST", "/api/socialV2/memory-clear"),
            ("GET", "/api/socialV2/config"),
            ("POST", "/api/socialV2/config"),
            ("GET", "/api/socialV2/state"),
            ("GET", "/api/socialV2/states"),
            ("GET", "/api/socialV2/activity"),
            ("POST", "/api/socialV2/activity"),
            ("GET", "/api/socialV2/unread"),
            ("GET", "/api/socialV2/prompt"),
            ("GET", "/api/socialV2/initial-prompt"),
            ("POST", "/api/socialV2/initial-prompt"),
            ("GET", "/api/socialV2/recent"),
            ("GET", "/api/socialV2/my-recent"),
            ("GET", "/api/socialV2/active-members"),
            ("GET", "/api/socialV2/send-outbox"),
            ("GET", "/api/sessions"),
            ("GET", "/api/pending"),
            ("POST", "/api/session/reset"),
            ("GET", "/api/turns"),
            ("POST", "/api/turns/resolve"),
            ("GET", "/api/recovery-record"),
            ("POST", "/api/restart"),
            // Ticket 24 opened access, security, token, audit, reset and manual wake.
            ("POST", "/api/whitelist"),
            ("GET", "/api/security"),
            ("POST", "/api/security"),
            ("POST", "/api/console/token"),
            ("POST", "/api/authorize/read"),
            ("GET", "/api/audit/recent"),
            ("POST", "/api/audit/undo"),
            ("POST", "/api/socialV2/reset"),
            ("POST", "/api/socialV2/wake"),
        ] {
            assert!(
                context.supports_console_request(method, path),
                "expected exact route {method} {path} to be allowed"
            );
        }
        for (method, path) in [
            ("POST", "/api/tokens/reset"),
            ("POST", "/api/tokens/summary"),
            ("GET", "/api/tokens/summary/"),
            ("GET", "/api/tokens/unknown"),
            ("GET", "/api/usage/attribution/extra"),
            ("POST", "/api/socialV2/memory"),
            ("GET", "/api/socialV2/memory-append"),
            ("POST", "/api/socialV2/memory-append/"),
            ("PUT", "/api/socialV2/config"),
            ("POST", "/api/socialV2/state"),
            ("PUT", "/api/socialV2/activity"),
            ("GET", "/api/socialV2/mark-read"),
            ("GET", "/api/socialV2/wake-config"),
            ("GET", "/api/socialV2/states/"),
            ("GET", "/api/pending/"),
            ("POST", "/api/pending"),
            ("GET", "/api/turns/"),
            ("GET", "/api/session/reset"),
            ("POST", "/api/sessions"),
            ("PUT", "/api/socialV2/initial-prompt"),
            ("POST", "/api/socialV2/prompt"),
            ("GET", "/api/socialV2/initial-prompt/"),
            ("GET", "/api/audit/recent/"),
            ("GET", "/api/audit/undo"),
            ("PUT", "/api/security"),
            ("GET", "/api/console/token"),
            ("GET", "/api/socialV2/reset"),
            ("GET", "/api/socialV2/wake"),
            ("POST", "/api/socialV2/wake/"),
            ("POST", "/api/workspace/reset"),
        ] {
            assert!(
                !context.supports_console_request(method, path),
                "unexpectedly opened {method} {path}"
            );
        }
        let legacy = RuntimeContext::legacy();
        assert!(legacy.supports_console_request("POST", "/api/tokens/reset"));
        assert!(legacy.supports_console_request("GET", "/api/pending"));
        cleanup(&root);
    }

    #[test]
    fn isolated_resource_http_allowlist_is_method_and_path_exact() {
        let root = fixture_root("resource-http-allowlist");
        let context = RuntimeContext::isolated(&root).unwrap();
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/favicon.ico"),
            ("GET", "/apple-touch-icon.png"),
            ("GET", "/api/voice/list"),
            ("GET", "/api/socialV2/voices"),
            ("GET", "/api/socialV2/tool-log"),
            ("POST", "/api/socialV2/tool-log/clear"),
        ] {
            assert!(
                context.supports_console_request(method, path),
                "expected exact resource route {method} {path} to be allowed"
            );
        }
        for (method, path) in [
            ("POST", "/"),
            ("GET", "/favicon.ico/"),
            ("POST", "/apple-touch-icon.png"),
            ("POST", "/api/voice/list"),
            ("POST", "/api/socialV2/voices"),
            ("POST", "/api/socialV2/tool-log"),
            ("GET", "/api/socialV2/tool-log/clear"),
            ("POST", "/api/socialV2/tool-log/clear/"),
            ("GET", "/api/voice/list/extra"),
        ] {
            assert!(
                !context.supports_console_request(method, path),
                "unexpectedly opened resource route {method} {path}"
            );
        }
        cleanup(&root);
    }

    #[test]
    fn isolated_resolver_creates_parents_and_keeps_two_contexts_separate_concurrently() {
        let root_a = fixture_root("a");
        let root_b = fixture_root("b");
        let ctx_a = RuntimeContext::isolated(&root_a).unwrap();
        let ctx_b = RuntimeContext::isolated(&root_b).unwrap();

        let a = std::thread::spawn(move || {
            for n in 0..40 {
                ctx_a.write_atomic(RuntimeArea::Data, "nested/state.json", format!(r#"{{"owner":"a","n":{n}}}"#).as_bytes()).unwrap();
                let value: Value = serde_json::from_slice(&ctx_a.read_file(RuntimeArea::Data, "nested/state.json").unwrap()).unwrap();
                assert_eq!(value["owner"], "a");
            }
            ctx_a
        });
        let b = std::thread::spawn(move || {
            for n in 0..40 {
                ctx_b.write_atomic(RuntimeArea::Data, "nested/state.json", format!(r#"{{"owner":"b","n":{n}}}"#).as_bytes()).unwrap();
                let value: Value = serde_json::from_slice(&ctx_b.read_file(RuntimeArea::Data, "nested/state.json").unwrap()).unwrap();
                assert_eq!(value["owner"], "b");
            }
            ctx_b
        });
        let _ctx_a = a.join().unwrap();
        let _ctx_b = b.join().unwrap();
        let value_a: Value = serde_json::from_slice(&fs::read(root_a.join("data/nested/state.json")).unwrap()).unwrap();
        let value_b: Value = serde_json::from_slice(&fs::read(root_b.join("data/nested/state.json")).unwrap()).unwrap();
        assert_eq!(value_a["owner"], "a");
        assert_eq!(value_b["owner"], "b");
        cleanup(&root_a);
        cleanup(&root_b);
    }

    #[test]
    fn isolated_resolver_rejects_traversal_and_detects_replaced_root() {
        let root = fixture_root("root-revalidate");
        let context = RuntimeContext::isolated(&root).unwrap();
        assert!(context.resolve_path(RuntimeArea::Data, "../outside.json", PathIntent::WriteFile).is_err());
        let moved = root.with_extension("moved");
        fs::rename(&root, &moved).unwrap();
        assert!(context.read_file(RuntimeArea::Data, "social-v2.json").is_err());
        cleanup(&moved);
    }

    #[test]
    fn isolated_optional_reads_do_not_create_missing_paths_and_reject_leaf_links() {
        let root = fixture_root("optional-read");
        let outside = root.with_extension("outside");
        fs::write(&outside, "outside sentinel").unwrap();
        let context = RuntimeContext::isolated(&root).unwrap();

        assert_eq!(
            context
                .read_optional_text(RuntimeArea::Data, "missing/nested.json")
                .unwrap(),
            None
        );
        assert!(!root.join("data/missing").exists());

        let link = root.join("data/optional-link.json");
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, &link);
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link);
        if linked.is_ok() {
            assert!(context
                .read_optional_text(RuntimeArea::Data, "optional-link.json")
                .is_err());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside sentinel");
        }

        cleanup(&root);
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn isolated_resolver_rejects_existing_leaf_symlink_when_platform_allows_creation() {
        let root = fixture_root("leaf-link");
        let outside = root.with_extension("outside");
        fs::write(&outside, "outside").unwrap();
        let link = root.join("data/linked.json");
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, &link);
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link);
        if linked.is_ok() {
            let context = RuntimeContext::isolated(&root).unwrap();
            assert!(context.read_file(RuntimeArea::Data, "linked.json").is_err());
            assert!(context.write_atomic(RuntimeArea::Data, "linked.json", b"replacement").is_err());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
        }
        cleanup(&root);
        let _ = fs::remove_file(outside);
    }
}

pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}
fn contained_path(root: &Path, relative: &str, directory: bool) -> Result<PathBuf, &'static str> {
    let root_metadata = fs::symlink_metadata(root).map_err(|_| "runtime layout is incomplete")?;
    if is_reparse_point(&root_metadata) || !root_metadata.is_dir() {
        return Err("runtime layout is incomplete");
    }
    let canonical_root = fs::canonicalize(root).map_err(|_| "runtime layout is incomplete")?;
    if canonical_root != root {
        return Err("runtime layout is incomplete");
    }

    let components: Vec<_> = Path::new(relative).components().collect();
    if components.is_empty() {
        return Err("runtime layout is incomplete");
    }
    let mut candidate = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("runtime layout is incomplete");
        };
        candidate.push(name);
        let metadata =
            fs::symlink_metadata(&candidate).map_err(|_| "runtime layout is incomplete")?;
        if is_reparse_point(&metadata) {
            return Err("runtime layout is incomplete");
        }
        let is_final = index + 1 == components.len();
        let wrong_type = if is_final {
            if directory {
                !metadata.is_dir()
            } else {
                !metadata.is_file()
            }
        } else {
            !metadata.is_dir()
        };
        if wrong_type {
            return Err("runtime layout is incomplete");
        }
    }

    let canonical = fs::canonicalize(&candidate).map_err(|_| "runtime layout is incomplete")?;
    if !canonical.starts_with(&canonical_root) {
        return Err("runtime layout is incomplete");
    }
    let metadata = fs::metadata(&canonical).map_err(|_| "runtime layout is incomplete")?;
    if directory && !metadata.is_dir() || !directory && !metadata.is_file() {
        return Err("runtime layout is incomplete");
    }
    Ok(canonical)
}

pub(crate) fn is_loopback_http_endpoint(url: &str) -> bool {
    let Some(authority) = url.strip_prefix("http://") else {
        return false;
    };
    if authority
        .chars()
        .any(|ch| matches!(ch, '/' | '?' | '#' | '@'))
    {
        return false;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, rest)) = bracketed.split_once(']') else {
            return false;
        };
        let Some(port) = rest.strip_prefix(':') else {
            return false;
        };
        (host, port)
    } else {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return false;
        };
        (host, port)
    };
    if port.parse::<u16>().map(|value| value != 0).unwrap_or(false) == false {
        return false;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

pub(crate) fn validate_canonical_live_groups(value: &str) -> Result<(), &'static str> {
    use std::collections::BTreeSet;

    if value.is_empty() {
        return Err("isolated live runtime requires a non-empty canonical --live-groups list");
    }
    let mut seen = BTreeSet::new();
    for entry in value.split(',') {
        let (kind, id) = entry
            .split_once(':')
            .ok_or("isolated live groups must use canonical group:<id> or private:<id> entries")?;
        if !matches!(kind, "group" | "private")
            || id.is_empty()
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("isolated live groups must use canonical group:<id> or private:<id> entries");
        }
        let parsed = id
            .parse::<i64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or("isolated live identifiers must be positive canonical i64 decimal values")?;
        if parsed.to_string() != id || !seen.insert(entry) {
            return Err("isolated live groups must not contain non-canonical or duplicate entries");
        }
    }
    Ok(())
}

pub(crate) fn is_loopback_websocket_endpoint(url: &str) -> bool {
    let Some(authority) = url.strip_prefix("ws://") else {
        return false;
    };
    if authority
        .chars()
        .any(|ch| matches!(ch, '/' | '?' | '#' | '@' | '\\'))
    {
        return false;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, rest)) = bracketed.split_once(']') else {
            return false;
        };
        let Some(port) = rest.strip_prefix(':') else {
            return false;
        };
        (host, port)
    } else {
        let Some((host, port)) = authority.split_once(':') else {
            return false;
        };
        (host, port)
    };
    if !port.parse::<u16>().map(|value| value != 0).unwrap_or(false) {
        return false;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}
