//! Process instance identity, full inspection, matching and pinned termination.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// What identifies one process instance: a pid alone can be reused, a pid plus the OS start stamp cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Linux: `/proc/<pid>/stat` starttime (clock ticks since boot). Windows: creation FILETIME.
    pub start: u64,
    /// Executable path as the OS reports it.
    pub exe: String,
}

/// Result of comparing a recorded identity with the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// The same process instance is running.
    Matches,
    /// No process with that pid is running.
    Gone,
    /// The pid now belongs to another process instance or executable.
    Mismatch(String),
    /// The OS could not be asked (access denied, unreadable, unsupported platform).
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    Killed,
    /// The process had already exited.
    Gone,
    /// The pid is a different process instance now: nothing was stopped.
    Refused(String),
    /// The OS could not confirm the identity or the termination: treat the process as still running.
    Unknown(String),
}

/// The OS identity of `pid`: `Ok(None)` when no such process runs, `Err` when it cannot be determined.
pub fn identity_of(pid: u32) -> Result<Option<ProcessIdentity>, String> {
    os::identity_of(pid)
}

/// Foreground an existing QQ window while retaining a handle to the verified instance.
#[cfg(windows)]
pub fn focus_qq_window(expected: &ProcessIdentity) -> Result<(), String> {
    os::focus_qq_window(expected)
}

pub fn current_identity() -> Result<ProcessIdentity, String> {
    identity_of(std::process::id())?
        .ok_or_else(|| "own process identity is unavailable".to_string())
}

/// The token user of this process, independent of environment account names.
#[cfg(windows)]
pub fn current_user() -> Result<Vec<u8>, String> {
    os::current_user()
}

/// Resolve a live loopback TCP peer through the OS table and inspect its pinned
/// process token. Caller-provided process IDs never participate in this proof.
#[cfg(windows)]
pub fn tcp_peer_user(stream: &std::net::TcpStream) -> Result<Vec<u8>, String> {
    os::tcp_peer_user(stream)
}

pub fn same_identity(found: &ProcessIdentity, expected: &ProcessIdentity) -> bool {
    found.pid == expected.pid
        && found.start == expected.start
        && same_exe(&found.exe, &expected.exe)
}

/// Full inspection is deliberately separate from limited-rights instance liveness.
/// Arguments include argv[0]; owner markers retain only pid/start/exe.
#[derive(Debug, Clone, Serialize)]
pub struct Process {
    #[serde(flatten)]
    pub identity: ProcessIdentity,
    pub cwd: String,
    pub args: Vec<String>,
}

pub fn identify(pid: u32) -> Result<Option<Process>, String> {
    os::inspect(pid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Matches,
    Mismatch(String),
    /// A package launcher is not the resolved dependency entry. Never adopt it.
    Wrapper,
    Unknown(String),
}

/// Match the resolved program and actual script operand, never arbitrary argv text.
/// Relative entries use cwd first, with the program folder as a fallback.
pub fn matches(
    process: &Process,
    program: &std::path::Path,
    script: Option<&std::path::Path>,
) -> Match {
    fn compare(
        process: &Process,
        program: &std::path::Path,
        script: Option<&std::path::Path>,
    ) -> Result<Match, String> {
        let program = canonical(program)?;
        let found = canonical(std::path::Path::new(&process.identity.exe))?;
        if !same_exe(&program.to_string_lossy(), &found.to_string_lossy()) {
            return Ok(Match::Mismatch("configured program differs".into()));
        }
        let Some(script) = script else {
            return Ok(Match::Matches);
        };
        let Some(entry) = script_operand(&process.args)? else {
            return Ok(Match::Mismatch("process has no script entry".into()));
        };
        let name = entry.rsplit(['/', '\\']).next().unwrap_or(entry);
        if name.eq_ignore_ascii_case("npm-cli.js") || name.eq_ignore_ascii_case("npx-cli.js") {
            return Ok(Match::Wrapper);
        }
        let actual = resolve_script(process, std::path::Path::new(entry))?;
        let expected = resolve_script(process, script)?;
        if same_exe(&actual.to_string_lossy(), &expected.to_string_lossy()) {
            Ok(Match::Matches)
        } else {
            Ok(Match::Mismatch("configured script differs".into()))
        }
    }
    compare(process, program, script).unwrap_or_else(Match::Unknown)
}

fn canonical(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !path.is_absolute() {
        return Err("process/configured program must be absolute".into());
    }
    std::fs::canonicalize(path).map_err(|e| format!("cannot resolve process path: {}", e.kind()))
}

fn resolve_script(
    process: &Process,
    script: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    if script.is_absolute() {
        return canonical(script);
    }
    let cwd = std::path::Path::new(&process.cwd);
    if !cwd.is_absolute() {
        return Err("process working folder is unavailable".into());
    }
    // Only a missing cwd candidate permits fallback. Access errors are unknown.
    match std::fs::canonicalize(cwd.join(script)) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let folder = std::path::Path::new(&process.identity.exe)
                .parent()
                .ok_or("program folder is unavailable")?;
            canonical(&folder.join(script))
        }
        Err(e) => Err(format!(
            "cannot resolve script from working folder: {}",
            e.kind()
        )),
    }
}

