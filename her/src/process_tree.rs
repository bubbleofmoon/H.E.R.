//! Owned child-process tree containment for synchronous subprocesses.
//!
//! Spawns a child in an owned Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) or
//! Unix process group (`process_group(0)`), terminates the full tree on stop/exit/drop, and
//! confirms all members have exited within a bounded grace period.

use std::process::{Child, ChildStdin, Command, ExitStatus};
use std::time::{Duration, Instant};

const CLEANUP_GRACE: Duration = Duration::from_millis(1_500);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(crate) enum SpawnError {
    Deadline(String),
    Failed(String),
}

impl SpawnError {
    fn operation(deadline: Instant, stage: &str, error: impl std::fmt::Display) -> Self {
        let message = format!("{stage}: {error}");
        if Instant::now() >= deadline {
            Self::Deadline(format!("deadline expired during {message}"))
        } else {
            Self::Failed(message)
        }
    }
}

pub(crate) struct ProcessTree {
    inner: os::Tree,
}

impl ProcessTree {
    /// Start a background child process tree with an overall setup timeout.
    pub(crate) fn spawn(command: Command, timeout: Duration) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "task timeout is out of range".to_string())?;
        Self::spawn_with_deadline(command, deadline).map_err(|failure| match failure {
            SpawnError::Deadline(message) | SpawnError::Failed(message) => message,
        })
    }

    /// Start a contained child process tree bounded by `deadline`.
    pub(crate) fn spawn_with_deadline(
        command: Command,
        deadline: Instant,
    ) -> Result<Self, SpawnError> {
        os::Tree::spawn_with_deadline(command, deadline).map(|inner| Self { inner })
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        self.inner.child_mut()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.child_mut().stdin.take()
    }

    pub(crate) fn id(&self) -> u32 {
        self.inner.id()
    }

    pub(crate) fn try_wait_root(&mut self) -> Result<Option<ExitStatus>, String> {
        self.inner.try_wait_root()
    }

    pub(crate) fn terminate_and_reap(&mut self) -> Result<(), String> {
        self.inner.terminate_and_reap()
    }

    pub(crate) fn force_close(&mut self) -> Result<(), String> {
        self.inner.force_close()
    }

    pub(crate) fn stop(&mut self) -> Result<(), String> {
        self.terminate_and_reap()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn spawn_unassigned_for_test(command: Command) -> Result<Self, String> {
        os::Tree::spawn_unassigned_for_test(command).map(|inner| Self { inner })
    }

    #[cfg(all(test, windows))]
    pub(crate) fn inject_wait_observation_error_for_test(&mut self) {
        self.inner.inject_wait_observation_error_for_test();
    }

    #[cfg(all(test, windows))]
    pub(crate) fn job_accounting_for_test(&self) -> Result<(u32, Vec<u32>), String> {
        self.inner.job_accounting_for_test()
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.inner.is_finished() {
            return;
        }
        if self.terminate_and_reap().is_err() {
            let _ = self.force_close();
        }
    }
}

/// Check whether any non-zombie process remains in Unix process group `pid`.
/// On Windows, the owning Job Object (`KILL_ON_JOB_CLOSE`) handles containment.
pub(crate) fn process_group_alive(pid: u32) -> bool {
    os::process_group_alive(pid)
}

