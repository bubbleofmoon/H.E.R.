//! Guarded files: atomic replacement, link checks and gate opens under one durability rule.
//! An atomic write refuses a linked or non-file target, fsyncs a fresh sibling temporary file,
//! replaces the target (Windows: write-through, with std's POSIX-semantics rename on access
//! denied), retries only Windows access/sharing/lock violations for at most one second, then
//! fsyncs the parent folder (Unix; Windows write-through covers it).
// herupdate compiles only release_store, install_root, update and handover.
#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    pub(crate) static STEP_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&'static str) -> io::Result<()>>>> =
        std::cell::RefCell::new(None);
}

fn step(name: &'static str) -> io::Result<()> {
    #[cfg(test)]
    if let Some(result) = STEP_HOOK.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook(name))) {
        return result;
    }
    let _ = name;
    Ok(())
}

/// A symlink, or any Windows reparse point (junction, mount point, symlink).
pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

/// Open the final component itself, never a link's target:
/// `O_NOFOLLOW` on Unix, `FILE_FLAG_OPEN_REPARSE_POINT` on Windows.
pub(crate) fn no_follow(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    options
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Open an existing gate only.
    Existing,
    /// Create the gate when it is missing.
    Create,
    /// Create with mode 0600; on Unix also require the caller's uid and a single hard link.
    Private,
}

#[derive(Debug)]
pub(crate) enum GateError {
    Open(io::Error),
    Redirected,
    Unsafe,
}

impl From<GateError> for io::Error {
    fn from(error: GateError) -> Self {
        match error {
            GateError::Open(error) => error,
            GateError::Redirected => io::Error::new(io::ErrorKind::PermissionDenied, "gate is redirected"),
            GateError::Unsafe => io::Error::new(io::ErrorKind::PermissionDenied, "gate is unsafe"),
        }
    }
}

/// Open a lock/gate file read-write without truncation, refusing links and non-files.
/// The caller takes the OS lock and keeps the handle; gates are never replaced or unlinked.
pub(crate) fn open_gate(path: &Path, gate: Gate) -> Result<File, GateError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(gate != Gate::Existing).truncate(false);
    no_follow(&mut options);
    #[cfg(unix)]
    if gate == Gate::Private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(GateError::Open)?;
    let metadata = file.metadata().map_err(GateError::Open)?;
    if !metadata.is_file() || is_link(&metadata) {
        return Err(GateError::Redirected);
    }
    #[cfg(unix)]
    if gate == Gate::Private {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(GateError::Unsafe);
        }
    }
    Ok(file)
}

#[derive(Debug)]
pub(crate) enum Failure {
    Io { class: &'static str, error: io::Error },
    Refused(String),
}

impl Failure {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Io { class, .. } => class,
            Self::Refused(_) => "refused",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { class, error } => {
                write!(f, "class={class} kind={:?} os={:?}", error.kind(), error.raw_os_error())
            }
            Self::Refused(message) => f.write_str(message),
        }
    }
}

impl From<Failure> for io::Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Io { error, .. } => error,
            Failure::Refused(message) => io::Error::new(io::ErrorKind::PermissionDenied, message),
        }
    }
}

fn io_failure(class: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |error| Failure::Io { class, error }
}

fn folder(target: &Path) -> &Path {
    target.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

fn check_target(target: &Path) -> Result<(), Failure> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => Ok(()),
        Ok(_) => Err(Failure::Refused("guarded file target is a link or not a regular file".into())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Failure::Io { class: "leaf/metadata", error }),
    }
}

/// A fsynced sibling temporary file waiting to replace its target. Dropping it unpublished removes it.
pub(crate) struct Staged {
    temp: PathBuf,
    target: PathBuf,
    published: bool,
}

/// Write `input` to a new sibling temporary file and fsync it. `unix_mode` sets the temporary
/// file's permissions on Unix (default: the process umask); Windows ignores it.
pub(crate) fn stage(target: &Path, input: &mut dyn Read, unix_mode: Option<u32>) -> Result<Staged, Failure> {
    check_target(target)?;
    let name = target
        .file_name()
        .ok_or_else(|| Failure::Refused("guarded file has no name".into()))?
        .to_string_lossy();
    let temp = folder(target).join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = unix_mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = unix_mode;
    let mut file = options.open(&temp).map_err(io_failure("temp/create"))?;
    let staged = Staged { temp, target: target.to_path_buf(), published: false };
    let written = io::copy(input, &mut file)
        .map_err(io_failure("temp/write"))
        .and_then(|_| step("temp/sync").and_then(|()| file.sync_all()).map_err(io_failure("temp/sync")));
    drop(file);
    written.map(|()| staged)
}