/// The actual resolved interpreter entry, excluding package launchers and eval.
/// Used to distinguish a resolved foreign descendant from an unresolved wrapper.
pub fn resolved_script(process: &Process) -> Result<Option<std::path::PathBuf>, String> {
    let Some(entry) = script_operand(&process.args)? else {
        return Ok(None);
    };
    let name = entry.rsplit(['/', '\\']).next().unwrap_or(entry);
    if name.eq_ignore_ascii_case("npm-cli.js") || name.eq_ignore_ascii_case("npx-cli.js") {
        return Ok(None);
    }
    resolve_script(process, std::path::Path::new(entry)).map(Some)
}

/// Node's first script operand, preserving the launcher classification used by
/// dependency startup and restart. Unknown interpreter switches stay unknown, never ownership.
fn script_operand(args: &[String]) -> Result<Option<&str>, String> {
    let mut i = 1;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            return Ok(args.get(i + 1).map(String::as_str));
        }
        if matches!(arg, "-" | "-e" | "--eval" | "-p" | "--print")
            || arg.starts_with("--eval=")
            || arg.starts_with("--print=")
        {
            return Ok(None);
        }
        if matches!(
            arg,
            "-r" | "--require"
                | "--import"
                | "--loader"
                | "--experimental-loader"
                | "-C"
                | "--conditions"
                | "--inspect-port"
                | "--max-old-space-size"
                | "--max-semi-space-size"
                | "--disable-warning"
        ) {
            if args.get(i + 1).is_none() {
                return Err("interpreter option has no value".into());
            }
            i += 2;
            continue;
        }
        let flag = arg.split('=').next().unwrap_or(arg);
        if (arg.contains('=')
            && matches!(
                flag,
                "--require"
                    | "--import"
                    | "--loader"
                    | "--experimental-loader"
                    | "--conditions"
                    | "--inspect-port"
                    | "--max-old-space-size"
                    | "--max-semi-space-size"
            ))
            || matches!(
                flag,
                "--no-warnings"
                    | "--trace-warnings"
                    | "--enable-source-maps"
                    | "--experimental-strip-types"
                    | "--no-experimental-strip-types"
                    | "--inspect"
                    | "--inspect-brk"
            )
            || (arg.contains('=') && flag == "--disable-warning")
        {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("cannot classify interpreter option {flag}"));
        }
        return Ok(Some(arg));
    }
    Ok(None)
}

fn match_json(result: Match) -> serde_json::Value {
    match result {
        Match::Matches => serde_json::json!({"state":"matches"}),
        Match::Mismatch(reason) => serde_json::json!({"state":"mismatch", "reason":reason}),
        Match::Wrapper => serde_json::json!({"state":"wrapper"}),
        Match::Unknown(reason) => serde_json::json!({"state":"unknown", "reason":reason}),
    }
}