#[cfg(windows)]
mod os {
    use super::{CLEANUP_GRACE, POLL_INTERVAL, SpawnError};
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenThread, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
        THREAD_SUSPEND_RESUME,
    };

    const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;
    const ERROR_NO_MORE_FILES: u32 = 18;
    const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;

    pub(super) fn process_group_alive(_pid: u32) -> bool {
        false
    }

    pub(super) struct OwnedHandle(HANDLE);

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

    pub(super) fn wait_stopped_until(
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
                    _ => {
                        return Err(format!(
                            "could not wait for CLI process: Win32 error {}",
                            unsafe { GetLastError() }
                        ));
                    }
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
                    job.0 .0,
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

        fn active_processes(&self) -> Result<u32, String> {
            if self.0 .0.is_null() {
                return Err("job handle was already closed".into());
            }
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            let mut returned_length = 0u32;
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0 .0,
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
            Ok(accounting.ActiveProcesses)
        }

        fn is_empty(&self) -> Result<bool, String> {
            self.active_processes().map(|active| active == 0)
        }

        fn process_ids(&self) -> Result<Vec<u32>, String> {
            if self.0 .0.is_null() {
                return Err("job handle was already closed".into());
            }
            // The PID list has an aligned variable-length ULONG_PTR tail.
            let mut buffer = vec![0usize; 65];
            loop {
                let queried = unsafe {
                    QueryInformationJobObject(
                        self.0 .0,
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
                        info.ProcessIdList.as_ptr(),
                        info.NumberOfProcessIdsInList as usize,
                    )
                };
                return Ok(ids.iter().map(|&pid| pid as u32).collect());
            }
        }

        fn process_handles(&self) -> Result<Vec<OwnedHandle>, String> {
            let ids = self.process_ids()?;
            let mut handles = Vec::new();
            for pid in ids {
                let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
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
            Ok(handles)
        }

        fn wait_empty_until(
            &self,
            processes: &[OwnedHandle],
            deadline: Instant,
        ) -> Result<bool, String> {
            wait_stopped_until(processes, deadline, || self.is_empty())
        }

        fn terminate(&self) -> Result<(), String> {
            if self.0 .0.is_null() {
                return Err("job handle was already closed".into());
            }
            if unsafe { TerminateJobObject(self.0 .0, 1) } != 0 {
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
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut matching_thread = None;
        let mut found = unsafe { Thread32First(snapshot.0, &mut entry) };
        if found == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_MORE_FILES {
                return Err(format!("Thread32First failed with Win32 error {error}"));
            }
        }
        while found != 0 {
            if entry.th32OwnerProcessID == process_id
                && matching_thread.replace(entry.th32ThreadID).is_some()
            {
                return Err(format!(
                    "suspended process {process_id} exposed more than one initial thread"
                ));
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            found = unsafe { Thread32Next(snapshot.0, &mut entry) };
            if found == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_NO_MORE_FILES {
                    return Err(format!("Thread32Next failed with Win32 error {error}"));
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

    pub(super) struct Tree {
        child: Child,
        job: JobObject,
        assigned: bool,
        root_reaped: bool,
        finished: bool,
        #[cfg(test)]
        fail_next_wait_observation: bool,
    }

    impl Tree {
        pub(super) fn spawn_with_deadline(
            mut command: Command,
            deadline: Instant,
        ) -> Result<Self, SpawnError> {
            if Instant::now() >= deadline {
                return Err(SpawnError::Deadline(
                    "deadline expired before creating the Windows job".into(),
                ));
            }
            let job = JobObject::new().map_err(|error| {
                SpawnError::operation(deadline, "creating Windows job", error)
            })?;
            // command_without_console set CREATE_NO_WINDOW; include it again while adding
            // CREATE_SUSPENDED so CommandExt's replacement semantics cannot clear that flag.
            command.creation_flags(CREATE_NO_WINDOW_FLAG | CREATE_SUSPENDED);
            let child = command
                .spawn()
                .map_err(|error| SpawnError::operation(deadline, "CreateProcess failed", error))?;
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
                    return Err(SpawnError::Deadline(
                        "deadline expired during suspended process creation".to_string(),
                    ));
                }
                let thread = open_primary_thread(tree.child.id()).map_err(|error| {
                    SpawnError::operation(deadline, "opening suspended primary thread", error)
                })?;
                if unsafe {
                    AssignProcessToJobObject(tree.job.0 .0, tree.child.as_raw_handle() as HANDLE)
                } == 0
                {
                    let error = format!(
                        "AssignProcessToJobObject failed with Win32 error {}",
                        unsafe { GetLastError() }
                    );
                    return Err(SpawnError::operation(
                        deadline,
                        "assigning process to job",
                        error,
                    ));
                }
                tree.assigned = true;
                let previous_suspend_count = unsafe { ResumeThread(thread.0) };
                if previous_suspend_count == u32::MAX {
                    let error = format!(
                        "ResumeThread failed with Win32 error {}",
                        unsafe { GetLastError() }
                    );
                    return Err(SpawnError::operation(
                        deadline,
                        "resuming contained process",
                        error,
                    ));
                }
                if previous_suspend_count != 1 {
                    return Err(SpawnError::operation(
                        deadline,
                        "resuming contained process",
                        format!(
                            "ResumeThread returned unexpected suspend count {previous_suspend_count}"
                        ),
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(SpawnError::Deadline(
                        "deadline expired immediately after resuming the contained process".into(),
                    ));
                }
                Ok(())
            })();
            if let Err(mut failure) = setup {
                let cleanup = tree.terminate_and_reap().err();
                if let Some(cleanup) = cleanup {
                    match &mut failure {
                        SpawnError::Deadline(message) | SpawnError::Failed(message) => {
                            message.push_str(&format!("; cleanup was not confirmed: {cleanup}"));
                        }
                    }
                }
                return Err(failure);
            }
            Ok(tree)
        }

        pub(super) fn child_mut(&mut self) -> &mut Child {
            &mut self.child
        }

        pub(super) fn id(&self) -> u32 {
            self.child.id()
        }

        pub(super) fn is_finished(&self) -> bool {
            self.finished
        }

        pub(super) fn try_wait_root(&mut self) -> Result<Option<ExitStatus>, String> {
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
        pub(super) fn spawn_unassigned_for_test(mut command: Command) -> Result<Self, String> {
            let job = JobObject::new()?;
            command.creation_flags(CREATE_NO_WINDOW_FLAG | CREATE_SUSPENDED);
            let child = command.spawn().map_err(|error| {
                format!("could not spawn the test-owned suspended child: {error}")
            })?;
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
        pub(super) fn inject_wait_observation_error_for_test(&mut self) {
            self.fail_next_wait_observation = true;
        }

        #[cfg(test)]
        pub(super) fn job_accounting_for_test(&self) -> Result<(u32, Vec<u32>), String> {
            Ok((self.job.active_processes()?, self.job.process_ids()?))
        }

        pub(super) fn terminate_and_reap(&mut self) -> Result<(), String> {
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
                    return Err(
                        "Windows job still had live processes after the cleanup grace".into(),
                    );
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
                                cleanup_deadline
                                    .saturating_duration_since(now)
                                    .min(POLL_INTERVAL),
                            );
                        }
                        Ok(None) => {
                            return Err(
                                "CLI root process was not reaped before cleanup deadline".into()
                            );
                        }
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
                issues.push(format!(
                    "could not terminate unassigned suspended CLI child: {error}"
                ));
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
                        issues.push(
                            "unassigned CLI child was not reaped before cleanup deadline".into(),
                        );
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
                std::thread::sleep(
                    cleanup_deadline
                        .saturating_duration_since(now)
                        .min(POLL_INTERVAL),
                );
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

        pub(super) fn force_close(&mut self) -> Result<(), String> {
            // KILL_ON_JOB_CLOSE is the final fallback when explicit termination/wait failed.
            // Close the job before joining any stdin writer so inherited pipe readers exit.
            self.job.0.close();
            self.assigned = false;
            let mut issues = Vec::new();
            match self.try_wait_root() {
                Ok(Some(_)) => {
                    self.root_reaped = true;
                    self.finished = true;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => issues.push(format!(
                    "could not observe CLI root after closing job: {error}"
                )),
            }
            if let Err(error) = self.child.kill() {
                issues.push(format!(
                    "could not terminate CLI root after closing job: {error}"
                ));
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
                            issues.push(format!(
                                "could not observe CLI root while reaping: {error}"
                            ));
                        }
                        if issues.is_empty() {
                            self.finished = true;
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
                    issues.push(
                        "force-close cleanup deadline elapsed without observing CLI root exit"
                            .into(),
                    );
                    return Err(issues.join("; "));
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn isolated_empty_job_accounting_requires_signaled_process_handles() {
        let mut command =
            crate::subprocess::command_without_console(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "process_tree::tests::process_tree_probe",
                "--nocapture",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut tree = Tree::spawn_unassigned_for_test(command).unwrap();
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, tree.child.id()) };
        assert!(!handle.is_null());
        let handles = [OwnedHandle(handle)];
        // Force the observed interleaving: accounting says zero while the pinned process
        // handle is still unsignaled. A suspended child makes this deterministic.
        let stopped = wait_stopped_until(&handles, Instant::now(), || Ok(true)).unwrap();
        tree.terminate_and_reap().unwrap();
        assert!(
            !stopped,
            "empty accounting alone must not report a live process stopped"
        );
        assert!(wait_stopped_until(&handles, Instant::now(), || Ok(true)).unwrap());
    }
}

#[cfg(unix)]
mod os {
    use super::{CLEANUP_GRACE, POLL_INTERVAL, SpawnError};
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::time::Instant;

    pub(super) fn process_group_alive(pid: u32) -> bool {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        let result = unsafe { libc::kill(-(pid as i32), 0) };
        if result == 0 {
            #[cfg(target_os = "linux")]
            {
                // Reaped leaders can leave dead descendants awaiting their
                // adoptive parent's wait. A zombie main thread can still have
                // live sibling threads, so check those before declaring exit.
                let Ok(entries) = std::fs::read_dir("/proc") else {
                    return true;
                };
                for entry in entries {
                    let Ok(entry) = entry else { return true };
                    if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                        continue;
                    }
                    let text = match std::fs::read_to_string(entry.path().join("stat")) {
                        Ok(text) => text,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => return true,
                    };
                    let Some((_, fields)) = text.rsplit_once(')') else {
                        return true;
                    };
                    let fields: Vec<&str> = fields.split_whitespace().collect();
                    if fields.get(2).and_then(|field| field.parse::<u32>().ok()) != Some(pid) {
                        continue;
                    }
                    if !matches!(fields.first(), Some(&"Z" | &"X")) {
                        return true;
                    }
                    let threads = match std::fs::read_dir(entry.path().join("task")) {
                        Ok(threads) => threads,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => return true,
                    };
                    for thread in threads {
                        let Ok(thread) = thread else { return true };
                        let text = match std::fs::read_to_string(thread.path().join("stat")) {
                            Ok(text) => text,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(_) => return true,
                        };
                        let Some((_, fields)) = text.rsplit_once(')') else {
                            return true;
                        };
                        if !matches!(fields.split_whitespace().next(), Some("Z" | "X")) {
                            return true;
                        }
                    }
                }
                return false;
            }
            #[cfg(not(target_os = "linux"))]
            return true;
        }
        io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    pub(super) struct Tree {
        child: Child,
        pgid: i32,
        root_reaped: bool,
        finished: bool,
    }

    impl Tree {
        pub(super) fn spawn_with_deadline(
            mut command: Command,
            deadline: Instant,
        ) -> Result<Self, SpawnError> {
            if Instant::now() >= deadline {
                return Err(SpawnError::Deadline(
                    "deadline expired before spawning Unix process group".into(),
                ));
            }
            command.process_group(0);
            let mut child = command.spawn().map_err(|error| {
                SpawnError::operation(deadline, "spawning process group", error)
            })?;
            let pgid = match i32::try_from(child.id()) {
                Ok(pgid) if pgid > 0 => pgid,
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SpawnError::Failed(
                        "spawned child PID cannot form a Unix process group".into(),
                    ));
                }
            };
            let mut tree = Self {
                child,
                pgid,
                root_reaped: false,
                finished: false,
            };
            if Instant::now() >= deadline {
                let mut detail = String::new();
                if let Err(cleanup) = tree.terminate_and_reap() {
                    detail = format!("; cleanup was not confirmed: {cleanup}");
                }
                return Err(SpawnError::Deadline(format!(
                    "deadline expired immediately after spawning process group{detail}"
                )));
            }
            Ok(tree)
        }

        pub(super) fn child_mut(&mut self) -> &mut Child {
            &mut self.child
        }

        pub(super) fn id(&self) -> u32 {
            self.child.id()
        }

        pub(super) fn is_finished(&self) -> bool {
            self.finished
        }

        pub(super) fn try_wait_root(&mut self) -> Result<Option<ExitStatus>, String> {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.root_reaped = true;
                    Ok(Some(status))
                }
                Ok(None) => Ok(None),
                Err(error) => Err(format!("could not observe CLI root process: {error}")),
            }
        }

        fn signal_and_wait(
            &mut self,
            deadline: Instant,
            issues: &mut Vec<String>,
        ) -> Result<(), String> {
            if unsafe { libc::kill(-self.pgid, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    issues.push(format!("cannot stop process group: {}", error.kind()));
                }
            }
            if !self.root_reaped {
                if let Err(error) = self.child.kill() {
                    match self.child.try_wait() {
                        Ok(Some(_)) => self.root_reaped = true,
                        Ok(None) => issues.push(format!(
                            "cannot stop task process: {}",
                            error.kind()
                        )),
                        Err(wait_error) => issues.push(format!(
                            "cannot stop task process: {}; wait failed: {}",
                            error.kind(),
                            wait_error.kind()
                        )),
                    }
                }
            }
            let mut reap_observation_error = None;
            loop {
                if !self.root_reaped {
                    match self.child.try_wait() {
                        Ok(Some(_)) => {
                            self.root_reaped = true;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            if Instant::now() >= deadline {
                                issues.push(format!("could not reap CLI child: {error}"));
                                return Err(issues.join("; "));
                            }
                            reap_observation_error.get_or_insert_with(|| error.to_string());
                        }
                    }
                }
                if self.root_reaped && !process_group_alive(self.pgid as u32) {
                    if let Some(error) = reap_observation_error {
                        issues.push(format!("root wait observation failed while reaping: {error}"));
                    }
                    return Ok(());
                }
                let now = Instant::now();
                if now >= deadline {
                    if !self.root_reaped {
                        issues.push(
                            "CLI root process was not reaped before cleanup deadline".into(),
                        );
                    } else {
                        issues.push(
                            "Unix process group still had live processes after the cleanup grace"
                                .into(),
                        );
                    }
                    return Err(issues.join("; "));
                }
                std::thread::sleep(deadline.saturating_duration_since(now).min(POLL_INTERVAL));
            }
        }

        pub(super) fn terminate_and_reap(&mut self) -> Result<(), String> {
            if self.finished {
                return Ok(());
            }
            let cleanup_deadline = Instant::now()
                .checked_add(CLEANUP_GRACE)
                .ok_or_else(|| "cleanup deadline is out of range".to_string())?;
            let mut issues = Vec::new();
            if !self.root_reaped {
                if let Err(error) = self.try_wait_root() {
                    issues.push(format!("root wait observation failed: {error}"));
                }
            }
            self.signal_and_wait(cleanup_deadline, &mut issues)?;
            if !issues.is_empty() {
                issues.push("Unix process tree was terminated and reaped".into());
                return Err(issues.join("; "));
            }
            self.finished = true;
            Ok(())
        }

        pub(super) fn force_close(&mut self) -> Result<(), String> {
            if self.finished {
                return Ok(());
            }
            let deadline = Instant::now()
                .checked_add(CLEANUP_GRACE)
                .ok_or_else(|| "force-close deadline is out of range".to_string())?;
            let mut issues = Vec::new();
            self.signal_and_wait(deadline, &mut issues)?;
            if !issues.is_empty() {
                issues.push("CLI root was terminated and reaped".into());
                return Err(issues.join("; "));
            }
            self.finished = true;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    const MODE_ENV: &str = "HER_PROCESS_TREE_TEST_MODE";
    const ROOT_PID_FILE_ENV: &str = "HER_PROCESS_TREE_ROOT_PID_FILE";
    const CHILD_PID_FILE_ENV: &str = "HER_PROCESS_TREE_CHILD_PID_FILE";
    const READY_FILE_ENV: &str = "HER_PROCESS_TREE_READY_FILE";
    const MONITOR_ACK_FILE_ENV: &str = "HER_PROCESS_TREE_MONITOR_ACK_FILE";

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(label: &str) -> Self {
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-process-tree-{label}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            )));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_marker(path: &Path, contents: &str) -> std::io::Result<()> {
        let mut temp = path.as_os_str().to_os_string();
        temp.push(format!(".{}.tmp", std::process::id()));
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, contents)?;
        std::fs::rename(temp, path)
    }

    fn wait_for_pid_file(path: &Path, timeout: Duration) -> u32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    struct PinnedProcess(usize);

    #[cfg(windows)]
    impl PinnedProcess {
        fn open(pid: u32) -> Self {
            use windows_sys::Win32::Foundation::GetLastError;
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
            const PROCESS_TERMINATE: u32 = 0x0001;
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(
                !handle.is_null(),
                "OpenProcess({pid}) failed with Win32 error {}",
                unsafe { GetLastError() }
            );
            Self(handle as usize)
        }

        fn is_alive(&self) -> bool {
            use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            match unsafe { WaitForSingleObject(self.0 as *mut std::ffi::c_void, 0) } {
                WAIT_TIMEOUT => true,
                WAIT_OBJECT_0 => false,
                other => panic!("unexpected WaitForSingleObject result {other}"),
            }
        }

        fn cleanup_kill(&self) {
            use windows_sys::Win32::System::Threading::TerminateProcess;
            unsafe {
                let _ = TerminateProcess(self.0 as *mut std::ffi::c_void, 1);
            }
        }
    }

    #[cfg(windows)]
    impl Drop for PinnedProcess {
        fn drop(&mut self) {
            use windows_sys::Win32::Foundation::CloseHandle;
            unsafe {
                CloseHandle(self.0 as *mut std::ffi::c_void);
            }
        }
    }

    #[cfg(unix)]
    struct PinnedProcess(u32);

    #[cfg(unix)]
    impl PinnedProcess {
        fn open(pid: u32) -> Self {
            Self(pid)
        }

        fn is_alive(&self) -> bool {
            let pid = i32::try_from(self.0).unwrap();
            if unsafe { libc::kill(pid, 0) } != 0 {
                return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
            }
            #[cfg(target_os = "linux")]
            {
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.0)) else {
                    return false;
                };
                if let Some((_, rest)) = stat.rsplit_once(')') {
                    if matches!(rest.split_whitespace().next(), Some("Z" | "X")) {
                        return false;
                    }
                }
            }
            true
        }

        fn cleanup_kill(&self) {
            if let Ok(pid) = i32::try_from(self.0) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    #[test]
    #[ignore = "synthetic probe spawned by process_tree unit tests"]
    fn process_tree_probe() {
        let mode = std::env::var(MODE_ENV).unwrap_or_default();
        match mode.as_str() {
            "parent-with-descendant" | "exit-leaving-descendant" => {
                let root_pid_file = PathBuf::from(std::env::var_os(ROOT_PID_FILE_ENV).unwrap());
                let child_pid_file = PathBuf::from(std::env::var_os(CHILD_PID_FILE_ENV).unwrap());
                let ready_file = PathBuf::from(std::env::var_os(READY_FILE_ENV).unwrap());
                let monitor_ack_file =
                    PathBuf::from(std::env::var_os(MONITOR_ACK_FILE_ENV).unwrap());
                write_marker(&root_pid_file, &format!("{}\n", std::process::id())).unwrap();

                let mut child =
                    crate::subprocess::command_without_console(std::env::current_exe().unwrap());
                child
                    .args([
                        "--ignored",
                        "--exact",
                        "process_tree::tests::process_tree_probe",
                        "--nocapture",
                    ])
                    .env(MODE_ENV, "descendant")
                    .env(CHILD_PID_FILE_ENV, &child_pid_file)
                    .env(READY_FILE_ENV, &ready_file)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut descendant = child.spawn().unwrap();
                write_marker(&child_pid_file, &format!("{}\n", descendant.id())).unwrap();

                let ack_deadline = Instant::now() + Duration::from_secs(5);
                while (!ready_file.is_file() || !monitor_ack_file.is_file())
                    && Instant::now() < ack_deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if mode == "exit-leaving-descendant" {
                    drop(descendant);
                    return;
                }
                std::thread::sleep(Duration::from_secs(10));
                let _ = descendant.kill();
                let _ = descendant.wait();
            }
            "descendant" => {
                let ready_file = PathBuf::from(std::env::var_os(READY_FILE_ENV).unwrap());
                write_marker(&ready_file, &format!("{}\n", std::process::id())).unwrap();
                std::thread::sleep(Duration::from_secs(10));
            }
            _ => {}
        }
    }

    fn probe_command(root: &Path, mode: &str) -> Command {
        let mut cmd = crate::subprocess::command_without_console(std::env::current_exe().unwrap());
        cmd.args([
            "--ignored",
            "--exact",
            "process_tree::tests::process_tree_probe",
            "--nocapture",
        ])
        .env(MODE_ENV, mode)
        .env(ROOT_PID_FILE_ENV, root.join("root.pid"))
        .env(CHILD_PID_FILE_ENV, root.join("child.pid"))
        .env(READY_FILE_ENV, root.join("child.ready"))
        .env(MONITOR_ACK_FILE_ENV, root.join("monitor.ack"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        cmd
    }

    #[test]
    fn process_tree_drop_terminates_root_and_descendant() {
        let dir = TempRoot::new("drop");
        let cmd = probe_command(dir.path(), "parent-with-descendant");
        let tree = ProcessTree::spawn(cmd, Duration::from_secs(10)).unwrap();
        let root_pid = wait_for_pid_file(&dir.path().join("root.pid"), Duration::from_secs(5));
        let child_pid = wait_for_pid_file(&dir.path().join("child.ready"), Duration::from_secs(5));
        let root_handle = PinnedProcess::open(root_pid);
        let child_handle = PinnedProcess::open(child_pid);
        assert!(root_handle.is_alive(), "root must be running before drop");
        assert!(
            child_handle.is_alive(),
            "descendant must be running before drop"
        );
        write_marker(&dir.path().join("monitor.ack"), "ok\n").unwrap();

        drop(tree);

        let root_alive = root_handle.is_alive();
        let child_alive = child_handle.is_alive();
        if root_alive {
            root_handle.cleanup_kill();
        }
        if child_alive {
            child_handle.cleanup_kill();
        }
        assert!(
            !root_alive,
            "dropping ProcessTree must terminate the root process"
        );
        assert!(
            !child_alive,
            "dropping ProcessTree must terminate all descendants"
        );
    }

    #[test]
    fn process_tree_stop_reaps_descendant_after_root_exits() {
        let dir = TempRoot::new("root-exit");
        let cmd = probe_command(dir.path(), "exit-leaving-descendant");
        let mut tree = ProcessTree::spawn(cmd, Duration::from_secs(10)).unwrap();
        let root_pid = wait_for_pid_file(&dir.path().join("root.pid"), Duration::from_secs(5));
        let child_pid = wait_for_pid_file(&dir.path().join("child.ready"), Duration::from_secs(5));
        let root_handle = PinnedProcess::open(root_pid);
        let child_handle = PinnedProcess::open(child_pid);
        assert!(root_handle.is_alive());
        assert!(child_handle.is_alive());
        write_marker(&dir.path().join("monitor.ack"), "ok\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = tree.try_wait_root().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "root did not exit within bound");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());

        tree.terminate_and_reap().unwrap();

        let child_alive = child_handle.is_alive();
        if child_alive {
            child_handle.cleanup_kill();
        }
        assert!(
            !root_handle.is_alive(),
            "root process must remain exited"
        );
        assert!(
            !child_alive,
            "terminate_and_reap must terminate surviving descendants after root exits"
        );
    }

    #[test]
    fn process_tree_deadline_expired_before_spawn_returns_deadline_error() {
        let dir = TempRoot::new("expired");
        let cmd = probe_command(dir.path(), "noop");
        let expired = Instant::now() - Duration::from_millis(1);
        match ProcessTree::spawn_with_deadline(cmd, expired) {
            Err(SpawnError::Deadline(_)) => {}
            Err(SpawnError::Failed(msg)) => panic!("expected Deadline error, got Failed: {msg}"),
            Ok(_) => panic!("expected Deadline error, got Ok"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn isolated_empty_job_accounting_requires_signaled_process_handles() {
        os::isolated_empty_job_accounting_requires_signaled_process_handles();
    }

    #[cfg(windows)]
    #[test]
    fn process_tree_job_accounting_and_pid_list_reflect_live_members() {
        let dir = TempRoot::new("job-accounting");
        let cmd = probe_command(dir.path(), "parent-with-descendant");
        let mut tree = ProcessTree::spawn(cmd, Duration::from_secs(10)).unwrap();
        let root_pid = wait_for_pid_file(&dir.path().join("root.pid"), Duration::from_secs(5));
        let child_pid = wait_for_pid_file(&dir.path().join("child.ready"), Duration::from_secs(5));
        assert_eq!(root_pid, tree.id());

        let (active, pids) = tree.job_accounting_for_test().unwrap();
        assert!(
            active >= 2,
            "expected at least 2 active job processes, got {active}"
        );
        assert!(
            pids.contains(&root_pid),
            "job PID list {pids:?} missing root {root_pid}"
        );
        assert!(
            pids.contains(&child_pid),
            "job PID list {pids:?} missing descendant {child_pid}"
        );
        write_marker(&dir.path().join("monitor.ack"), "ok\n").unwrap();

        tree.terminate_and_reap().unwrap();
        let (active_after, pids_after) = tree.job_accounting_for_test().unwrap();
        assert_eq!(active_after, 0);
        assert!(pids_after.is_empty(), "expected empty job PID list, got {pids_after:?}");
    }

    #[cfg(unix)]
    #[test]
    fn process_tree_unix_group_liveness_tracks_descendant() {
        let dir = TempRoot::new("unix-group");
        let cmd = probe_command(dir.path(), "parent-with-descendant");
        let mut tree = ProcessTree::spawn(cmd, Duration::from_secs(10)).unwrap();
        let root_pid = wait_for_pid_file(&dir.path().join("root.pid"), Duration::from_secs(5));
        let _child_pid = wait_for_pid_file(&dir.path().join("child.ready"), Duration::from_secs(5));
        assert_eq!(root_pid, tree.id());
        assert!(process_group_alive(root_pid));
        write_marker(&dir.path().join("monitor.ack"), "ok\n").unwrap();

        tree.terminate_and_reap().unwrap();
        assert!(!process_group_alive(root_pid));
    }
}
