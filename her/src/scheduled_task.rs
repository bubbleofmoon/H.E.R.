//! The single HER logon task. Rust owns its definition and the Scheduler boundary.
#![allow(dead_code)]
use crate::install_root::InstallRoot;
use serde::Serialize;
use std::path::PathBuf;

pub const TASK_NAME: &str = "HER";
const FAKE_ENV: &str = "HER_TASK_SCHEDULER_FILE";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinition {
    pub user_id: String,
    pub command: String,
    pub arguments: String,
    pub working_directory: String,
}

impl TaskDefinition {
    pub fn for_install(install: &InstallRoot) -> Result<Self, String> {
        let path = |suffix: &str| crate::subprocess::external_path(install.root().join(suffix));
        Ok(Self {
            user_id: current_user()?,
            command: path("current/her.exe")?,
            arguments: format!(
                "supervise --runtime-root {} --install-root {}",
                quote_argument(&path("runtime")?),
                quote_argument(&crate::subprocess::external_path(install.root())?)
            ),
            working_directory: path("current")?,
        })
    }

    pub fn xml(&self) -> String {
        format!(concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
            "<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>",
            "<Principals><Principal id=\"Installer\"><UserId>{user}</UserId>",
            "<LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>",
            "<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<Enabled>true</Enabled><Hidden>true</Hidden><ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>",
            "<Actions Context=\"Installer\"><Exec><Command>{command}</Command><Arguments>{arguments}</Arguments>",
            "<WorkingDirectory>{cwd}</WorkingDirectory></Exec></Actions></Task>"
        ), user = escape_xml(&self.user_id), command = escape_xml(&self.command),
            arguments = escape_xml(&self.arguments), cwd = escape_xml(&self.working_directory))
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// CommandLineToArgvW/CRT quoting, including a root ending in a backslash.
fn quote_argument(text: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in text.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            slashes * if ch == '"' { 2 } else { 1 },
        ));
        if ch == '"' {
            result.push('\\');
        }
        result.push(ch);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

pub trait TaskScheduler {
    fn install(&self, definition: &TaskDefinition) -> Result<(), String>;
    fn read(&self) -> Result<Option<String>, String>;
    fn remove(&self) -> Result<(), String>;
    fn compare(&self, definition: &TaskDefinition) -> Result<Comparison, String> {
        let Some(xml) = self.read()? else {
            return Ok(Comparison {
                matches: false,
                reason: Some("HER task is missing".into()),
                fingerprint: drift_fingerprint("missing"),
            });
        };
        let expected = semantics(&definition.xml())?;
        let actual = semantics(&xml)?;
        let difference = expected
            .keys()
            .chain(actual.keys())
            .find(|key| !task_values_match(key, expected.get(*key), actual.get(*key)));
        Ok(Comparison {
            matches: difference.is_none(),
            reason: difference.map(|key| format!("definition differs at {key}")),
            fingerprint: drift_fingerprint((&expected, &actual)),
        })
    }
}

fn task_values_match(_path: &str, expected: Option<&String>, actual: Option<&String>) -> bool {
    #[cfg(windows)]
    if let Some((parent, element)) = _path.rsplit_once('/') {
        let parent = parent.rsplit('/').next().unwrap_or("");
        if element.starts_with("UserId[")
            && (parent.starts_with("Principal[") || parent.starts_with("LogonTrigger["))
        {
            return match (
                expected.and_then(|s| account_sid(s)),
                actual.and_then(|s| account_sid(s)),
            ) {
                (Some(expected), Some(actual)) => expected == actual,
                _ => false,
            };
        }
    }
    expected == actual
}