/// Read-only operator diagnostics; no service/config access or process mutation.
pub fn run_cli(args: &[String]) -> i32 {
    fn run(args: &[String]) -> Result<serde_json::Value, String> {
        let operation = args
            .first()
            .map(String::as_str)
            .ok_or("process identify|match needs options")?;
        if !matches!(operation, "identify" | "match" | "check") || (args.len() - 1) % 2 != 0 {
            return Err("usage: her-cli process identify|match|check --pid <id> --runtime-root <root> [--program <absolute path> [--script <path>]]".into());
        }
        let mut options = std::collections::BTreeMap::new();
        for pair in args[1..].chunks_exact(2) {
            let allowed = matches!(pair[0].as_str(), "--pid" | "--runtime-root")
                || operation == "match" && matches!(pair[0].as_str(), "--program" | "--script")
                || operation == "check" && pair[0] == "--start";
            if !allowed || options.insert(pair[0].as_str(), pair[1].as_str()).is_some() {
                return Err("unknown or duplicate process option".into());
            }
        }
        let root = std::path::Path::new(
            options
                .get("--runtime-root")
                .ok_or("--runtime-root is required")?,
        );
        crate::install_root::check_runtime(root, false)?;
        if !root.is_absolute() || !root.join("data").is_dir() {
            return Err("runtime data directory is unavailable".into());
        }
        let pid = options
            .get("--pid")
            .ok_or("--pid is required")?
            .parse::<u32>()
            .map_err(|_| "invalid pid")?;
        if operation == "check" {
            let start = options
                .get("--start")
                .ok_or("--start is required")?
                .parse::<u64>()
                .map_err(|_| "invalid creation stamp")?;
            return Ok(match check_liveness(pid, start) {
                Liveness::Matches => serde_json::json!({"state":"matches"}),
                Liveness::Gone => serde_json::json!({"state":"gone"}),
                Liveness::Mismatch(reason) => {
                    serde_json::json!({"state":"mismatch", "reason":reason})
                }
                Liveness::Unknown(reason) => {
                    serde_json::json!({"state":"unknown", "reason":reason})
                }
            });
        }
        let program = if operation == "match" {
            Some(std::path::Path::new(
                options.get("--program").ok_or("--program is required")?,
            ))
        } else {
            None
        };
        Ok(match identify(pid) {
            Ok(Some(process)) => match program {
                Some(program) => match_json(matches(
                    &process,
                    program,
                    options.get("--script").map(std::path::Path::new),
                )),
                None => serde_json::json!({"state":"identified", "process":process}),
            },
            Ok(None) => serde_json::json!({"state":"gone"}),
            Err(reason) => serde_json::json!({"state":"unknown", "reason":reason}),
        })
    }
    match run(args) {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("[process] {error}");
            2
        }
    }
}

fn same_exe(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        let norm = |s: &str| s.replace('/', "\\").to_lowercase();
        norm(left) == norm(right)
    } else {
        left == right
    }
}

/// Compare only PID and creation stamp. Full inspection/image access is not required.
pub fn check_liveness(pid: u32, start: u64) -> Liveness {
    match os::start_of(pid) {
        Err(reason) => Liveness::Unknown(reason),
        Ok(None) => Liveness::Gone,
        Ok(Some(found)) if found == start => Liveness::Matches,
        Ok(Some(_)) => Liveness::Mismatch(format!("pid {pid} has another creation time")),
    }
}

/// Compare a recorded identity with the running process of that pid.
pub fn check_identity(expected: &ProcessIdentity) -> Liveness {
    match identity_of(expected.pid) {
        Err(error) => Liveness::Unknown(error),
        Ok(None) => Liveness::Gone,
        Ok(Some(found)) if same_identity(&found, expected) => Liveness::Matches,
        Ok(Some(found)) => Liveness::Mismatch(format!(
            "pid {} is now another process (start {} exe {})",
            found.pid,
            found.start,
            file_name(&found.exe)
        )),
    }
}

fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// Stop `expected` only if the OS still reports exactly that process instance. The identity is checked through a handle
/// (Linux pidfd, Windows process handle) that pins the process, so a pid reused between the check and the kill cannot be
/// hit.
pub fn kill_verified(expected: &ProcessIdentity) -> KillOutcome {
    os::kill_verified(expected)
}

#[cfg(target_os = "linux")]
mod os {
    use super::{same_identity, KillOutcome, ProcessIdentity};

