//! The desktop is a console client, independent of the supervisor's lifetime.
use crate::install_root::InstallRoot;
use crate::runtime::RuntimeContext;
use std::path::{Path, PathBuf};


/// Portable tray presentation and approval payload. A click retains the displayed tag.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct TrayUpdate {
    label: String,
    approval: Option<serde_json::Value>,
}
#[cfg_attr(not(windows), allow(dead_code))]
impl TrayUpdate {
    fn unavailable() -> Self {
        Self {
            label: "Updates: status unavailable".into(),
            approval: None,
        }
    }
    fn from_status(status: &serde_json::Value) -> Self {
        if status["ok"] != true {
            return Self::unavailable();
        }
        if status["settings"]["policy"] == "off" {
            return Self {
                label: "Updates disabled".into(),
                approval: None,
            };
        }
        let Some(current) = status["current"].as_str() else {
            return Self::unavailable();
        };
        if status["available"].is_null() && status["error"].is_null() {
            return Self {
                label: "HER is up to date".into(),
                approval: None,
            };
        }
        let Some(tag) = status["available"]["tag"].as_str().filter(|tag| {
            crate::release_store::version(tag)
                .is_some_and(|next| crate::release_store::version(current).is_none_or(|old| next > old))
        }) else {
            return Self::unavailable();
        };
        Self {
            label: format!("Update available: {current} → {tag}"),
            approval: Some(serde_json::json!({"confirm":true,"tag":tag})),
        }
    }
}

/// Desktop sign-in proves access to the selected runtime's token file.
pub(crate) fn console_token(context: &RuntimeContext) -> Result<String, String> {
    #[cfg(windows)]
    {
        let token = windows::read_private_token(context)?;
        let token = token.trim();
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err("protected console token file is invalid".into());
        }
        Ok(token.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = context;
        Err("desktop token proof requires Windows".into())
    }
}

/// Register startup and return whether the desktop UI should open.
#[cfg(any(windows, test))]
fn prepare_logon(
    installed: bool,
    register_logon: bool,
    run_store: Option<&str>,
    register: impl FnOnce(Option<&str>) -> Result<(), String>,
) -> Result<bool, String> {
    if installed || register_logon {
        register(run_store)?;
    }
    Ok(!register_logon)
}

pub fn run_cli(args: &[String]) -> i32 {
    let result = || -> Result<(), String> {
        let mut root = std::env::var_os("HER_RUNTIME_ROOT").map(PathBuf::from);
        let mut hidden = false;
        let mut register_logon = false;
        let mut run_store = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--help" => {
                    println!("usage: her [app] [--runtime-root <absolute root>] [--hidden] [--register-logon] [--run-store <data-relative fixture>]\nAn installed image opens its runtime; development requires --runtime-root.");
                    return Ok(());
                }
                "--hidden" => hidden = true,
                "--register-logon" => register_logon = true,
                "--run-store" => {
                    if run_store.is_some() {
                        return Err("desktop Run store was selected more than once".into());
                    }
                    index += 1;
                    run_store = Some(
                        args.get(index)
                            .ok_or("--run-store requires a value")?
                            .clone(),
                    );
                }
                "--runtime-root" => {
                    if root.is_some() {
                        return Err("desktop runtime was selected more than once".into());
                    }
                    index += 1;
                    root = Some(PathBuf::from(
                        args.get(index).ok_or("--runtime-root requires a value")?,
                    ));
                }
                _ => return Err("unknown desktop option; use app --help".into()),
            }
            index += 1;
        }
        let install = InstallRoot::configured()?;
        let image = std::env::current_exe().map_err(|_| "desktop executable is unavailable")?;
        let root = match root {
            Some(root) => root,
            None => {
                install.require_release_image(&image).map_err(|_| {
                    "desktop app requires --runtime-root outside an installed release"
                })?;
                install.root().join("runtime")
            }
        };
        install.check_runtime(&root, &image, false)?;
        let context = RuntimeContext::from_root(&root).map_err(str::to_string)?;
        let installed = same_directory(&root, &install.root().join("runtime"))
            && install.require_release_image(&image).is_ok();
        let cli = if installed {
            install.root().join("current/her-cli.exe")
        } else {
            image
                .parent()
                .ok_or("desktop executable directory is unavailable")?
                .join(if cfg!(windows) {
                    "her-cli.exe"
                } else {
                    "her-cli"
                })
        };
        #[cfg(windows)]
        {
            if !prepare_logon(installed, register_logon, run_store.as_deref(), |store| {
                windows::register_logon(&context, &install, installed, store)
            })? {
                return Ok(());
            }
            return windows::run(root, context, cli, hidden).map_err(|error| {
                windows::show_error(&error);
                error
            });
        }
        #[cfg(not(windows))]
        {
            let _ = (context, cli, hidden, register_logon, run_store);
            Err("HER desktop app is available on Windows".into())
        }
    };
    match result() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[her] {error}");
            2
        }
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn installed_app_with_fake_run_store_registers_and_continues_to_ui() {
        let mut stores = Vec::new();
        let open_ui = prepare_logon(true, false, Some("desktop-run.json"), |store| {
            stores.push(store.map(str::to_string));
            Ok(())
        })
        .expect("a fake Run store must be accepted without registration-only mode");
        assert_eq!(stores, [Some("desktop-run.json".to_string())]);
        assert!(open_ui, "registration must continue to the desktop UI");
    }

    #[test]
    fn default_launches_and_registration_only_keep_their_startup_behavior() {
        for (installed, registration_only, store, should_register, should_open) in [
            (true, false, None, true, true),
            (false, false, None, false, true),
            (true, true, None, true, false),
            (true, true, Some("desktop-run.json"), true, false),
            (false, true, Some("desktop-run.json"), true, false),
        ] {
            let mut registered = false;
            let open_ui = prepare_logon(installed, registration_only, store, |selected| {
                assert_eq!(selected, store);
                registered = true; // Policy test only: never call the real Run store.
                Ok(())
            }).unwrap();
            assert_eq!(registered, should_register);
            assert_eq!(open_ui, should_open);
        }
    }

    #[test]
    fn failed_fake_registration_prevents_ui_startup() {
        let result = prepare_logon(true, false, Some("desktop-run.json"), |_| {
            Err("desktop Run fixture is invalid".into())
        });
        assert_eq!(result, Err("desktop Run fixture is invalid".into()));
    }
}