#[cfg(windows)]
fn account_sid(account: &str) -> Option<Vec<u8>> {
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, HLOCAL},
            Security::{
                Authorization::ConvertStringSidToSidW, GetLengthSid, LookupAccountNameW, PSID,
                SID_NAME_USE,
            },
        },
    };
    if account.is_empty() || account.contains('\0') {
        return None;
    }
    let wide: Vec<u16> = account.encode_utf16().chain(Some(0)).collect();
    let name = PCWSTR(wide.as_ptr());
    let mut sid = PSID::default();
    // SID strings need no account lookup and remain usable if the account was deleted.
    if unsafe { ConvertStringSidToSidW(name, &mut sid) }.is_ok() {
        let bytes = unsafe {
            std::slice::from_raw_parts(sid.0.cast::<u8>(), GetLengthSid(sid) as usize).to_vec()
        };
        unsafe { LocalFree(Some(HLOCAL(sid.0))) };
        return Some(bytes);
    }
    let (mut sid_size, mut domain_size) = (0, 0);
    let mut usage = SID_NAME_USE::default();
    let error = unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            name,
            None,
            &mut sid_size,
            None,
            &mut domain_size,
            &mut usage,
        )
    }
    .err()?;
    if error.code() != windows::core::HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0)
        || sid_size == 0
    {
        return None;
    }
    // The output SID must be DWORD-aligned; keep its backing storage alive until copied.
    let mut storage = vec![0usize; (sid_size as usize).div_ceil(std::mem::size_of::<usize>())];
    let sid = PSID(storage.as_mut_ptr().cast());
    let mut domain = vec![0u16; domain_size as usize];
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            name,
            Some(sid),
            &mut sid_size,
            (!domain.is_empty()).then(|| PWSTR(domain.as_mut_ptr())),
            &mut domain_size,
            &mut usage,
        )
    }
    .ok()?;
    Some(unsafe {
        std::slice::from_raw_parts(sid.0.cast::<u8>(), GetLengthSid(sid) as usize).to_vec()
    })
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub matches: bool,
    pub reason: Option<String>,
    #[serde(skip)]
    fingerprint: String,
}

fn drift_fingerprint(value: impl Serialize) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("drift serializes"))
    )
}

const XML_LIMIT: u64 = 1024 * 1024;
const XML_NS: &str = "http://schemas.microsoft.com/windows/2004/02/mit/task";

fn decode_xml(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() as u64 > XML_LIMIT {
        return Err("task XML exceeds the read limit".into());
    }
    if bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[b'<', 0])
        || bytes.starts_with(&[0xfe, 0xff])
    {
        let big_endian = bytes.starts_with(&[0xfe, 0xff]);
        let bytes = if bytes.starts_with(&[b'<', 0]) {
            bytes
        } else {
            &bytes[2..]
        };
        if bytes.len() % 2 != 0 {
            return Err("task XML has invalid UTF-16".into());
        }
        let words: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| {
                if big_endian {
                    u16::from_be_bytes([b[0], b[1]])
                } else {
                    u16::from_le_bytes([b[0], b[1]])
                }
            })
            .collect();
        String::from_utf16(&words).map_err(|_| "task XML has invalid UTF-16".into())
    } else {
        std::str::from_utf8(bytes)
            .map(|s| s.trim_start_matches('\u{feff}').to_string())
            .map_err(|_| "task XML has invalid UTF-8".into())
    }
}

#[cfg(windows)]
fn decode_registered_xml(xml: &windows::core::BSTR) -> Result<String, String> {
    if xml.len() as u64 > XML_LIMIT / 2 {
        return Err("task XML exceeds the read limit".into());
    }
    String::from_utf16(xml).map_err(|_| "task XML has invalid UTF-16".into())
}

#[cfg(windows)]
fn registered_task_xml(name: &str) -> Result<Option<String>, String> {
    use windows::{
        core::BSTR,
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            System::{
                Com::{
                    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
                    COINIT_MULTITHREADED,
                },
                TaskScheduler::{ITaskService, TaskScheduler},
                Variant::VARIANT,
            },
        },
    };
    let failure = |operation: &str, code: windows::core::HRESULT| {
        format!(
            "scheduler read {operation} failed (HRESULT 0x{:08x})",
            code.0 as u32
        )
    };
    // This synchronous call stays on one thread. An existing STA is usable too;
    // balance only successful initializations, including S_FALSE.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
        return Err(failure("initialize COM", initialized));
    }
    struct Apartment(bool);
    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
    // Declared first so all COM interfaces/BSTRs are released before COM teardown.
    let _apartment = Apartment(initialized.is_ok());
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| failure("create service", error.code()))?;
    let empty = VARIANT::default();
    unsafe { service.Connect(&empty, &empty, &empty, &empty) }
        .map_err(|error| failure("connect", error.code()))?;
    let folder = unsafe { service.GetFolder(&BSTR::from("\\")) }
        .map_err(|error| failure("open folder", error.code()))?;
    let task = match unsafe { folder.GetTask(&BSTR::from(name)) } {
        Ok(task) => task,
        Err(error) if matches!(error.code().0 as u32, 0x80070002 | 0x80070003) => return Ok(None),
        Err(error) => return Err(failure("get task", error.code())),
    };
    // get_Xml returns a UTF-16 BSTR. schtasks' piped XML can instead be encoded
    // in an OEM/console code page, and can even lose unrepresentable characters.
    let xml = unsafe { task.Xml() }.map_err(|error| failure("get XML", error.code()))?;
    decode_registered_xml(&xml).map(Some)
}