    pub(super) fn inspect(pid: u32) -> Result<Option<super::Process>, String> {
        let Some(identity) = identity_of(pid)? else {
            return Ok(None);
        };
        let read = || -> Result<super::Process, String> {
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                .map_err(|e| format!("cannot inspect process {pid} working folder: {e}"))?;
            let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map_err(|e| format!("cannot inspect process {pid} arguments: {e}"))?;
            let args = argv
                .strip_suffix(&[0])
                .unwrap_or(&argv)
                .split(|b| *b == 0)
                .map(|arg| {
                    String::from_utf8(arg.to_vec())
                        .map_err(|_| "process arguments are not Unicode".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(super::Process {
                identity: identity.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                args,
            })
        };
        let result = read();
        match identity_of(pid)? {
            None => Ok(None),
            Some(found) if same_identity(&found, &identity) => result.map(Some),
            Some(_) => Err("process instance changed during inspection".into()),
        }
    }

    pub(super) fn start_of(pid: u32) -> Result<Option<u64>, String> {
        if pid == 0 {
            return Ok(None);
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "process {pid} status is unreadable: {}",
                    error.kind()
                ))
            }
        };
        let (state, start) =
            parse_stat(&stat).ok_or_else(|| format!("process {pid} status is unparsable"))?;
        if state == 'Z' || state == 'X' {
            Ok(None)
        } else {
            Ok(Some(start))
        }
    }

    pub(super) fn identity_of(pid: u32) -> Result<Option<ProcessIdentity>, String> {
        let Some(start) = start_of(pid)? else {
            return Ok(None);
        };
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| format!("process {pid} executable is unreadable: {}", error.kind()));
        match start_of(pid)? {
            None => Ok(None),
            Some(found) if found != start => {
                Err("process instance changed during image inspection".into())
            }
            Some(_) => Ok(Some(ProcessIdentity {
                pid,
                start,
                exe: exe?,
            })),
        }
    }

    /// `(state, starttime)` from `/proc/<pid>/stat`; the command name may contain spaces and parentheses.
    fn parse_stat(stat: &str) -> Option<(char, u64)> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let state = fields.first()?.chars().next()?;
        let start = fields.get(19)?.parse().ok()?; // field 22 of the full line
        Some((state, start))
    }

    pub(super) fn kill_verified(expected: &ProcessIdentity) -> KillOutcome {
        if expected.pid == 0 || expected.pid > i32::MAX as u32 {
            return KillOutcome::Refused("invalid pid".to_string());
        }
        // SAFETY: plain syscalls with integer arguments; the returned descriptor is closed below.
        let fd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, expected.pid as libc::pid_t, 0) } as i32;
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                KillOutcome::Gone
            } else {
                KillOutcome::Unknown(format!(
                    "cannot pin process {}: {}",
                    expected.pid,
                    error.kind()
                ))
            };
        }
        // The descriptor now refers to one process instance. If the pid was reused before it was opened, the identity
        // read below belongs to the new process and does not match.
        let outcome = match identity_of(expected.pid) {
            Err(error) => KillOutcome::Unknown(error),
            Ok(None) => KillOutcome::Gone,
            Ok(Some(found)) if !same_identity(&found, expected) => {
                KillOutcome::Refused(format!("pid {} is another process now", expected.pid))
            }
            Ok(Some(_)) => {
                // SAFETY: valid descriptor, null siginfo.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        libc::SIGKILL,
                        std::ptr::null::<u8>(),
                        0,
                    )
                };
                if rc == 0 {
                    KillOutcome::Killed
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        KillOutcome::Gone
                    } else {
                        KillOutcome::Unknown(format!(
                            "cannot stop process {}: {}",
                            expected.pid,
                            error.kind()
                        ))
                    }
                }
            }
        };
        // SAFETY: fd was returned by pidfd_open above and is closed once.
        unsafe { libc::close(fd) };
        outcome
    }
}