fn same_directory(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left)
        .ok()
        .zip(std::fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::lifecycle::{OwnerGate, OwnerProbe};
    use crate::process_identity::{self, ProcessIdentity};
    use crate::runtime::{PathIntent, RuntimeArea};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
    use std::process::Stdio;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};
    use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::window::WindowBuilder;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};
    use wry::{WebContext, WebViewBuilder};

    pub(super) fn read_private_token(context: &RuntimeContext) -> Result<String, String> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            CreateWellKnownSid, EqualSid, GetAce, IsValidAcl, IsValidSid,
            WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
            DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        };
        let error = || "protected console token file is unavailable or has unsafe ownership/ACL".to_string();
        let path = context.resolve_path(RuntimeArea::Data, "console-token", PathIntent::ReadFile)
            .map_err(|_| error())?;
        // Pin the same file for inspection and reading. Deny writers and replacement until done;
        // open a reparse node itself rather than following a late link substitution.
        let mut file = std::fs::OpenOptions::new().read(true).share_mode(1)
            .custom_flags(0x00200000).open(path).map_err(|_| error())?; // FILE_FLAG_OPEN_REPARSE_POINT
        let metadata = file.metadata().map_err(|_| error())?;
        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 { return Err(error()); }
        let user = process_identity::current_user()?;
        let mut system = [0u32; 17];
        let mut admins = [0u32; 17];
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: owned handle and initialized out pointers; buffers hold SECURITY_MAX_SID_SIZE.
        unsafe {
            for (kind, buffer) in [(WinLocalSystemSid, &mut system), (WinBuiltinAdministratorsSid, &mut admins)] {
                let mut size = std::mem::size_of_val(buffer) as u32;
                if CreateWellKnownSid(kind, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size) == 0 {
                    return Err(error());
                }
            }
            if GetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner, std::ptr::null_mut(), &mut acl, std::ptr::null_mut(), &mut descriptor) != 0 {
                return Err(error());
            }
        }
        struct Descriptor(*mut std::ffi::c_void);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
                unsafe { LocalFree(self.0); }
            }
        }
        let _descriptor = Descriptor(descriptor);
        // SAFETY: trustee pointers refer to the retained security descriptor or SID buffers.
        let allowed = |sid: *mut std::ffi::c_void| unsafe {
            !sid.is_null() && IsValidSid(sid) != 0 &&
                [user.as_ptr().cast_mut().cast(), system.as_ptr().cast_mut().cast(), admins.as_ptr().cast_mut().cast()]
                    .iter().any(|trusted| EqualSid(sid, *trusted) != 0)
        };
        if !allowed(owner) || acl.is_null() || unsafe { IsValidAcl(acl) } == 0 { return Err(error()); }
        // Fail closed on unknown ACE formats; deny entries cannot broaden access.
        for index in 0..unsafe { (*acl).AceCount } as u32 {
            let mut ace = std::ptr::null_mut();
            // SAFETY: validated ACL and in-range index; the descriptor remains allocated.
            unsafe {
                if GetAce(acl, index, &mut ace) == 0 || ace.is_null() { return Err(error()); }
                let header = &*ace.cast::<ACE_HEADER>();
                match header.AceType {
                    0 => { // ACCESS_ALLOWED_ACE_TYPE
                        if (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>() { return Err(error()); }
                        let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                        if entry.Mask != 0 && !allowed(std::ptr::addr_of!(entry.SidStart).cast_mut().cast()) { return Err(error()); }
                    }
                    1 => {} // ACCESS_DENIED_ACE_TYPE
                    _ => return Err(error()),
                }
            }
        }
        let mut token = String::new();
        file.read_to_string(&mut token).map_err(|_| error())?;
        Ok(token)
    }

    // Always quote each argument using the Windows backslash/quote rules. In particular,
    // double a trailing backslash before the closing quote (install roots may end in one).
    fn quote_argument(value: &str) -> String {
        let mut result = String::from("\"");
        let mut slashes = 0;
        for ch in value.chars() {
            if ch == '\\' {
                slashes += 1;
                continue;
            }
            result.extend(std::iter::repeat_n(
                '\\',
                if ch == '"' { slashes * 2 + 1 } else { slashes },
            ));
            result.push(ch);
            slashes = 0;
        }
        result.extend(std::iter::repeat_n('\\', slashes * 2));
        result.push('"');
        result
    }

    pub(super) fn register_logon(
        context: &RuntimeContext,
        install: &InstallRoot,
        installed: bool,
        fixture: Option<&str>,
    ) -> Result<(), String> {
        let gui = install.root().join("current/her.exe");
        if fixture.is_none() {
            if !installed {
                return Err("real desktop logon registration requires an installed release".into());
            }
            for image in [&gui, &install.root().join("current/her-cli.exe")] {
                if !image.is_file() {
                    return Err("desktop logon requires both installed HER images".into());
                }
                install.require_release_image(image)?;
            }
        }
        let command = [
            crate::subprocess::external_path(&gui)?,
            "app".into(),
            "--hidden".into(),
        ]
        .iter()
        .map(|arg| quote_argument(arg))
        .collect::<Vec<_>>()
        .join(" ");
        if command.encode_utf16().count() > 260 {
            return Err("desktop startup command exceeds the 260-character Run limit".into());
        }
        if let Some(relative) = fixture {
            let _gate = context.authority_gate()?;
            let path = context
                .resolve_path(RuntimeArea::Data, relative, PathIntent::WriteFile)
                .map_err(str::to_string)?;
            let mut values = match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                    .map_err(|_| "desktop Run fixture is invalid")?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(_) => return Err("desktop Run fixture is unavailable".into()),
            };
            values
                .as_object_mut()
                .ok_or("desktop Run fixture must be an object")?
                .insert("HERDesktop".into(), Value::String(command));
            return context.write_atomic(
                RuntimeArea::Data,
                relative,
                values.to_string().as_bytes(),
            );
        }
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
            REG_OPTION_NON_VOLATILE, REG_SZ,
        };
        struct Key(HKEY);
        impl Drop for Key {
            fn drop(&mut self) {
                // SAFETY: this wrapper owns the successfully opened registry handle.
                unsafe {
                    RegCloseKey(self.0);
                }
            }
        }
        let wide = |value: &str| value.encode_utf16().chain([0]).collect::<Vec<_>>();
        let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let name = wide("HERDesktop");
        let data = wide(&command);
        let bytes =
            u32::try_from(data.len() * 2).map_err(|_| "desktop startup command is too long")?;
        let mut handle = std::ptr::null_mut();
        // SAFETY: NUL-terminated strings and out handle remain live through each call.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut handle,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(format!(
                "desktop logon Run key could not be opened ({status})"
            ));
        }
        let key = Key(handle);
        // SAFETY: REG_SZ bytes cover the complete live UTF-16 buffer including its terminator.
        let status =
            unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes) };
        if status != 0 {
            return Err(format!(
                "desktop logon Run entry could not be written ({status})"
            ));
        }
        Ok(())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Health {
        Unknown,
        Unhealthy,
        Healthy,
    }
    enum Ui {
        Health(Health),
        Qq(Value),
        Login(Result<(), String>),
        Updates(super::TrayUpdate),
        UpdateResult(Result<(), String>),
        Session {
            url: String,
            token: String,
            navigate: bool,
        },
        Navigate(String),
        DocumentLoaded,
        Menu(String),
        Notice(String),
        Restart(Result<(), String>),
    }
    enum Action {
        Update(Value),
        Open,
        Login,
        Restart,
        Quit,
    }

    fn poll_action(rx: &mpsc::Receiver<Action>) -> Result<Action, mpsc::RecvTimeoutError> {
        rx.recv_timeout(Duration::from_secs(30))
    }

    #[derive(Default)]
    struct LoginNotification {
        notified: bool,
    }

    impl LoginNotification {
        fn observe(&mut self, state: &str, deliver: impl FnOnce() -> bool) -> bool {
            if state == "logged-in" {
                self.notified = false;
            } else if state == "needs-login" && !self.notified {
                self.notified = deliver();
                return self.notified;
            }
            false
        }
    }

    // tray-icon exposes its window but keeps its native icon ID private. Give
    // the balloon a GUID-owned hidden registration on that same tray window;
    // this adds no second visible icon and never guesses the library's ID.
    struct LoginBalloon(windows_sys::Win32::UI::Shell::NOTIFYICONDATAW);

    impl LoginBalloon {
        fn show(tray: &tray_icon::TrayIcon) -> Result<Self, String> {
            use windows_sys::Win32::UI::Shell::{
                Shell_NotifyIconW, NIF_GUID, NIF_INFO, NIF_STATE, NIIF_WARNING,
                NIM_ADD, NIS_HIDDEN, NOTIFYICONDATAW,
            };
            // SAFETY: CoCreateGuid writes an owned value and needs no COM apartment.
            let guid = unsafe { ::windows::Win32::System::Com::CoCreateGuid() }
                .map_err(|_| "Windows notification identity is unavailable")?;
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: tray.window_handle(),
                uFlags: NIF_GUID | NIF_INFO | NIF_STATE,
                dwState: NIS_HIDDEN,
                dwStateMask: NIS_HIDDEN,
                dwInfoFlags: NIIF_WARNING,
                guidItem: windows_sys::core::GUID::from_u128(guid.to_u128()),
                ..Default::default()
            };
            for (slot, unit) in data.szInfoTitle.iter_mut().zip("HER: QQ needs login".encode_utf16()) {
                *slot = unit;
            }
            for (slot, unit) in data.szInfo.iter_mut().zip(
                "Open HER from the tray and choose Open QQ login window to sign in.".encode_utf16(),
            ) {
                *slot = unit;
            }
            // SAFETY: data has the native size, a live retained tray HWND, unique
            // GUID and fixed, NUL-terminated UTF-16 buffers. Shell copies it.
            if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
                return Err("Windows did not accept the QQ login notification".into());
            }
            Ok(Self(data))
        }
    }

    impl Drop for LoginBalloon {
        fn drop(&mut self) {
            // SAFETY: delete only the GUID-owned registration created above,
            // before the desktop destroys its tray window.
            unsafe {
                windows_sys::Win32::UI::Shell::Shell_NotifyIconW(
                    windows_sys::Win32::UI::Shell::NIM_DELETE, &self.0,
                );
            }
        }
    }

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            owner: *mut std::ffi::c_void,
            text: *const u16,
            title: *const u16,
            kind: u32,
        ) -> i32;
    }
    pub(super) fn show_error(message: &str) {
        let text: Vec<u16> = message.encode_utf16().chain([0]).collect();
        let title: Vec<u16> = "HER".encode_utf16().chain([0]).collect();
        // SAFETY: both buffers are NUL-terminated and live until this synchronous call returns.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
        }
    }

    fn console_port(context: &RuntimeContext) -> Result<u16, String> {
        let cfg = context.load_config().map_err(str::to_string)?;
        match cfg.get("consolePort") {
            None | Some(Value::Null) => Ok(3100),
            Some(value) => value
                .as_f64()
                .filter(|port| port.fract() == 0.0 && (1.0..=65535.0).contains(port))
                .map(|port| port as u16)
                .ok_or_else(|| "consolePort is invalid".into()),
        }
    }

    fn console_owner(context: &RuntimeContext) -> Option<ProcessIdentity> {
        match OwnerGate::new(context.data_dir().join("logs/rbridge-console.lock")).probe_existing()
        {
            OwnerProbe::Held {
                marker: Some(marker),
            } if marker.role == "console" => Some(marker.identity),
            _ => None,
        }
    }

    // The public CLI delegates to health_check and authorizes the active release's images.
    // In particular an old GUI remains usable after the current link moves to a new release.
    fn health(root: &Path, cli: &Path) -> Result<(Health, bool), String> {
        let mut command = crate::subprocess::command_without_console(crate::subprocess::external_path(cli)?);
        command.args(["lifecycle", "assert-running", "--runtime-root"])
            .arg(root).args(["--wait", "0ms"]).env_remove("HER_RUNTIME_ROOT");
        health_command(&mut command)
    }

    fn health_command(command: &mut std::process::Command) -> Result<(Health, bool), String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    // Child retains the original process handle: no PID lookup or tree stop.
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let mut child = Child(command.stdin(Stdio::null()).stderr(Stdio::null())
            .stdout(Stdio::piped()).spawn().map_err(|_| "HER health is unavailable")?);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stdout = child.0.stdout.take().ok_or("HER health output is unavailable")?;
        let mut bytes = Vec::new();
        let mut status = None;
        let status = loop {
            if Instant::now() >= deadline { return Err("HER health child timed out".into()); }
            let mut available = 0;
            // SAFETY: stdout owns the live anonymous pipe handle; the out count is initialized.
            let peek = unsafe { PeekNamedPipe(stdout.as_raw_handle(), std::ptr::null_mut(), 0,
                std::ptr::null_mut(), &mut available, std::ptr::null_mut()) };
            if peek == 0 && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_BROKEN_PIPE as i32) {
                return Err("HER health output is unavailable".into());
            }
            if available > 0 {
                let mut buffer = [0u8; 4096];
                let count = (available as usize).min(buffer.len());
                stdout.read_exact(&mut buffer[..count]).map_err(|_| "HER health output is unavailable")?;
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > 65536 { return Err("HER health output is too large".into()); }
                continue;
            }
            if let Some(status) = status { break status; }
            status = child.0.try_wait().map_err(|_| "HER health child wait failed")?;
            // Drain once more after observing exit. Never wait for pipe EOF: an inherited
            // writer could outlive this child and must not extend the parent's deadline.
            if status.is_none() { std::thread::sleep(Duration::from_millis(25)); }
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "HER health verdict is unreadable")?;
        let ok = value["ok"]
            .as_bool()
            .ok_or("HER health verdict is invalid")?;
        if value["v"] != 1 || status.code() != Some(if ok { 0 } else { 1 }) {
            return Err("HER health verdict is invalid".into());
        }
        let console = match value
            .pointer("/parts/console/verdict")
            .and_then(Value::as_str)
        {
            Some("pass") => true,
            Some("fail") => false,
            _ => return Err("HER console health is unavailable".into()),
        };
        Ok((
            if ok {
                Health::Healthy
            } else {
                Health::Unhealthy
            },
            console,
        ))
    }

    fn request(port: u16, token: &str, path: &str, body: Value) -> Result<Value, String> {
        request_method(port, token, "POST", path, body)
    }

    fn request_method(port: u16, token: &str, method: &str, path: &str, body: Value) -> Result<Value, String> {
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err("console credential is unavailable".into());
        }
        let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))
            .map_err(|_| "HER console is unavailable")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|_| "console timeout setup failed")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|_| "console timeout setup failed")?;
        // Prove the connected server's user before handing it the protected credential.
        if process_identity::tcp_peer_user(&stream)? != process_identity::current_user()? {
            return Err("console Windows user cannot be verified".into());
        }
        let body = body.to_string();
        let message = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Console-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream
            .write_all(message.as_bytes())
            .map_err(|_| "console request failed")?;
        let mut bytes = Vec::new();
        stream
            .take(65536)
            .read_to_end(&mut bytes)
            .map_err(|_| "console response failed")?;
        let text = String::from_utf8(bytes).map_err(|_| "console response is unreadable")?;
        let (headers, body) = text
            .split_once("\r\n\r\n")
            .ok_or("console response is incomplete")?;
        let status = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or("console response is invalid")?;
        if !(200..300).contains(&status) {
            return Err(format!("HER console refused the request (HTTP {status})"));
        }
        let value: Value =
            serde_json::from_str(body).map_err(|_| "console response is not JSON")?;
        if value.get("ok") == Some(&Value::Bool(false)) {
            return Err("HER console refused the request".into());
        }
        Ok(value)
    }

    fn session_url(context: &RuntimeContext, raw: &str, token: &str) -> Option<String> {
        let mut url = url::Url::parse(raw).ok()?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port_or_known_default() != Some(console_port(context).ok()?)
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "token")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("token", token);
        Some(url.to_string())
    }

    fn worker(
        root: PathBuf,
        context: RuntimeContext,
        cli: PathBuf,
        rx: mpsc::Receiver<Action>,
        proxy: EventLoopProxy<Ui>,
    ) {
        let mut action = Some(Action::Open);
        let mut signed_at: Option<Instant> = None;
        let mut last_owner: Option<ProcessIdentity> = None;
        let mut last_epoch = String::new();
        loop {
            if matches!(action, Some(Action::Quit)) {
                return;
            }
            let open = matches!(action, Some(Action::Open));
            let restart = matches!(action, Some(Action::Restart));
            let login = matches!(action, Some(Action::Login));
            let update = match action.as_ref() { Some(Action::Update(body)) => Some(body.clone()), _ => None };
            let snapshot = health(&root, &cli);
            let (state, ready) = snapshot.unwrap_or((Health::Unknown, false));
            if proxy.send_event(Ui::Health(state)).is_err() {
                return;
            }
            if ready {
                let result = (|| -> Result<(), String> {
                    let owner = console_owner(&context).ok_or("console owner is unavailable")?;
                    let credential = super::console_token(&context)?;
                    let port = console_port(&context)?;
                    let qq = request_method(port, &credential, "GET", "/api/qq/login", Value::Null)
                        .unwrap_or_else(|_| json!({"state":"unknown","processes":[]}));
                    let _ = proxy.send_event(Ui::Qq(qq.clone()));
                    let releases = request_method(port,&credential,"GET","/api/releases",Value::Null)
                        .unwrap_or(Value::Null);
                    let _ = proxy.send_event(Ui::Updates(super::TrayUpdate::from_status(&releases)));
                    if let Some(body) = update.as_ref() {
                        let result = request(port,&credential,"/api/releases/approve",body.clone()).map(|_|());
                        let _ = proxy.send_event(Ui::UpdateResult(result));
                    }
                    if login {
                        let result = focus_login(&qq);
                        let _ = proxy.send_event(Ui::Login(result));
                    }
                    let epoch = format!("{:x}", Sha256::digest(credential.as_bytes()));
                    if restart {
                        let result = crate::console::generate_console_token().and_then(|id| {
                            request(
                                port,
                                &credential,
                                "/api/restart",
                                json!({"confirm":true,"requestId":format!("desktop-{id}")}),
                            )
                            .map(|_| ())
                        });
                        let _ = proxy.send_event(Ui::Restart(result));
                    }
                    let replaced = last_owner
                        .as_ref()
                        .is_none_or(|previous| !process_identity::same_identity(previous, &owner));
                    let age = signed_at.map(|at| at.elapsed());
                    if open
                        || restart
                        || replaced
                        || epoch != last_epoch
                        || age.is_none_or(|age| age >= Duration::from_secs(3600))
                    {
                        let reply = request(port, &credential, "/api/desktop/sign-in", json!({}))?;
                        let token = reply["session"]
                            .as_str()
                            .filter(|token| !token.is_empty())
                            .ok_or("desktop session is unavailable")?
                            .to_string();
                        let url =
                            session_url(&context, &format!("http://127.0.0.1:{port}/"), &token)
                                .ok_or("console URL is unavailable")?;
                        let navigate = open
                            || restart
                            || replaced
                            || age.is_none_or(|age| age >= Duration::from_secs(6 * 3600));
                        let _ = proxy.send_event(Ui::Session {
                            url,
                            token,
                            navigate,
                        });
                        signed_at = Some(Instant::now());
                        last_owner = Some(owner);
                        last_epoch = epoch;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = proxy.send_event(Ui::Updates(super::TrayUpdate::unavailable()));
                    if update.is_some() { let _ = proxy.send_event(Ui::UpdateResult(Err("Console credentials are unavailable; update was not approved".into()))); }
                    let _ = proxy.send_event(Ui::Qq(json!({"state":"unknown","processes":[]})));
                    if login { let _ = proxy.send_event(Ui::Login(Err("No verified QQ login window was found; console credentials are unavailable".into()))); }
                    let _ = proxy.send_event(Ui::Notice(error));
                }
            } else {
                let _ = proxy.send_event(Ui::Updates(super::TrayUpdate::unavailable()));
                if update.is_some() { let _ = proxy.send_event(Ui::UpdateResult(Err("HER console is unavailable; update was not approved".into()))); }
                let _ = proxy.send_event(Ui::Qq(json!({"state":"unknown","processes":[]})));
                if login { let _ = proxy.send_event(Ui::Login(Err("No verified QQ login window was found; HER console is unavailable".into()))); }
                if restart {
                let _ = proxy.send_event(Ui::Restart(Err(
                    "HER console is unavailable; check the supervisor".into(),
                )));
                }
            }
            action = match poll_action(&rx) {
                Ok(action) => Some(action),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
        }
    }

    fn focus_login(snapshot: &Value) -> Result<(), String> {
        let mut failure = "No verified QQ login window was found".to_string();
        if snapshot["state"] != "needs-login" { return Err(failure); }
        for target in snapshot["processes"].as_array().into_iter().flatten() {
            let Some(pid) = target["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()) else { continue; };
            let Some(identity) = process_identity::identity_of(pid).ok().flatten() else { continue; };
            if target["start"].as_u64() != Some(identity.start) { continue; }
            match process_identity::focus_qq_window(&identity) {
                Ok(()) => return Ok(()),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    fn pixels(health: Health) -> Vec<u8> {
        let color = match health {
            Health::Healthy => [30, 160, 85, 255],
            Health::Unhealthy => [215, 65, 65, 255],
            Health::Unknown => [210, 150, 35, 255],
        };
        let mut bytes = Vec::with_capacity(32 * 32 * 4);
        for y in 0i32..32 {
            for x in 0i32..32 {
                let inside = (x - 16).pow(2) + (y - 16).pow(2) <= 225;
                let letter = (8..24).contains(&y)
                    && ((8..12).contains(&x)
                        || (20..24).contains(&x)
                        || ((14..18).contains(&y) && (12..20).contains(&x)));
                bytes.extend_from_slice(&if !inside {
                    [0, 0, 0, 0]
                } else if letter {
                    [255, 255, 255, 255]
                } else {
                    color
                });
            }
        }
        bytes
    }

    pub(super) fn run(
        root: PathBuf,
        context: RuntimeContext,
        cli: PathBuf,
        hidden: bool,
    ) -> Result<(), String> {
        let event_loop = EventLoopBuilder::<Ui>::with_user_event().build();
        let window = WindowBuilder::new()
            .with_title("HER")
            .with_visible(!hidden)
            .with_inner_size(LogicalSize::new(1100.0, 760.0))
            .with_min_inner_size(LogicalSize::new(800.0, 500.0))
            .with_window_icon(Some(
                tao::window::Icon::from_rgba(pixels(Health::Unknown), 32, 32)
                    .map_err(|error| error.to_string())?,
            ))
            .build(&event_loop)
            .map_err(|error| error.to_string())?;
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Open HER", true, None);
        let restart = MenuItem::with_id("restart", "Restart HER", true, None);
        let qq_login = MenuItem::with_id("qq-login", "QQ login state unavailable", false, None);
        let updates = MenuItem::with_id("updates", "Updates: status unavailable", false, None);
        let quit = MenuItem::with_id("quit", "Quit desktop app", true, None);
        menu.append_items(&[&open, &qq_login, &restart, &updates, &quit])
            .map_err(|error| error.to_string())?;
        let mut tray = Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("HER: checking health")
                .with_icon(
                    Icon::from_rgba(pixels(Health::Unknown), 32, 32)
                        .map_err(|error| error.to_string())?,
                )
                .build()
                .map_err(|error| error.to_string())?,
        );
        let proxy = event_loop.create_proxy();
        let ipc_proxy = event_loop.create_proxy();
        let load_proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = proxy.send_event(Ui::Menu(event.id.0));
        }));
        let current = Arc::new(Mutex::new(None::<String>));
        let nav_session = current.clone();
        let nav_context = context.clone();
        let proxy = event_loop.create_proxy();
        let profile = context
            .resolve_path(
                RuntimeArea::Data,
                "desktop-webview2",
                PathIntent::CreateDirectory,
            )
            .map_err(str::to_string)?;
        let mut web_context = WebContext::new(Some(PathBuf::from(
            crate::subprocess::external_path(profile)?,
        )));
        let webview = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html("<!doctype html><title>HER</title><body style='font:18px system-ui;padding:40px'>Connecting to HER…</body>")
            .with_devtools(false)
            .with_initialization_script("if(location.protocol==='http:'&&location.hostname==='127.0.0.1'){const t=new URLSearchParams(location.search).get('token');if(t){localStorage.setItem('consoleToken',t);sessionStorage.setItem('consoleToken',t);}}")
            .with_initialization_script(r#"window.herQqLogin=function(state){
                let banner=document.getElementById('her-qq-login');
                if(state!=='needs-login'){if(banner)banner.remove();return;}
                if(banner)return;
                banner=document.createElement('div');banner.id='her-qq-login';
                banner.style.cssText='position:fixed;top:0;left:0;right:0;z-index:2147483647;padding:12px;background:#fff0bd;color:#302700;font:16px system-ui;display:flex;gap:20px;align-items:center';
                const label=document.createElement('span');label.textContent='QQ needs login';
                const button=document.createElement('button');button.textContent='Open QQ login window';
                button.onclick=()=>window.ipc.postMessage('qq-login');
                banner.append(label,button);document.body.append(banner);
            };"#)
            .with_ipc_handler(move |request| {
                if request.body() == "qq-login" { let _ = ipc_proxy.send_event(Ui::Menu("qq-login".into())); }
            })
            .with_on_page_load_handler(move |event, _| {
                if matches!(event, wry::PageLoadEvent::Finished) {
                    let _ = load_proxy.send_event(Ui::DocumentLoaded);
                }
            })
            .with_navigation_handler(move |raw| {
                if raw == "about:blank" { return true; }
                let Some(token) = nav_session.lock().ok().and_then(|token| token.clone()) else { return false; };
                let Some(url) = session_url(&nav_context, &raw, &token) else { return false; };
                if url::Url::parse(&raw).ok().is_some_and(|url| url.query_pairs().find(|(key, _)| key == "token").is_some_and(|(_, value)| value == token)) { return true; }
                let _ = proxy.send_event(Ui::Navigate(url));
                false
            })
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .build(&window).map_err(|error| format!("Cannot open the HER window. Check the WebView2 Runtime: {error}"))?;
        let (tx, rx) = mpsc::channel();
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || worker(root, context, cli, rx, proxy));
        window.set_visible(!hidden);
        let mut update_approval = None;
        let mut last_health = None;
        let mut qq_needed = false;
        let mut qq_state = json!("unknown");
        let mut login_notification = LoginNotification::default();
        let mut login_balloon = None;
        let mut loaded = false;
        event_loop.run(move |event, _, control| {
            *control = ControlFlow::Wait;
            match event {
                Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => window.set_visible(false),
                Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                    let _ = webview.set_bounds(wry::Rect { position: PhysicalPosition::new(0, 0).into(), size: PhysicalSize::new(size.width, size.height).into() });
                }
                Event::UserEvent(Ui::Menu(id)) => match id.as_str() {
                    "open" => { window.set_visible(true); window.set_focus(); let _ = tx.send(Action::Open); }
                    "restart" => { let _ = tx.send(Action::Restart); }
                    "updates" => { if let Some(body) = update_approval.clone() { let _ = tx.send(Action::Update(body)); } }
                    "qq-login" => { let _ = tx.send(Action::Login); }
                    "quit" => { let _ = tx.send(Action::Quit); login_balloon.take(); tray.take(); *control = ControlFlow::Exit; }
                    _ => {}
                },
                Event::UserEvent(Ui::Health(health)) => {
                    if Some(health) != last_health {
                        if let Some(tray) = tray.as_ref() {
                            if let Ok(icon) = Icon::from_rgba(pixels(health), 32, 32) { let _ = tray.set_icon(Some(icon)); }
                            let _ = tray.set_tooltip(Some(if qq_needed { "HER: QQ needs login" } else { match health { Health::Healthy => "HER: healthy", Health::Unhealthy => "HER: needs attention", Health::Unknown => "HER: health unavailable" } }));
                        }
                        last_health = Some(health);
                    }
                }
                Event::UserEvent(Ui::Qq(snapshot)) => {
                    qq_state = snapshot["state"].clone();
                    login_notification.observe(qq_state.as_str().unwrap_or("unknown"), || {
                        let Some(tray) = tray.as_ref() else { return false; };
                        match LoginBalloon::show(tray) {
                            Ok(balloon) => { login_balloon = Some(balloon); true }
                            Err(error) => { crate::safe_eprintln!("[desktop] {error}"); false }
                        }
                    });
                    qq_needed = snapshot["state"] == "needs-login";
                    qq_login.set_text(if qq_needed { "QQ needs login — Open QQ login window" } else if snapshot["state"] == "logged-in" { "QQ logged in" } else { "QQ login state unavailable" });
                    qq_login.set_enabled(qq_needed);
                    window.set_title(if qq_needed { "HER — QQ needs login" } else { "HER" });
                    if let Some(tray) = tray.as_ref() {
                        let _ = tray.set_tooltip(Some(if qq_needed { "HER: QQ needs login" } else { match last_health { Some(Health::Healthy) => "HER: healthy", Some(Health::Unhealthy) => "HER: needs attention", _ => "HER: health unavailable" } }));
                    }
                    let _ = webview.evaluate_script(&format!("if(window.herQqLogin&&document.body)window.herQqLogin({});", snapshot["state"]));
                }
                Event::UserEvent(Ui::Updates(state)) => {
                    updates.set_text(&state.label);
                    updates.set_enabled(state.approval.is_some());
                    update_approval = state.approval;
                }
                Event::UserEvent(Ui::UpdateResult(result)) => match result {
                    Ok(()) => { update_approval = None; updates.set_enabled(false); updates.set_text("Update approved"); }
                    Err(error) => show_error(&error),
                },
                Event::UserEvent(Ui::Login(result)) => { if let Err(error) = result { show_error(&error); } }
                Event::UserEvent(Ui::Session { url, token, navigate }) => {
                    if let Ok(mut current) = current.lock() { *current = Some(token.clone()); }
                    if navigate || !loaded {
                        if webview.load_url(&url).is_ok() { loaded = true; }
                    } else if let Ok(url) = url::Url::parse(&url) {
                        let script = format!("if(location.origin==={}){{localStorage.setItem('consoleToken',{});sessionStorage.setItem('consoleToken',{});}}", json!(url.origin().ascii_serialization()), json!(token), json!(token));
                        let _ = webview.evaluate_script(&script);
                    }
                    window.set_title(if qq_needed { "HER — QQ needs login" } else { "HER" });
                }
                Event::UserEvent(Ui::Navigate(url)) => { let _ = webview.load_url(&url); }
                Event::UserEvent(Ui::DocumentLoaded) => {
                    let _ = webview.evaluate_script(&format!("if(window.herQqLogin&&document.body)window.herQqLogin({qq_state});"));
                }
                Event::UserEvent(Ui::Notice(message)) => {
                    window.set_title("HER — console unavailable");
                    if !loaded { let _ = webview.evaluate_script(&format!("document.body.textContent={};", json!(message))); }
                }
                Event::UserEvent(Ui::Restart(result)) => match result {
                    Ok(()) => { if let Some(tray) = tray.as_ref() { let _ = tray.set_tooltip(Some("HER: restart requested")); } }
                    Err(error) => show_error(&error),
                },
                _ => {}
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn qq_login_notification_is_once_per_episode_and_unknown_does_not_rearm() {
            let mut notification = LoginNotification::default();
            let mut shown = 0;
            for (state, expected) in [
                ("unknown", false), ("logged-in", false), ("needs-login", true),
                ("needs-login", false), ("unknown", false), ("needs-login", false),
                ("logged-in", false), ("needs-login", true),
            ] {
                assert_eq!(notification.observe(state, || { shown += 1; true }), expected);
            }
            assert_eq!(shown, 2);
        }

        #[test]
        fn qq_login_notification_retries_native_submission_failure_on_a_later_poll() {
            let mut notification = LoginNotification::default();
            assert!(!notification.observe("needs-login", || false));
            assert!(!notification.observe("unknown", || panic!("unknown must not notify")));
            assert!(notification.observe("needs-login", || true));
            assert!(!notification.observe("needs-login", || panic!("episode already notified")));
        }

        #[test]
        fn health_child_reads_the_shared_verdict() {
            for (json, exit, expected, ready) in [
                (r#"{"v":1,"ok":true,"parts":{"console":{"verdict":"pass"}}}"#, "0", Health::Healthy, true),
                (r#"{"v":1,"ok":false,"parts":{"console":{"verdict":"fail"}}}"#, "1", Health::Unhealthy, false),
            ] {
                let script = format!("Write-Output '{json}'; exit {exit}");
                let mut command = crate::subprocess::command_without_console("powershell.exe");
                command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
                let (state, console) = health_command(&mut command).unwrap();
                assert!(state == expected);
                assert_eq!(console, ready);
            }
        }

        #[test]
        fn tray_health_polls_after_thirty_seconds() {
            let (_sender, receiver) = mpsc::channel();
            let started = Instant::now();
            assert!(matches!(poll_action(&receiver), Err(mpsc::RecvTimeoutError::Timeout)));
            assert!(started.elapsed() >= Duration::from_millis(29_500), "idle polling must wait 30 seconds: {:?}", started.elapsed());
        }

        #[test]
        fn stalled_health_child_times_out_as_unknown_and_leaves_other_children_alive() {
            struct Child(std::process::Child);
            impl Drop for Child {
                fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
            }
            let mut other = Child(crate::subprocess::command_without_console("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 60"])
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
            let identity = process_identity::identity_of(other.0.id()).unwrap().unwrap();
            println!("unrelated fixture pid={} start={}", identity.pid, identity.start);
            let mut command = crate::subprocess::command_without_console("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command",
                "Start-Sleep -Seconds 12; Write-Output '{\"v\":1,\"ok\":true,\"parts\":{\"console\":{\"verdict\":\"pass\"}}}'"]);
            let started = Instant::now();
            let (state, ready) = health_command(&mut command).unwrap_or((Health::Unknown, false));
            assert!(matches!(state, Health::Unknown) && !ready, "a stalled CLI must show unknown health");
            assert!(started.elapsed() < Duration::from_secs(11), "the child wait needs its own ten-second bound");
            assert!(other.0.try_wait().unwrap().is_none(), "a different child must remain alive");
        }
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tray_update_state_and_command_bind_to_the_displayed_release() {
        let mut status = json!({"ok":true,"settings":{"policy":"notify"},"current":"v1.2.3","available":{"tag":"v1.10.0"}});
        let item = TrayUpdate::from_status(&status);
        assert_eq!(item.label, "Update available: v1.2.3 → v1.10.0");
        assert_eq!(item.approval, Some(json!({"confirm":true,"tag":"v1.10.0"})));
        status["available"]["tag"] = "v2.0.0".into();
        assert_eq!(
            item.approval.unwrap()["tag"],
            "v1.10.0",
            "a menu click must retain the displayed version"
        );
        let bootstrap = TrayUpdate::from_status(&json!({"ok":true,"current":"bootstrap","available":{"tag":"v1.0.0"}}));
        assert_eq!(bootstrap.approval, Some(json!({"confirm":true,"tag":"v1.0.0"})));
        for (status, label) in [
            (json!({}), "Updates: status unavailable"),
            (
                json!({"ok":true,"settings":{"policy":"off"}}),
                "Updates disabled",
            ),
            (
                json!({"ok":true,"current":"v1.2.3","available":null}),
                "HER is up to date",
            ),
            (
                json!({"ok":true,"current":"v1.2.3","available":{"tag":"v1.2.2"}}),
                "Updates: status unavailable",
            ),
        ] {
            let item = TrayUpdate::from_status(&status);
            assert_eq!(item.label, label);
            assert_eq!(item.approval, None);
        }
    }
}