// Schema defaults are omitted from the comparison, whether exported explicitly or implicitly.
// Everything else (including unknown settings, extra actions/triggers and their order) stays checked.
fn default_value(parent: &str, name: &str) -> Option<&'static str> {
    match (parent, name) {
        (
            "Settings",
            "AllowStartOnDemand"
            | "AllowHardTerminate"
            | "DisallowStartIfOnBatteries"
            | "StopIfGoingOnBatteries"
            | "Enabled",
        ) => Some("true"),
        (
            "Settings",
            "StartWhenAvailable"
            | "RunOnlyIfNetworkAvailable"
            | "WakeToRun"
            | "Hidden"
            | "RunOnlyIfIdle"
            | "UseUnifiedSchedulingEngine"
            | "DisallowStartOnRemoteAppSession"
            | "Volatile",
        ) => Some("false"),
        ("Settings", "MultipleInstancesPolicy") => Some("IgnoreNew"),
        ("Settings", "Priority") => Some("7"),
        ("Settings", "ExecutionTimeLimit") | ("LogonTrigger", "ExecutionTimeLimit") => {
            Some("PT72H")
        }
        ("Settings", "DeleteExpiredTaskAfter") | ("LogonTrigger", "Delay") => Some("PT0S"),
        ("LogonTrigger", "Enabled") => Some("true"),
        ("Principal", "RunLevel") => Some("LeastPrivilege"),
        ("Principal", "ProcessTokenSidType") => Some("Unrestricted"),
        ("IdleSettings", "Duration") => Some("PT10M"),
        ("IdleSettings", "WaitTimeout") => Some("PT1H"),
        ("IdleSettings", "StopOnIdleEnd") => Some("true"),
        ("IdleSettings", "RestartOnIdle") => Some("false"),
        _ => None,
    }
}

fn normalized(name: &str, text: &str) -> String {
    let text = text.trim();
    if matches!(
        name,
        "Enabled"
            | "Hidden"
            | "AllowStartOnDemand"
            | "AllowHardTerminate"
            | "DisallowStartIfOnBatteries"
            | "StopIfGoingOnBatteries"
            | "StartWhenAvailable"
            | "RunOnlyIfNetworkAvailable"
            | "WakeToRun"
            | "RunOnlyIfIdle"
            | "UseUnifiedSchedulingEngine"
            | "DisallowStartOnRemoteAppSession"
            | "Volatile"
            | "StopOnIdleEnd"
            | "RestartOnIdle"
    ) {
        return match text {
            "1" => "true",
            "0" => "false",
            other => other,
        }
        .into();
    }
    if matches!(
        name,
        "ExecutionTimeLimit"
            | "Delay"
            | "Interval"
            | "Duration"
            | "WaitTimeout"
            | "DeleteExpiredTaskAfter"
    ) {
        static DURATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let pattern = DURATION.get_or_init(|| {
            regex::Regex::new(
                r"^P(?:0+Y)?(?:0+M)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)(?:\.0+)?S)?)?$",
            )
            .unwrap()
        });
        if let Some(caps) = pattern
            .captures(text)
            .filter(|_| text.bytes().any(|b| b.is_ascii_digit()))
        {
            let seconds =
                [86400u64, 3600, 60, 1]
                    .iter()
                    .enumerate()
                    .try_fold(0u64, |sum, (i, factor)| {
                        caps.get(i + 1)
                            .map_or(Some(0), |m| m.as_str().parse::<u64>().ok())
                            .and_then(|n| n.checked_mul(*factor))
                            .and_then(|n| sum.checked_add(n))
                    });
            if let Some(seconds) = seconds {
                return format!("seconds:{seconds}");
            }
        }
    }
    if matches!(name, "Priority" | "Count") {
        if let Ok(number) = text.parse::<u64>() {
            return number.to_string();
        }
    }
    #[cfg(windows)]
    if matches!(name, "Command" | "WorkingDirectory") {
        return text.replace('/', "\\").to_lowercase();
    }
    text.to_string()
}