#[cfg(windows)]
mod os {
    use super::{same_identity, KillOutcome, ProcessIdentity};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ,
    };

    const SYNCHRONIZE: u32 = 0x00100000;

    fn token_user(process: HANDLE) -> Result<Vec<u8>, String> {
        use windows_sys::Win32::Security::{
            GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        let mut token = std::ptr::null_mut();
        // SAFETY: the process handle remains pinned; token is an initialized out pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            // SAFETY: capture the failing call's thread-local error before any cleanup.
            let code = unsafe { GetLastError() };
            return Err(format!("process token user is unavailable: OpenProcessToken failed with Win32 error {code}"));
        }
        let token = Handle(token);
        let mut size = 0;
        // SAFETY: size query uses no output buffer.
        let size_error = if unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size)
        } == 0
        {
            // SAFETY: capture now; the normal size probe reports ERROR_INSUFFICIENT_BUFFER.
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        if size < std::mem::size_of::<TOKEN_USER>() as u32 || size > 65536 {
            return Err(match size_error {
                Some(code) => format!("process token user size is unavailable: GetTokenInformation (size) failed with Win32 error {code}"),
                None => "process token user size is unavailable".into(),
            });
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: pointer-aligned storage covers size bytes; the token handle is valid.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            // SAFETY: capture before the token handle is closed by Drop.
            let code = unsafe { GetLastError() };
            return Err(format!("process token user is unavailable: GetTokenInformation failed with Win32 error {code}"));
        }
        // SAFETY: a successful TokenUser query returns a TOKEN_USER and its SID in this buffer.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
        if !(8..=68).contains(&length) {
            return Err("process token SID is invalid".into());
        }
        Ok(unsafe { std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), length) }.to_vec())
    }

    pub(super) fn current_user() -> Result<Vec<u8>, String> {
        // SAFETY: the pseudo-handle is valid in this process and is not closed.
        token_user(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
    }

    #[cfg(test)]
    mod native_tests {
        #[test]
        fn token_user_failure_reports_api_and_win32_code() {
            // A null process handle deterministically fails without opening a token.
            let error = super::token_user(std::ptr::null_mut()).unwrap_err();
            assert!(error.contains("OpenProcessToken"), "{error}");
            assert!(error.contains("Win32 error 6"), "{error}"); // ERROR_INVALID_HANDLE
        }
    }

    fn tcp_peer_pid(stream: &std::net::TcpStream) -> Result<u32, String> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL};
        let (std::net::SocketAddr::V4(peer), std::net::SocketAddr::V4(local)) =
            (stream.peer_addr().map_err(|_| "TCP peer is unavailable")?, stream.local_addr().map_err(|_| "TCP listener is unavailable")?) else {
                return Err("desktop sign-in requires IPv4 loopback".into());
            };
        if !peer.ip().is_loopback() || !local.ip().is_loopback() {
            return Err("desktop sign-in requires loopback".into());
        }
        let mut size = 0;
        // SAFETY: query required IPv4 owner table size; AF_INET is 2.
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, 2, TCP_TABLE_OWNER_PID_ALL, 0) };
        for _ in 0..3 {
            if size < 4 || size > 16 * 1024 * 1024 { return Err("TCP owner table is unavailable".into()); }
            let mut bytes = vec![0u8; size as usize];
            let status = unsafe { GetExtendedTcpTable(bytes.as_mut_ptr().cast(), &mut size, 0, 2, TCP_TABLE_OWNER_PID_ALL, 0) };
            if status == 122 { continue; } // ERROR_INSUFFICIENT_BUFFER: table grew.
            if status != 0 { return Err("TCP owner table is unavailable".into()); }
            let count = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
            let width = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
            if count > (bytes.len() - 4) / width { return Err("TCP owner table is invalid".into()); }
            let mut found = None;
            for index in 0..count {
                // SAFETY: bounds checked above; read_unaligned avoids assumptions about Vec<u8> alignment.
                let row = unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(4 + index * width).cast::<MIB_TCPROW_OWNER_PID>()) };
                if row.dwState == 5 // MIB_TCP_STATE_ESTAB
                    && row.dwLocalAddr.to_ne_bytes() == peer.ip().octets()
                    && row.dwRemoteAddr.to_ne_bytes() == local.ip().octets()
                    && u16::from_be(row.dwLocalPort as u16) == peer.port()
                    && u16::from_be(row.dwRemotePort as u16) == local.port() {
                    if found.replace(row.dwOwningPid).is_some() { return Err("TCP peer ownership is ambiguous".into()); }
                }
            }
            return found.filter(|pid| *pid != 0).ok_or_else(|| "TCP peer process is unavailable".into());
        }
        Err("TCP owner table changed during inspection".into())
    }

    pub(super) fn tcp_peer_user(stream: &std::net::TcpStream) -> Result<Vec<u8>, String> {
        let pid = tcp_peer_pid(stream)?;
        let handle = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?.ok_or("TCP peer process exited")?;
        let before = identity(&handle, pid)?.ok_or("TCP peer process exited")?;
        let user = token_user(handle.0)?;
        let after = identity(&handle, pid)?.ok_or("TCP peer process exited")?;
        if !same_identity(&before, &after) || tcp_peer_pid(stream)? != pid {
            return Err("TCP peer process changed during inspection".into());
        }
        Ok(user)
    }

    // Process parameters are read only with VM_READ, separately from the limited
    // query handle used by owner checks. Support native and WOW64 parameter layouts.
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process: HANDLE,
            class: u32,
            info: *mut std::ffi::c_void,
            size: u32,
            returned: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn ReadProcessMemory(
            process: HANDLE,
            address: *const std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
        fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    #[link(name = "shell32")]
    extern "system" {
        fn CommandLineToArgvW(command: *const u16, count: *mut i32) -> *mut *mut u16;
    }

    fn read_memory(handle: &Handle, address: usize, length: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0; length];
        let mut read = 0;
        // SAFETY: OS validates the remote address; local buffer has the requested size.
        if unsafe {
            ReadProcessMemory(
                handle.0,
                address as _,
                bytes.as_mut_ptr() as _,
                length,
                &mut read,
            )
        } == 0
            || read != length
        {
            return Err(
                "cannot inspect process parameters (access denied or process exiting)".into(),
            );
        }
        Ok(bytes)
    }

    fn pointer(bytes: &[u8], width: usize) -> usize {
        if width == 4 {
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize
        } else {
            u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize
        }
    }

    fn unicode(handle: &Handle, address: usize, width: usize) -> Result<Vec<u16>, String> {
        let descriptor = read_memory(handle, address, width * 2)?;
        let length = u16::from_le_bytes(descriptor[..2].try_into().unwrap()) as usize;
        let maximum = u16::from_le_bytes(descriptor[2..4].try_into().unwrap()) as usize;
        if length > maximum || length % 2 != 0 {
            return Err("invalid process Unicode descriptor".into());
        }
        let bytes = read_memory(handle, pointer(&descriptor[width..], width), length)?;
        Ok(bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect())
    }

    fn parameters(handle: &Handle) -> Result<(String, Vec<String>), String> {
        let mut basic = [0usize; 6];
        // SAFETY: PROCESS_BASIC_INFORMATION is six pointer-sized fields.
        if unsafe {
            NtQueryInformationProcess(
                handle.0,
                0,
                basic.as_mut_ptr() as _,
                std::mem::size_of_val(&basic) as u32,
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err("cannot inspect process PEB".into());
        }
        let mut peb = basic[1];
        let mut width = std::mem::size_of::<usize>();
        #[cfg(target_pointer_width = "64")]
        {
            let mut wow = 0usize;
            if unsafe {
                NtQueryInformationProcess(
                    handle.0,
                    26,
                    &mut wow as *mut _ as _,
                    8,
                    std::ptr::null_mut(),
                )
            } < 0
            {
                return Err("cannot inspect process architecture".into());
            }
            if wow != 0 {
                peb = wow;
                width = 4;
            }
        }
        let offset = if width == 8 { 0x20 } else { 0x10 };
        let params = pointer(&read_memory(handle, peb + offset, width)?, width);
        let cwd = unicode(handle, params + if width == 8 { 0x38 } else { 0x24 }, width)?;
        let mut command = unicode(handle, params + if width == 8 { 0x70 } else { 0x40 }, width)?;
        if command.is_empty() || command.contains(&0) {
            return Err("process command line is unavailable".into());
        }
        command.push(0);
        let mut count = 0;
        // SAFETY: NUL-terminated input; Shell owns the returned argument array.
        let argv = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
        if argv.is_null() {
            return Err("cannot parse process arguments".into());
        }
        let result = (|| {
            let mut args = Vec::new();
            for i in 0..count as usize {
                let ptr = unsafe { *argv.add(i) };
                let mut len = 0;
                while unsafe { *ptr.add(len) } != 0 {
                    len += 1;
                }
                args.push(
                    String::from_utf16(unsafe { std::slice::from_raw_parts(ptr, len) })
                        .map_err(|_| "process argument is not Unicode")?,
                );
            }
            Ok((
                String::from_utf16(&cwd).map_err(|_| "process working folder is not Unicode")?,
                args,
            ))
        })();
        unsafe { LocalFree(argv as _) };
        result
    }

    pub(super) fn inspect(pid: u32) -> Result<Option<super::Process>, String> {
        if pid == 0 {
            return Err("system idle process cannot be inspected".into());
        }
        let Some(handle) = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)? else {
            return Ok(None);
        };
        let Some(identity) = identity(&handle, pid)? else {
            return Ok(None);
        };
        let details = parameters(&handle);
        if identity_of_handle_exited(&handle, pid)? {
            return Ok(None);
        }
        let (cwd, args) = details?;
        Ok(Some(super::Process {
            identity,
            cwd,
            args,
        }))
    }

    fn identity_of_handle_exited(handle: &Handle, pid: u32) -> Result<bool, String> {
        Ok(instance_start(handle, pid)?.is_none())
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle returned by OpenProcess, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn open(pid: u32, access: u32) -> Result<Option<Handle>, String> {
        // SAFETY: plain call; a null result is checked.
        let handle = unsafe { OpenProcess(access | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // SAFETY: reads the calling thread's last error.
            let error = unsafe { GetLastError() };
            return if error == ERROR_INVALID_PARAMETER {
                Ok(None)
            } else {
                Err(if error == 5 {
                    format!("cannot inspect process {pid}: access denied")
                } else {
                    format!("cannot open process {pid}: os error {error}")
                })
            };
        }
        Ok(Some(Handle(handle)))
    }

    fn running(handle: &Handle, pid: u32) -> Result<bool, String> {
        // SAFETY: open() includes SYNCHRONIZE, and this handle pins the instance.
        match unsafe { WaitForSingleObject(handle.0, 0) } {
            0 => Ok(false),  // WAIT_OBJECT_0: includes every exit code, even 259.
            258 => Ok(true), // WAIT_TIMEOUT
            _ => Err(format!("cannot observe process {pid} liveness")),
        }
    }

    fn instance_start(handle: &Handle, pid: u32) -> Result<Option<u64>, String> {
        if !running(handle, pid)? {
            return Ok(None);
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: valid handle and out pointers.
        if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(format!("cannot read process {pid} start time"));
        }
        let start = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        if running(handle, pid)? {
            Ok(Some(start))
        } else {
            Ok(None)
        }
    }

    pub(super) fn start_of(pid: u32) -> Result<Option<u64>, String> {
        if pid == 0 {
            return Err("system idle process cannot be inspected".into());
        }
        match open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? {
            None => Ok(None),
            Some(handle) => instance_start(&handle, pid),
        }
    }

    fn identity(handle: &Handle, pid: u32) -> Result<Option<ProcessIdentity>, String> {
        let Some(start) = instance_start(handle, pid)? else {
            return Ok(None);
        };
        let mut buffer = vec![0u16; 32_768];
        let mut size = buffer.len() as u32;
        // SAFETY: buffer and size describe a writable UTF-16 buffer.
        if unsafe {
            QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size)
        } == 0
        {
            if !running(handle, pid)? {
                return Ok(None);
            }
            return Err(format!("cannot read process {pid} executable"));
        }
        let exe = String::from_utf16_lossy(&buffer[..size as usize]);
        if !running(handle, pid)? {
            return Ok(None);
        }
        Ok(Some(ProcessIdentity { pid, start, exe }))
    }

    pub(super) fn identity_of(pid: u32) -> Result<Option<ProcessIdentity>, String> {
        if pid == 0 {
            return Ok(None);
        }
        match open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? {
            None => Ok(None),
            Some(handle) => identity(&handle, pid),
        }
    }

    pub(super) fn focus_qq_window(expected: &ProcessIdentity) -> Result<(), String> {
        type Window = *mut std::ffi::c_void;
        #[link(name = "user32")]
        extern "system" {
            fn EnumWindows(callback: Option<unsafe extern "system" fn(Window, isize) -> i32>, data: isize) -> i32;
            fn GetWindowThreadProcessId(window: Window, pid: *mut u32) -> u32;
            fn IsWindowVisible(window: Window) -> i32;
            fn IsIconic(window: Window) -> i32;
            fn ShowWindowAsync(window: Window, command: i32) -> i32;
            fn SetForegroundWindow(window: Window) -> i32;
            fn GetForegroundWindow() -> Window;
        }
        struct Search { pid: u32, window: Window }
        unsafe extern "system" fn visit(window: Window, data: isize) -> i32 {
            // SAFETY: EnumWindows is synchronous; data points to its live Search.
            let search = unsafe { &mut *(data as *mut Search) };
            let mut pid = 0;
            // SAFETY: native window handle supplied by EnumWindows and initialized out pointer.
            unsafe {
                GetWindowThreadProcessId(window, &mut pid);
                if pid == search.pid && IsWindowVisible(window) != 0 {
                    search.window = window;
                    return 0;
                }
            }
            1
        }
        let missing = "No verified QQ login window was found";
        if !std::path::Path::new(&expected.exe).file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("QQ.exe")) {
            return Err(missing.into());
        }
        let handle = open(expected.pid, PROCESS_QUERY_LIMITED_INFORMATION)?.ok_or(missing)?;
        let matches = || -> Result<bool, String> {
            Ok(identity(&handle, expected.pid)?.is_some_and(|found| same_identity(&found, expected)))
        };
        if !matches()? { return Err(missing.into()); }
        let mut search = Search { pid: expected.pid, window: std::ptr::null_mut() };
        // SAFETY: synchronous enumeration, callback state remains live through the call.
        unsafe { EnumWindows(Some(visit), (&mut search as *mut Search) as isize); }
        if search.window.is_null() || !matches()? { return Err(missing.into()); }
        let mut pid = 0;
        // SAFETY: recheck the window owner immediately before activation; the process handle
        // remains pinned and no process creation, termination or injection is performed.
        unsafe {
            GetWindowThreadProcessId(search.window, &mut pid);
            if pid != expected.pid { return Err(missing.into()); }
            if IsIconic(search.window) != 0 { ShowWindowAsync(search.window, 9); }
            if SetForegroundWindow(search.window) == 0 || GetForegroundWindow() != search.window {
                return Err("Windows could not bring the verified QQ login window to the front".into());
            }
        }
        Ok(())
    }

    pub(super) fn kill_verified(expected: &ProcessIdentity) -> KillOutcome {
        if expected.pid == 0 {
            return KillOutcome::Refused("invalid pid".to_string());
        }
        // The handle pins this process instance until it is closed: the identity check and the termination below
        // address the same process even if the pid is reused meanwhile.
        let handle = match open(
            expected.pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
        ) {
            Err(error) => return KillOutcome::Unknown(error),
            Ok(None) => return KillOutcome::Gone,
            Ok(Some(handle)) => handle,
        };
        match identity(&handle, expected.pid) {
            Err(error) => KillOutcome::Unknown(error),
            Ok(None) => KillOutcome::Gone,
            Ok(Some(found)) if !same_identity(&found, expected) => {
                KillOutcome::Refused(format!("pid {} is another process now", expected.pid))
            }
            Ok(Some(_)) => {
                // SAFETY: valid handle with PROCESS_TERMINATE access.
                if unsafe { TerminateProcess(handle.0, 1) } != 0 {
                    KillOutcome::Killed
                } else {
                    // SAFETY: reads the calling thread's last error.
                    let error = unsafe { GetLastError() };
                    KillOutcome::Unknown(format!(
                        "cannot stop process {}: os error {error}",
                        expected.pid
                    ))
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod os {
    use super::{KillOutcome, ProcessIdentity};

    pub(super) fn start_of(_pid: u32) -> Result<Option<u64>, String> {
        Err("process liveness is not supported on this platform".into())
    }

    pub(super) fn identity_of(_pid: u32) -> Result<Option<ProcessIdentity>, String> {
        Err("process identity is not supported on this platform".to_string())
    }

    pub(super) fn inspect(_pid: u32) -> Result<Option<super::Process>, String> {
        Err("process inspection is not supported on this platform".into())
    }

    pub(super) fn kill_verified(_expected: &ProcessIdentity) -> KillOutcome {
        KillOutcome::Unknown("process identity is not supported on this platform".to_string())
    }
}
