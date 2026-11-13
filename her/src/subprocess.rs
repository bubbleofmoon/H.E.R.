use std::ffi::OsStr;
use std::process::Command;

/// Ordinary filesystem spelling for a process/config boundary. Never use this for pinning
/// or containment: those comparisons must keep their canonical paths.
pub(crate) fn external_path(path: impl AsRef<std::path::Path>) -> Result<String, String> {
    let text = path.as_ref().to_str().ok_or("external path is not Unicode")?;
    let Some(rest) = text.strip_prefix(r"\\?\") else { return Ok(text.to_string()) };
    let ordinary = if rest.get(..4).is_some_and(|s| s.eq_ignore_ascii_case(r"UNC\")) {
        let tail = &rest[4..];
        if tail.split('\\').take(2).filter(|s| !s.is_empty()).count() != 2 {
            return Err("verbatim UNC path has no server/share".into());
        }
        format!(r"\\{tail}")
    } else if rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && rest.as_bytes().get(1..3) == Some(b":\\") {
        rest.to_string()
    } else { return Err("verbatim device path has no ordinary filesystem spelling".into()) };
    // Removing the prefix must not change Win32 name resolution or require long-path opt-in
    // from external tools (notably Windows PowerShell 5.1).
    let tail = if ordinary.starts_with(r"\\") { &ordinary[2..] } else { &ordinary[3..] };
    if ordinary.encode_utf16().count() >= 260 || tail.split('\\').any(|part| {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        part.ends_with([' ', '.']) || part.contains(['/', ':', '"', '<', '>', '|', '?', '*'])
            || part.chars().any(|c| c < ' ')
            || ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            || (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(&stem[3..], "1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9"|"¹"|"²"|"³")
    }) { return Err("verbatim path cannot be represented safely for external tools".into()); }
    Ok(ordinary)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Call once at executable startup, before any threads or children are created.
/// Windows inherits *all* inheritable handles, even when a child's standard
/// streams are redirected. Keep our incoming pipes usable here, but prevent
/// long-lived supervisors/dependencies from retaining an ancestor's pipe ends.
/// Command still duplicates explicitly selected stdio for each intended child.
/// Cleanup is best-effort: try every stream and report failures once without
/// making startup depend on a launcher's standard handles or diagnostic sink.
pub(crate) fn prevent_stdio_inheritance() {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::io::Write;
        const HANDLE_FLAG_INHERIT: u32 = 1;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetStdHandle(kind: u32) -> *mut c_void;
            fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
            fn GetLastError() -> u32;
        }
        let mut failures = Vec::new();
        for (kind, stream) in [(-10i32, "stdin"), (-11, "stdout"), (-12, "stderr")] {
            // SAFETY: these are STD_INPUT/OUTPUT/ERROR_HANDLE. GUI images may
            // have no standard handles; neither null nor INVALID_HANDLE_VALUE
            // is an owned handle to modify.
            let handle = unsafe { GetStdHandle(kind as u32) };
            if !handle.is_null() && handle != -1isize as *mut c_void {
                // SAFETY: change only HANDLE_FLAG_INHERIT on our standard handle;
                // do not close it or alter the pipe/file or its access rights.
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                    // Read the error before formatting or attempting another handle.
                    let code = unsafe { GetLastError() };
                    failures.push(format!("{stream} (OS error {code})"));
                }
            }
        }
        if !failures.is_empty() {
            let _ = writeln!(
                std::io::stderr(),
                "[her] standard handle inheritance: {}; continuing startup",
                failures.join(", ")
            );
        }
    }
}

/// Create a background child without attaching a Windows console.
///
/// Keep stdio configuration and lifecycle management at each call site so this
/// only changes window creation. Other platforms retain Command::new behavior.
pub(crate) fn command_without_console(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(all(test, windows))]
mod tests {
    use super::command_without_console;
    use std::ffi::c_void;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, ExitStatus, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    type Hwnd = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> Hwnd;
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn SetStdHandle(kind: u32, handle: *mut c_void) -> i32;
        fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
        fn CreateEventW(
            attributes: *mut c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(Hwnd, isize) -> i32>,
            lparam: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
        fn GetForegroundWindow() -> Hwnd;
    }

    const HELPER_ENV: &str = "QQBOT_QUIET_SUBPROCESS_HELPER";
    const HELPER_TEST: &str = "subprocess::tests::child_process_probe";

    struct ChildGuard {
        child: Child,
    }

    impl ChildGuard {
        fn new(child: Child) -> Self {
            Self { child }
        }

        fn child_mut(&mut self) -> &mut Child {
            &mut self.child
        }

        fn wait_bounded(&mut self, timeout: Duration) -> ExitStatus {
            let started = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait().expect("poll helper process") {
                    return status;
                }
                assert!(
                    started.elapsed() < timeout,
                    "helper exceeded bounded wait of {timeout:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn stop_and_wait(&mut self) -> ExitStatus {
            if self
                .child
                .try_wait()
                .expect("poll helper before stop")
                .is_none()
            {
                self.child.kill().expect("stop helper at synthetic timeout");
            }
            self.child.wait().expect("reap helper process")
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    struct LineCapture {
        lines: Receiver<String>,
        reader: Option<JoinHandle<String>>,
    }

    impl LineCapture {
        fn new(reader: impl Read + Send + 'static) -> Self {
            let (sender, lines) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut output = String::new();
                let mut line = String::new();
                let mut reader = BufReader::new(reader);
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            output.push_str(&line);
                            let _ = sender.send(line.clone());
                        }
                        Err(error) => panic!("read helper output: {error}"),
                    }
                }
                output
            });
            Self {
                lines,
                reader: Some(reader),
            }
        }

        fn read_marker_line(&self, marker: &str, timeout: Duration) -> String {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "timed out waiting for helper output marker {marker}"
                );
                match self.lines.recv_timeout(remaining) {
                    Ok(line) if line.starts_with(marker) => return line,
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        panic!("timed out waiting for helper output marker {marker}")
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("helper exited before writing output marker {marker}")
                    }
                }
            }
        }

        fn finish(mut self) -> String {
            self.reader
                .take()
                .expect("reader thread is available")
                .join()
                .expect("join helper output reader")
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct WindowSample {
        hwnd: usize,
        process_id: u32,
    }

    unsafe extern "system" fn collect_visible_window(hwnd: Hwnd, lparam: isize) -> i32 {
        if IsWindowVisible(hwnd) != 0 {
            let mut process_id = 0;
            GetWindowThreadProcessId(hwnd, &mut process_id);
            let windows = &mut *(lparam as *mut Vec<WindowSample>);
            windows.push(WindowSample {
                hwnd: hwnd as usize,
                process_id,
            });
        }
        1
    }

    fn visible_windows() -> Vec<WindowSample> {
        let mut windows = Vec::new();
        let ok = unsafe {
            EnumWindows(
                Some(collect_visible_window),
                &mut windows as *mut Vec<WindowSample> as isize,
            )
        };
        assert_ne!(ok, 0, "Win32 EnumWindows observation must succeed");
        windows
    }

    fn foreground_window() -> WindowSample {
        let hwnd = unsafe { GetForegroundWindow() };
        let mut process_id = 0;
        if !hwnd.is_null() {
            unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
        }
        WindowSample {
            hwnd: hwnd as usize,
            process_id,
        }
    }

    fn spawn_helper(mode: &str) -> Child {
        let mut command = command_without_console(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER_TEST, "--nocapture"])
            .env(HELPER_ENV, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
            .spawn()
            .expect("start isolated quiet-process helper")
    }

    #[test]
    fn child_process_probe() {
        match std::env::var(HELPER_ENV).as_deref() {
            Ok("stdio") => {
                super::prevent_stdio_inheritance();
                let mut input = String::new();
                std::io::stdin()
                    .read_to_string(&mut input)
                    .expect("read synthetic helper input");
                println!("helper-stdout-marker:{}", input.trim());
                eprintln!("helper-stderr-marker");
                std::process::exit(23);
            }
            Ok("inherit") => {
                super::prevent_stdio_inheritance();
                let mut command = command_without_console(std::env::current_exe().unwrap());
                command
                    .args(["--exact", HELPER_TEST, "--nocapture"])
                    .env(HELPER_ENV, "stdio")
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
                let mut child =
                    ChildGuard::new(command.spawn().expect("start intended stdio child"));
                assert_eq!(child.wait_bounded(Duration::from_secs(5)).code(), Some(23));
            }
            Ok("stale-stdio") => {
                // Only this isolated probe mutates process-global standard handles.
                // Make two streams fail and require the later, valid stderr stream
                // to be processed too; keep stderr available for the diagnostic.
                unsafe {
                    let stdin = GetStdHandle(-10i32 as u32);
                    let stdout = GetStdHandle(-11i32 as u32);
                    let stderr = GetStdHandle(-12i32 as u32);
                    assert_ne!(SetHandleInformation(stderr, 1, 1), 0);
                    let stale = CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null());
                    assert!(!stale.is_null());
                    assert_ne!(CloseHandle(stale), 0);
                    assert_ne!(SetStdHandle(-10i32 as u32, stale), 0);
                    assert_ne!(SetStdHandle(-11i32 as u32, stale), 0);
                    super::prevent_stdio_inheritance();
                    assert_ne!(SetStdHandle(-10i32 as u32, stdin), 0);
                    assert_ne!(SetStdHandle(-11i32 as u32, stdout), 0);
                    let mut flags = 0;
                    assert_ne!(GetHandleInformation(stderr, &mut flags), 0);
                    assert_eq!(
                        flags & 1,
                        0,
                        "valid stderr must be processed after failures"
                    );
                }
                println!("helper-startup-continued");
            }
            Ok("sleep") => {
                println!("helper-sleep-ready");
                std::io::stdout().flush().expect("flush helper readiness");
                std::thread::sleep(Duration::from_secs(30));
            }
            Ok("console") => {
                let hwnd = unsafe { GetConsoleWindow() } as usize;
                println!("helper-console-hwnd={hwnd:#x}");
                std::io::stdout()
                    .flush()
                    .expect("flush console observation");
                std::thread::sleep(Duration::from_millis(800));
            }
            _ => {}
        }
    }

    #[test]
    fn background_child_keeps_stdio_and_exit_status() {
        let mut child = ChildGuard::new(spawn_helper("stdio"));
        let stdout = LineCapture::new(child.child_mut().stdout.take().unwrap());
        let stderr = LineCapture::new(child.child_mut().stderr.take().unwrap());
        {
            let mut stdin = child.child_mut().stdin.take().unwrap();
            stdin
                .write_all(b"synthetic-prompt\r\n")
                .expect("write synthetic stdin");
        }
        let status = child.wait_bounded(Duration::from_secs(5));
        let output = stdout.finish();
        let error_output = stderr.finish();

        assert_eq!(status.code(), Some(23));
        assert!(output.contains("helper-stdout-marker:synthetic-prompt"));
        assert!(error_output.contains("helper-stderr-marker"));
    }

    #[test]
    fn initialized_probe_preserves_explicit_child_stdio() {
        let mut child = ChildGuard::new(spawn_helper("inherit"));
        let stdout = LineCapture::new(child.child_mut().stdout.take().unwrap());
        let stderr = LineCapture::new(child.child_mut().stderr.take().unwrap());
        child
            .child_mut()
            .stdin
            .take()
            .unwrap()
            .write_all(b"inherited-prompt\r\n")
            .expect("write inherited stdin");
        let status = child.wait_bounded(Duration::from_secs(8));
        let output = stdout.finish();
        let error_output = stderr.finish();
        assert!(status.success(), "initialized parent failed: {status}");
        assert!(output.contains("helper-stdout-marker:inherited-prompt"));
        assert!(error_output.contains("helper-stderr-marker"));
    }

    #[test]
    fn stale_standard_handles_do_not_abort_startup() {
        let mut child = ChildGuard::new(spawn_helper("stale-stdio"));
        let stdout = LineCapture::new(child.child_mut().stdout.take().unwrap());
        let stderr = LineCapture::new(child.child_mut().stderr.take().unwrap());
        let status = child.wait_bounded(Duration::from_secs(5));
        let output = stdout.finish();
        let error_output = stderr.finish();
        assert!(
            status.success(),
            "stale-handle startup failed: {status}; {error_output}"
        );
        assert!(output.contains("helper-startup-continued"));
        let diagnostics: Vec<_> = error_output.lines().collect();
        assert_eq!(
            diagnostics.len(),
            1,
            "expected one diagnostic: {error_output}"
        );
        assert!(diagnostics[0].contains("standard handle inheritance"));
        assert!(diagnostics[0].contains("stdin (OS error 6)"));
        assert!(diagnostics[0].contains("stdout (OS error 6)"));
        assert!(!diagnostics[0].contains("stderr (OS error"));
    }

    #[test]
    fn background_child_can_be_stopped_at_timeout() {
        let started = Instant::now();
        let mut child = ChildGuard::new(spawn_helper("sleep"));
        let child_id = child.child_mut().id();
        let stdout = LineCapture::new(child.child_mut().stdout.take().unwrap());
        let stderr = LineCapture::new(child.child_mut().stderr.take().unwrap());
        stdout.read_marker_line("helper-sleep-ready", Duration::from_secs(3));

        std::thread::sleep(Duration::from_millis(250));
        let status = child.stop_and_wait();
        let elapsed = started.elapsed();
        assert_ne!(status.code(), Some(0));
        assert!(
            elapsed < Duration::from_secs(3),
            "helper exceeded timeout cleanup bound: {elapsed:?}"
        );
        let _ = stdout.finish();
        let _ = stderr.finish();
        println!(
            "timeout_helper_pid={child_id} elapsed_ms={}",
            elapsed.as_millis()
        );
    }

    #[test]
    fn background_child_has_no_console_window_or_focus() {
        let mut child = ChildGuard::new(spawn_helper("console"));
        let child_id = child.child_mut().id();
        let stdout = LineCapture::new(child.child_mut().stdout.take().unwrap());
        let stderr = LineCapture::new(child.child_mut().stderr.take().unwrap());
        let console_line = stdout.read_marker_line("helper-console-hwnd=", Duration::from_secs(3));
        assert!(
            console_line.trim_end().ends_with("=0x0"),
            "background helper acquired a console: {console_line:?}"
        );

        let mut foreground_samples = Vec::new();
        let mut visible_count_samples = Vec::new();
        for _ in 0..10 {
            let foreground = foreground_window();
            let windows = visible_windows();
            assert_ne!(
                foreground.process_id, child_id,
                "background helper became foreground: {foreground:?}"
            );
            assert!(
                windows.iter().all(|window| window.process_id != child_id),
                "background helper owns a visible window: {windows:?}"
            );
            foreground_samples.push(foreground);
            visible_count_samples.push(windows.len());
            std::thread::sleep(Duration::from_millis(50));
        }
        let status = child.wait_bounded(Duration::from_secs(3));
        let output = stdout.finish();
        let error_output = stderr.finish();
        assert!(status.success(), "window probe helper failed: {status}");
        println!("helper_pid={child_id} {console_line} visible_count_samples={visible_count_samples:?} foreground_samples={foreground_samples:?} output_bytes={} stderr_bytes={}", output.len(), error_output.len());
    }
}