fn semantics(xml: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    use std::collections::BTreeMap;
    let doc = roxmltree::Document::parse(xml).map_err(|_| "task XML is invalid")?;
    let root = doc.root_element();
    if !root.has_tag_name((XML_NS, "Task")) {
        return Err("task XML has no Scheduler Task root".into());
    }
    // Principal labels are export metadata; the action must still reference its actual principal.
    let principal = root
        .children()
        .find(|n| n.has_tag_name((XML_NS, "Principals")))
        .and_then(|n| n.children().find(|n| n.has_tag_name((XML_NS, "Principal"))));
    if let Some(actions) = root
        .children()
        .find(|n| n.has_tag_name((XML_NS, "Actions")))
    {
        if let Some(context) = actions.attribute("Context") {
            if principal.and_then(|p| p.attribute("id")) != Some(context) {
                return Err("task action principal reference is invalid".into());
            }
        }
    }
    fn walk(
        node: roxmltree::Node<'_, '_>,
        parent: &str,
        path: &str,
        out: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        let name = node.tag_name().name();
        if parent == "Task" && matches!(name, "RegistrationInfo" | "Data")
            || parent == "Principal" && name == "DisplayName"
        {
            return Ok(());
        }
        if node.tag_name().namespace() != Some(XML_NS) {
            return Err("task XML has an unexpected namespace".into());
        }
        for attribute in node.attributes() {
            if attribute.namespace().is_none()
                && (attribute.name() == "id"
                    || name == "Task" && attribute.name() == "version"
                    || name == "Actions" && attribute.name() == "Context")
            {
                continue;
            }
            out.insert(
                format!(
                    "{path}/@{}:{}",
                    attribute.namespace().unwrap_or(""),
                    attribute.name()
                ),
                attribute.value().into(),
            );
        }
        let children: Vec<_> = node.children().filter(|n| n.is_element()).collect();
        if children.is_empty() {
            if matches!(name, "IdleSettings" | "NetworkSettings") {
                return Ok(());
            }
            let text: String = node
                .children()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect();
            let value = normalized(name, &text);
            if default_value(parent, name).is_some_and(|d| normalized(name, d) == value) {
                return Ok(());
            }
            out.insert(path.into(), value);
        } else {
            if !matches!(name, "IdleSettings" | "NetworkSettings") {
                out.insert(path.into(), String::new());
            }
            let mut seen = BTreeMap::<&str, usize>::new();
            for (order, child) in children.into_iter().enumerate() {
                let child_name = child.tag_name().name();
                let index = seen.entry(child_name).or_default();
                // Action/trigger order is operational; settings and their export order are not.
                let ordinal = if matches!(name, "Actions" | "Triggers") {
                    order
                } else {
                    *index
                };
                let child_path = format!("{path}/{child_name}[{ordinal}]");
                if *index > 0 {
                    out.insert(format!("{child_path}/duplicate"), "true".into());
                }
                *index += 1;
                walk(child, name, &child_path, out)?;
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    walk(root, "", "Task", &mut result)?;
    Ok(result)
}

pub struct FileScheduler {
    path: PathBuf,
}
impl FileScheduler {
    fn check(&self) -> Result<(), String> {
        crate::install_root::check_development_path(&self.path)
    }
}
impl TaskScheduler for FileScheduler {
    fn install(&self, definition: &TaskDefinition) -> Result<(), String> {
        self.check()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("file scheduler directory: {}", e.kind()))?;
        }
        self.check()?;
        std::fs::write(&self.path, definition.xml())
            .map_err(|e| format!("file scheduler install: {}", e.kind()))
    }
    fn read(&self) -> Result<Option<String>, String> {
        use std::io::Read;
        self.check()?;
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("file scheduler read: {}", e.kind())),
        };
        let mut bytes = Vec::new();
        file.take(XML_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("file scheduler read: {}", e.kind()))?;
        decode_xml(&bytes).map(Some)
    }
    fn remove(&self) -> Result<(), String> {
        self.check()?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("file scheduler remove: {}", e.kind())),
        }
    }
}

pub struct NativeScheduler {
    name: String,
}
impl NativeScheduler {
    fn her() -> Self {
        Self {
            name: TASK_NAME.into(),
        }
    }
}

#[cfg(windows)]
fn system_tool(name: &str) -> Result<PathBuf, String> {
    let windows =
        std::env::var_os("SystemRoot").ok_or("Windows system directory is unavailable")?;
    Ok(PathBuf::from(windows).join("System32").join(name))
}