impl Staged {
    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    /// Replace the target, then fsync its folder. `before_each` runs before every attempt and its
    /// refusal is never retried; only Windows access/sharing/lock violations retry, for at most one second.
    pub(crate) fn commit(mut self, before_each: &mut dyn FnMut() -> Result<(), String>) -> Result<(), Failure> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            before_each().map_err(Failure::Refused)?;
            check_target(&self.target)?;
            match step("leaf/replace").and_then(|()| replace(&self.temp, &self.target)) {
                Ok(()) => break,
                Err(error) if transient(&error) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(Failure::Io { class: "leaf/replace", error }),
            }
        }
        self.published = true;
        step("dir/sync")
            .and_then(|()| sync_directory(folder(&self.target)))
            .map_err(io_failure("dir/sync"))
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Stage and commit `bytes` with no caller checks between the steps.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    stage(path, &mut &bytes[..], None)?.commit(&mut || Ok::<(), String>(()))
}

/// Persist a folder's entries (Unix). Windows replacements are write-through instead.
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn transient(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(windows)]
fn replace(temp: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both vectors are NUL-terminated and live through the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(5) {
        return Err(error);
    }
    // ERROR_ACCESS_DENIED: std's rename retries with POSIX-semantics replacement, which succeeds
    // while readers that allow deletion still hold the old file.
    fs::rename(temp, target)
}

#[cfg(not(windows))]
fn replace(temp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-guarded-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temps(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    struct Hook;
    impl Hook {
        fn set(hook: impl FnMut(&'static str) -> io::Result<()> + 'static) -> Self {
            STEP_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
            Hook
        }
    }
    impl Drop for Hook {
        fn drop(&mut self) {
            STEP_HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    #[test]
    fn write_atomic_syncs_the_file_then_replaces_then_syncs_the_folder() {
        let dir = temp_dir("order");
        let path = dir.join("state.json");
        fs::write(&path, b"old").unwrap();
        let steps = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = steps.clone();
        let hook = Hook::set(move |step| {
            seen.borrow_mut().push(step);
            Ok(())
        });
        write_atomic(&path, b"new").unwrap();
        drop(hook);
        assert_eq!(*steps.borrow(), ["temp/sync", "leaf/replace", "dir/sync"]);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_file_sync_keeps_the_old_file_and_no_temp() {
        let dir = temp_dir("file-sync");
        let path = dir.join("state.json");
        fs::write(&path, b"old").unwrap();
        let hook = Hook::set(|step| if step == "temp/sync" { Err(io::Error::other("injected")) } else { Ok(()) });
        let error = write_atomic(&path, b"new").unwrap_err();
        drop(hook);
        assert_eq!(error.class(), "temp/sync", "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_folder_sync_is_reported_after_publication() {
        let dir = temp_dir("dir-sync");
        let path = dir.join("state.json");
        fs::write(&path, b"old").unwrap();
        let hook = Hook::set(|step| if step == "dir/sync" { Err(io::Error::other("injected")) } else { Ok(()) });
        let error = write_atomic(&path, b"new").unwrap_err();
        drop(hook);
        assert_eq!(error.class(), "dir/sync", "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"new", "the replacement is already published");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_directory_target_is_refused() {
        let dir = temp_dir("dir-target");
        fs::create_dir(dir.join("state.json")).unwrap();
        let error = write_atomic(&dir.join("state.json"), b"new").unwrap_err();
        assert_eq!(error.class(), "refused", "{error}");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_target_is_refused_and_its_target_left_alone() {
        let dir = temp_dir("linked");
        let outside_dir = temp_dir("linked-outside");
        let outside = outside_dir.join("real.json");
        fs::write(&outside, b"outside").unwrap();
        let path = dir.join("state.json");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let error = write_atomic(&path, b"new").unwrap_err();
        assert_eq!(error.class(), "refused", "{error}");
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(outside_dir);
    }

    #[cfg(unix)]
    #[test]
    fn gates_refuse_links_and_private_gates_refuse_shared_inodes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("gates");
        let gate = dir.join("a.lock");
        assert!(matches!(open_gate(&gate, Gate::Existing), Err(GateError::Open(e)) if e.kind() == io::ErrorKind::NotFound));
        open_gate(&gate, Gate::Create).unwrap();
        open_gate(&gate, Gate::Existing).unwrap();
        let link = dir.join("link.lock");
        std::os::unix::fs::symlink(&gate, &link).unwrap();
        assert!(open_gate(&link, Gate::Create).is_err(), "a linked gate is never followed");
        let private = dir.join("private.lock");
        open_gate(&private, Gate::Private).unwrap();
        assert_eq!(fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o600);
        fs::hard_link(&private, dir.join("alias.lock")).unwrap();
        assert!(matches!(open_gate(&private, Gate::Private), Err(GateError::Unsafe)));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_reader_that_allows_deletion_does_not_block_replacement() {
        let dir = temp_dir("shared-reader");
        let path = dir.join("state.json");
        fs::write(&path, b"old").unwrap();
        let reader = File::open(&path).unwrap();
        write_atomic(&path, b"new").unwrap();
        drop(reader);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(temps(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