#[cfg(windows)]
struct XmlFile(PathBuf);
#[cfg(windows)]
impl XmlFile {
    fn create(xml: &str) -> Result<Self, String> {
        use std::io::Write;
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|_| "task XML random name unavailable")?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("her-task-{name}.xml"));
        crate::install_root::check_development_path(&path)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("task XML create: {}", e.kind()))?;
        let owned = Self(path);
        let text = xml.replace("encoding=\"UTF-8\"", "encoding=\"UTF-16\"");
        file.write_all(&[0xff, 0xfe])
            .and_then(|_| {
                for word in text.encode_utf16() {
                    file.write_all(&word.to_le_bytes())?;
                }
                Ok(())
            })
            .map_err(|e| format!("task XML write: {}", e.kind()))?;
        Ok(owned)
    }
}
#[cfg(windows)]
impl Drop for XmlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl TaskScheduler for NativeScheduler {
    fn install(&self, definition: &TaskDefinition) -> Result<(), String> {
        #[cfg(windows)]
        {
            let xml = XmlFile::create(&definition.xml())?;
            let output = crate::subprocess::command_without_console(system_tool("schtasks.exe")?)
                .args(["/Create", "/TN", &self.name, "/XML"])
                .arg(crate::subprocess::external_path(&xml.0)?)
                .args(["/F", "/HRESULT"])
                .output()
                .map_err(|e| format!("scheduler install: {}", e.kind()))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "scheduler install failed (HRESULT 0x{:08x})",
                    output.status.code().unwrap_or(-1) as u32
                ))
            }
        }
        #[cfg(not(windows))]
        {
            let _ = definition;
            Err(
                "the native Task Scheduler requires Windows; select a file scheduler for fixtures"
                    .into(),
            )
        }
    }
    fn read(&self) -> Result<Option<String>, String> {
        #[cfg(windows)]
        {
            registered_task_xml(&self.name)
        }
        #[cfg(not(windows))]
        {
            Err("the native Task Scheduler requires Windows".into())
        }
    }
    fn remove(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            let output = crate::subprocess::command_without_console(system_tool("schtasks.exe")?)
                .args(["/Delete", "/TN", &self.name, "/F", "/HRESULT"])
                .output()
                .map_err(|e| format!("scheduler remove: {}", e.kind()))?;
            let code = output.status.code().unwrap_or(-1) as u32;
            if output.status.success() || matches!(code, 0x80070002 | 0x80070003) {
                Ok(())
            } else {
                Err(format!("scheduler remove failed (HRESULT 0x{code:08x})"))
            }
        }
        #[cfg(not(windows))]
        {
            Err("the native Task Scheduler requires Windows".into())
        }
    }
}

fn selected() -> Result<Box<dyn TaskScheduler>, String> {
    if let Some(path) = std::env::var_os(FAKE_ENV) {
        if path.is_empty() {
            return Err("file scheduler path is empty".into());
        }
        let scheduler = FileScheduler { path: path.into() };
        scheduler.check()?;
        Ok(Box::new(scheduler))
    } else {
        Ok(Box::new(NativeScheduler::her()))
    }
}

#[cfg(windows)]
struct Token(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Token {
    fn open() -> Result<Self, String> {
        use windows_sys::Win32::{
            Security::TOKEN_QUERY,
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err("installing user's Windows token is unavailable".into());
        }
        Ok(Self(token))
    }
}
#[cfg(windows)]
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

pub fn current_user() -> Result<String, String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_USER,
            },
        };
        let token = Token::open()?;
        let mut size = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size);
        }
        if size == 0 {
            return Err("installing user's SID size is unavailable".into());
        }
        let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                data.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            return Err("installing user's SID is unavailable".into());
        }
        let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
        let mut text = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
            return Err("installing user's SID cannot be represented".into());
        }
        let mut len = 0;
        unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
        }
        let result = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) })
            .map_err(|_| "installing user's SID is invalid".to_string());
        unsafe {
            LocalFree(text.cast());
        }
        result
    }
    #[cfg(unix)]
    {
        Ok(format!("uid:{}", unsafe { libc::geteuid() }))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("installing user identity is unavailable".into())
    }
}

pub fn install_cli(args: &[String]) -> i32 {
    let install = || -> Result<TaskDefinition, String> {
        if !args.is_empty() {
            return Err("usage: her-cli install [--install-root <absolute root>]".into());
        }
        let definition = TaskDefinition::for_install(&InstallRoot::configured()?)?;
        selected()?.install(&definition)?;
        Ok(definition)
    };
    match install() {
        Ok(definition) => {
            let mut output = serde_json::to_value(definition).expect("task definition serializes");
            output["task"] = TASK_NAME.into();
            println!("{output}");
            0
        }
        Err(error) => {
            eprintln!("[scheduled-task] {error}");
            2
        }
    }
}

pub fn run_cli(args: &[String]) -> i32 {
    if args.len() != 1 || !matches!(args[0].as_str(), "read" | "compare" | "remove") {
        eprintln!(
            "usage: her-cli scheduled-task read|compare|remove [--install-root <absolute root>]"
        );
        return 2;
    }
    let result = (|| -> Result<(serde_json::Value, i32), String> {
        let scheduler = selected()?;
        match args[0].as_str() {
            "read" => Ok((
                serde_json::json!({"task": TASK_NAME, "xml": scheduler.read()?}),
                0,
            )),
            "remove" => {
                scheduler.remove()?;
                Ok((serde_json::json!({"task": TASK_NAME, "removed": true}), 0))
            }
            _ => {
                let definition = TaskDefinition::for_install(&InstallRoot::configured()?)?;
                let comparison = scheduler.compare(&definition)?;
                let exit = if comparison.matches { 0 } else { 3 };
                Ok((
                    serde_json::to_value(comparison).expect("comparison serializes"),
                    exit,
                ))
            }
        }
    })();
    match result {
        Ok((value, exit)) => {
            println!("{value}");
            exit
        }
        Err(error) => {
            println!("{}", serde_json::json!({"matches": false, "reason": error}));
            3
        }
    }
}

/// Advisory Scheduler check after the supervisor owns its selected runtime.
pub fn check_at_start(context: &crate::runtime::RuntimeContext, cfg: &serde_json::Value) {
    let comparison = (|| {
        let definition = TaskDefinition::for_install(&InstallRoot::configured()?)?;
        selected()?.compare(&definition)
    })();
    let drift = match comparison {
        Ok(Comparison { matches: true, .. }) => None,
        Ok(Comparison {
            reason,
            fingerprint,
            ..
        }) => Some((
            reason.unwrap_or_else(|| "definition differs".into()),
            fingerprint,
        )),
        Err(error) => Some((
            format!("cannot inspect HER task: {error}"),
            drift_fingerprint(("unreadable", error)),
        )),
    };
    let reason = drift
        .as_ref()
        .map(|(reason, _)| reason.as_str())
        .unwrap_or("HER task matches the Rust definition");
    crate::recovery_log::record(
        context,
        crate::run_record::Trigger::Startup,
        crate::run_record::Component::Process,
        "scheduled-task-check",
        if drift.is_some() {
            crate::run_record::Outcome::Failed
        } else {
            crate::run_record::Outcome::Ok
        },
        reason,
    );
    use crate::runtime::RuntimeArea;
    let result = (|| -> Result<(), String> {
        const STATE: &str = "scheduled-task-drift.json";
        let previous = context
            .read_optional_text(RuntimeArea::Data, STATE)?
            .map(|text| serde_json::from_str::<Option<String>>(&text))
            .transpose()
            .map_err(|_| "invalid scheduled-task drift state")?
            .flatten();
        if let Some((reason, fingerprint)) = drift {
            if previous.as_deref() == Some(&fingerprint) {
                return Ok(());
            }
            if crate::admin_alert::notify_notice(
                context,
                cfg,
                &format!("HER scheduled task drift: {reason}"),
                "scheduled-task-drift",
            ) {
                context.write_atomic(
                    RuntimeArea::Data,
                    STATE,
                    &serde_json::to_vec(&Some(fingerprint)).expect("fingerprint serializes"),
                )?;
            }
        } else if previous.is_some() {
            context.write_atomic(RuntimeArea::Data, STATE, b"null\n")?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        crate::recovery_log::record(
            context,
            crate::run_record::Trigger::Startup,
            crate::run_record::Component::Process,
            "scheduled-task-drift-state",
            crate::run_record::Outcome::Failed,
            &error,
        );
    }
}

#[cfg(all(test, windows))]
mod native_tests {
    use super::*;

    struct Readback(String);
    impl TaskScheduler for Readback {
        fn install(&self, _: &TaskDefinition) -> Result<(), String> {
            panic!("comparison must not install a task")
        }
        fn read(&self) -> Result<Option<String>, String> {
            Ok(Some(self.0.clone()))
        }
        fn remove(&self) -> Result<(), String> {
            panic!("comparison must not remove a task")
        }
    }

    fn system_definition() -> TaskDefinition {
        TaskDefinition {
            user_id: "S-1-5-18".into(),
            command: "C:/fixture/current/her.exe".into(),
            arguments: "supervise --runtime-root C:/fixture/runtime".into(),
            working_directory: "C:/fixture/current".into(),
        }
    }

    #[test]
    fn comparison_matches_account_names_and_sids_in_both_user_id_elements() {
        let mut definition = system_definition();
        let sid_xml = definition.xml();
        let name_xml = sid_xml.replace("S-1-5-18", r"NT AUTHORITY\SYSTEM");
        let comparison = Readback(name_xml).compare(&definition).unwrap();
        assert!(comparison.matches, "{comparison:?}");
        definition.user_id = r"NT AUTHORITY\SYSTEM".into();
        let comparison = Readback(sid_xml).compare(&definition).unwrap();
        assert!(comparison.matches, "{comparison:?}");
    }

    #[test]
    fn comparison_rejects_different_sids_in_either_user_id_element() {
        let definition = system_definition();
        let original = definition.xml();
        for xml in [
            original.replacen("S-1-5-18", "S-1-5-19", 1),
            original.replace(
                "<UserId>S-1-5-18</UserId><LogonType>",
                "<UserId>S-1-5-19</UserId><LogonType>",
            ),
        ] {
            let comparison = Readback(xml).compare(&definition).unwrap();
            assert!(!comparison.matches, "{comparison:?}");
        }
    }

    #[test]
    fn comparison_rejects_unresolvable_user_ids_even_with_identical_text() {
        let mut definition = system_definition();
        let missing = r"NT AUTHORITY\HER-Test-Unresolvable-b18c7501";
        let original = definition.xml();
        for xml in [
            original.replacen("S-1-5-18", missing, 1),
            original.replace(
                "<UserId>S-1-5-18</UserId><LogonType>",
                &format!("<UserId>{missing}</UserId><LogonType>"),
            ),
        ] {
            let comparison = Readback(xml).compare(&definition).unwrap();
            assert!(!comparison.matches, "{comparison:?}");
        }
        definition.user_id = missing.into();
        let comparison = Readback(definition.xml()).compare(&definition).unwrap();
        assert!(!comparison.matches, "{comparison:?}");
    }

    #[test]
    fn comparison_accepts_real_scheduler_readback_defaults_and_order() {
        let definition = system_definition();
        // Owner's elevated 2026-11-04 export, with synthetic identity/paths.
        // It contains no dates or changed boolean spellings. Keep the added idle
        // defaults, omitted Enabled=true, reordered children and registration URI.
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><URI>\HER-Test-47e8f54c-5e78-4575-92d7-2e56a62a3d12</URI></RegistrationInfo>
  <Principals><Principal id="Installer">
    <UserId>S-1-5-18</UserId>
    <LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel>
  </Principal></Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Hidden>true</Hidden>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <RestartOnFailure><Count>3</Count><Interval>PT1M</Interval></RestartOnFailure>
    <IdleSettings><StopOnIdleEnd>true</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
  </Settings>
  <Triggers><LogonTrigger><UserId>NT AUTHORITY\SYSTEM</UserId></LogonTrigger></Triggers>
  <Actions Context="Installer"><Exec>
    <Command>C:\fixture\current/her.exe</Command>
    <Arguments>supervise --runtime-root C:/fixture/runtime</Arguments>
    <WorkingDirectory>C:\fixture\current</WorkingDirectory>
  </Exec></Actions>
</Task>"#;
        let comparison = Readback(xml.into()).compare(&definition).unwrap();
        assert!(comparison.matches, "{comparison:?}");
        // Account equivalence must not relax action arguments or nondefault settings.
        for drifted in [
            xml.replace("supervise", "SUPERVISE"),
            xml.replace("<StopOnIdleEnd>true", "<StopOnIdleEnd>false"),
            xml.replace("<Count>3", "<Count>4"),
        ] {
            let comparison = Readback(drifted).compare(&definition).unwrap();
            assert!(!comparison.matches, "{comparison:?}");
        }
    }

    #[test]
    fn registered_xml_preserves_text_that_schtasks_encodes_as_gbk() {
        // Known CP936 bytes for 中文, as in the owner's failing task action.
        // The file fake must stay strict; native readback uses a UTF-16 BSTR.
        let gbk = b"<Task><Command>C:\\HER\\\xd6\xd0\xce\xc4</Command></Task>";
        assert!(decode_xml(gbk).is_err());
        use windows_sys::Win32::Globalization::MultiByteToWideChar;
        let mut wide = vec![0u16; gbk.len()];
        let len = unsafe {
            MultiByteToWideChar(
                936,
                0,
                gbk.as_ptr(),
                gbk.len() as i32,
                wide.as_mut_ptr(),
                wide.len() as i32,
            )
        };
        assert!(len > 0);
        wide.truncate(len as usize);
        let xml = windows::core::BSTR::from_wide(&wide);
        assert_eq!(
            decode_registered_xml(&xml).unwrap(),
            "<Task><Command>C:\\HER\\中文</Command></Task>"
        );
    }

    #[test]
    fn registered_xml_preserves_characters_outside_the_oem_code_page() {
        let xml = windows::core::BSTR::from("<Task><Command>中文 🚀 &amp; café</Command></Task>");
        assert_eq!(
            decode_registered_xml(&xml).unwrap(),
            "<Task><Command>中文 🚀 &amp; café</Command></Task>"
        );
        let invalid = windows::core::BSTR::from_wide(&[0xd800]);
        assert_eq!(
            decode_registered_xml(&invalid).unwrap_err(),
            "task XML has invalid UTF-16"
        );
    }

    #[test]
    fn native_scheduler_round_trip() {
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION};
        let token = Token::open().expect("read caller token before any scheduler access");
        let mut elevation: TOKEN_ELEVATION = unsafe { std::mem::zeroed() };
        let mut size = 0;
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenElevation,
                    (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                    std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                    &mut size,
                )
            },
            0,
            "cannot determine caller elevation"
        );
        if elevation.TokenIsElevated == 0 {
            eprintln!("SKIP native scheduler round trip: caller token is not elevated; no task was accessed or registered");
            return;
        }
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).unwrap();
        random[6] = (random[6] & 0x0f) | 0x40;
        random[8] = (random[8] & 0x3f) | 0x80;
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let guid = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        let name = format!("HER-Test-{guid}");
        assert!(
            regex::Regex::new(r"^HER-Test-[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$")
                .unwrap()
                .is_match(&name)
        );
        let scheduler = NativeScheduler { name: name.clone() };
        assert!(scheduler
            .read()
            .expect("unique task absence must be known")
            .is_none());
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                std::fs::remove_dir(&self.0).expect("remove owned empty native fixture");
            }
        }
        let path = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-task-12-native-{guid}")),
        );
        std::fs::create_dir(&path).unwrap();
        let root = Root(path);
        let definition = TaskDefinition::for_install(
            &InstallRoot::new(&root.0.join("install 中文 🚀 & space")).unwrap(),
        )
        .unwrap();
        struct Cleanup {
            scheduler: NativeScheduler,
            active: bool,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.active {
                    if let Err(error) = self.scheduler.remove() {
                        eprintln!("native task cleanup failed: {error}");
                        if !std::thread::panicking() {
                            panic!("native task cleanup failed");
                        }
                    }
                }
            }
        }
        // Arm cleanup before registration: a failed import may have partially registered the task.
        let mut cleanup = Cleanup {
            scheduler,
            active: true,
        };
        cleanup
            .scheduler
            .install(&definition)
            .expect("install unique native task");
        let readback = cleanup
            .scheduler
            .read()
            .expect("read native task")
            .expect("native task exists");
        assert!(readback.contains("HighestAvailable"));
        assert!(readback.contains("中文 🚀"), "native XML lost Unicode text");
        let comparison = cleanup
            .scheduler
            .compare(&definition)
            .expect("compare native XML");
        assert!(comparison.matches, "{comparison:?}\n{readback}");
        cleanup
            .scheduler
            .remove()
            .expect("remove unique native task");
        assert!(cleanup.scheduler.read().expect("verify removal").is_none());
        cleanup.active = false;
        println!("native scheduler round trip installed/read/compared/removed {name}; task never triggered");
    }
}
