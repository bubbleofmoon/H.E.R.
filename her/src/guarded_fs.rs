//! OS-substitutable protected credential vault, handle-based Win32 DACL apply/verify,
//! native directory hardening, and symlink-safe protected file primitives.
//!
//! Layer 1 above `guarded_file`: depends only on `std`, OS primitives (`libc` / `windows_sys`),
//! `crate::guarded_file`, and `crate::process_identity` — never on `runtime`, `console*`,
//! `supervisor*`, or `desktop`.

pub(crate) use crate::guarded_file::{file_link_count, is_link, sync_directory};

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Trustee policy for protected files and directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AclPolicy {
    /// Only the current HER account SID (Unix: mode `0o600` file / `0o700` dir).
    OwnerOnly,
    /// Current HER account + `LocalSystem` + `BUILTIN\Administrators` (Unix: mode `0o600` file / `0o700` dir).
    OwnerSystemAdmins,
}

/// Outcome of verifying and conditionally hardening an entry's permissions / DACL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestrictOutcome {
    /// Entry already matched `policy` without needing a permission / DACL write.
    AlreadyPrivate,
    /// Entry permissions / DACL were updated to match `policy`.
    Hardened,
}

/// Failure from applying or verifying private permissions / DACLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AclError {
    /// The OS reported the entry missing (`ENOENT` / `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`).
    Missing(String),
    /// Any other failure or policy refusal.
    Refused(String),
}

impl From<AclError> for String {
    fn from(failure: AclError) -> Self {
        match failure {
            AclError::Missing(reason) | AclError::Refused(reason) => reason,
        }
    }
}

impl From<AclError> for io::Error {
    fn from(failure: AclError) -> Self {
        match failure {
            AclError::Missing(reason) => io::Error::new(io::ErrorKind::NotFound, reason),
            AclError::Refused(reason) => io::Error::new(io::ErrorKind::PermissionDenied, reason),
        }
    }
}

fn formatting(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// A path as diagnostics show it: control and bidirectional-formatting characters become `?`.
pub(crate) fn sanitized_path(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if formatting(c) { '?' } else { c })
        .collect()
}

/// Win32 `ERROR_FILE_NOT_FOUND` (2) and `ERROR_PATH_NOT_FOUND` (3).
#[cfg(windows)]
pub(crate) fn win32_vanished(code: u32) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

/// Whether an I/O error reports the entry itself missing (`ERROR_FILE_NOT_FOUND`/`ERROR_PATH_NOT_FOUND`
/// on Windows, `ENOENT` elsewhere).
pub(crate) fn vanished(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
            .is_some_and(win32_vanished)
    }
    #[cfg(not(windows))]
    {
        error.kind() == io::ErrorKind::NotFound
    }
}

#[cfg(not(windows))]
fn native_error_code(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("errno {code}"),
        None => format!("{:?}", error.kind()),
    }
}

/// Create a new private file before any content or handle is visible to another account.
pub(crate) fn create_protected_file(path: &Path, policy: AclPolicy) -> io::Result<File> {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    {
        windows::create_protected_file(path, policy)
    }
}

/// Stage `input` into a fresh sibling temporary file created with `policy`.
pub(crate) fn stage_protected_file(
    target: &Path,
    input: &mut dyn Read,
    policy: AclPolicy,
) -> Result<crate::guarded_file::Staged, crate::guarded_file::Failure> {
    crate::guarded_file::stage_with(target, input, |temp| create_protected_file(temp, policy))
}

/// Restrict an existing regular file or directory to `policy` without following links.
pub(crate) fn restrict_path(path: &Path, policy: AclPolicy) -> Result<(), AclError> {
    restrict_path_inner(path, None, policy)
}

/// Restrict an existing regular file (refusing directories and links) to `policy`.
pub(crate) fn restrict_file(path: &Path, policy: AclPolicy) -> Result<(), AclError> {
    restrict_path_inner(path, Some(false), policy)
}

fn restrict_path_inner(
    path: &Path,
    expected_dir: Option<bool>,
    policy: AclPolicy,
) -> Result<(), AclError> {
    #[cfg(unix)]
    {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if vanished(&error) => {
                return Err(AclError::Missing(format!(
                    "private path {}: symlink_metadata failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                )));
            }
            Err(error) => {
                return Err(AclError::Refused(format!(
                    "private path {}: symlink_metadata failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                )));
            }
        };
        if is_link(&meta)
            || !(meta.is_file() || meta.is_dir())
            || expected_dir.is_some_and(|dir| dir != meta.is_dir())
        {
            return Err(AclError::Refused(format!(
                "private path {}: expected regular entry",
                sanitized_path(path)
            )));
        }
        unix::check_ownership(path, &meta)?;
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        unix::apply_mode(path, mode)
    }
    #[cfg(windows)]
    {
        windows::restrict_path(path, expected_dir, policy)
    }
}

/// Restrict an already-inspected directory entry (file or directory) to `policy`.
/// On Unix, regular files preserve their owner execute bit (`(mode & 0o700) | 0o600`).
pub(crate) fn restrict_entry(
    path: &Path,
    meta: &fs::Metadata,
    policy: AclPolicy,
) -> Result<(), AclError> {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::PermissionsExt;
        if is_link(meta) || !(meta.is_file() || meta.is_dir()) {
            return Err(AclError::Refused(format!(
                "private path {}: expected regular file or directory",
                sanitized_path(path)
            )));
        }
        unix::check_ownership(path, meta)?;
        let mode = if meta.is_dir() {
            0o700
        } else {
            (meta.permissions().mode() & 0o700) | 0o600
        };
        unix::apply_mode(path, mode)
    }
    #[cfg(windows)]
    {
        if is_link(meta) || !(meta.is_file() || meta.is_dir()) {
            return Err(AclError::Refused(format!(
                "private path {}: expected regular file or directory",
                sanitized_path(path)
            )));
        }
        windows::restrict_path(path, Some(meta.is_dir()), policy)
    }
}

/// Verify on one no-follow handle/FD whether `path` already matches `policy`, hardening it on
/// that same handle only when needed.
pub(crate) fn ensure_entry_private(
    path: &Path,
    expected_dir: Option<bool>,
    policy: AclPolicy,
) -> Result<RestrictOutcome, AclError> {
    #[cfg(unix)]
    {
        unix::ensure_entry_private(path, expected_dir, policy)
    }
    #[cfg(windows)]
    {
        windows::ensure_entry_private(path, expected_dir, policy)
    }
}

/// Apply and verify `policy` DACL/mode on an already-open handle.
/// On Windows, the caller's handle must include `WRITE_DAC | READ_CONTROL`.
#[cfg(test)]
pub(crate) fn restrict_handle(
    file: &File,
    path: &Path,
    directory: bool,
    policy: AclPolicy,
) -> Result<(), AclError> {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::PermissionsExt;
        let meta = file.metadata().map_err(|error| {
            AclError::Refused(format!(
                "private path {}: metadata failed ({})",
                sanitized_path(path),
                native_error_code(&error)
            ))
        })?;
        if is_link(&meta) || meta.is_dir() != directory {
            return Err(AclError::Refused(format!(
                "private path {}: unexpected entry type",
                sanitized_path(path)
            )));
        }
        unix::check_ownership(path, &meta)?;
        let mode = if directory { 0o700 } else { 0o600 };
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| {
                let reason = format!(
                    "private permissions could not be applied: {}: set_permissions failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                );
                if vanished(&error) {
                    AclError::Missing(reason)
                } else {
                    AclError::Refused(reason)
                }
            })
    }
    #[cfg(windows)]
    {
        windows::restrict_handle(file, path, directory, policy)
    }
}

/// Verify that `path` exists as the expected type (not a link) and its DACL/mode matches `policy` exactly.
#[cfg(test)]
pub(crate) fn verify_path_exact_acl(
    path: &Path,
    directory: bool,
    policy: AclPolicy,
) -> Result<(), AclError> {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::PermissionsExt;
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if vanished(&error) => {
                return Err(AclError::Missing(format!(
                    "private path {}: symlink_metadata failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                )));
            }
            Err(error) => {
                return Err(AclError::Refused(format!(
                    "private path {}: symlink_metadata failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                )));
            }
        };
        if is_link(&meta) || meta.is_dir() != directory || (!directory && !meta.is_file()) {
            return Err(AclError::Refused(format!(
                "private path {}: verify failed: unexpected type or link",
                sanitized_path(path)
            )));
        }
        unix::check_ownership(path, &meta)?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(AclError::Refused(format!(
                "private path {}: verify failed: group or other access is enabled",
                sanitized_path(path)
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows::verify_path_exact_acl(path, directory, policy)
    }
}

/// Convenience predicate returning `true` when `path` is a regular file matching `policy`.
#[cfg(test)]
pub(crate) fn verify_file_private(path: &Path, policy: AclPolicy) -> bool {
    verify_path_exact_acl(path, false, policy).is_ok()
}

/// Verify on an already-open handle that ownership and every non-zero allow ACE belong to `policy` trustees.
#[cfg(any(windows, test))]
pub(crate) fn verify_handle_trustees(file: &File, policy: AclPolicy) -> Result<(), AclError> {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file
            .metadata()
            .map_err(|error| AclError::Refused(format!("handle metadata failed: {error}")))?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || (meta.permissions().mode() & 0o077) != 0 {
            return Err(AclError::Refused(
                "file ownership or permissions are unsafe".into(),
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows::verify_handle_trustees(file, policy)
    }
}

/// Open `path` without following links, verify handle ownership/ACL against `policy`, and read all bytes
/// from that same pinned handle.
#[cfg(any(windows, test))]
pub(crate) fn read_verified_private_file(path: &Path, policy: AclPolicy) -> Result<Vec<u8>, String> {
    let error = || "protected file is unavailable or has unsafe ownership/ACL".to_string();
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny concurrent writers and replacement on the pinned file handle.
        options.share_mode(1).custom_flags(0x0020_0000);
    }
    let mut file = options.open(path).map_err(|_| error())?;
    let meta = file.metadata().map_err(|_| error())?;
    if !meta.is_file() || is_link(&meta) || file_link_count(&file).ok() != Some(1) {
        return Err(error());
    }
    verify_handle_trustees(&file, policy).map_err(|_| error())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| error())?;
    Ok(bytes)
}

/// Check whether `dir` is a non-linked directory whose DACL/mode excludes inherited and foreign grants.
pub(crate) fn is_directory_hardened(dir: &Path, policy: AclPolicy) -> bool {
    #[cfg(unix)]
    {
        let _ = policy;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::symlink_metadata(dir).is_ok_and(|meta| {
            meta.is_dir()
                && !is_link(&meta)
                // SAFETY: geteuid has no preconditions.
                && meta.uid() == unsafe { libc::geteuid() }
                && (meta.permissions().mode() & 0o077) == 0
        })
    }
    #[cfg(windows)]
    {
        windows::is_directory_hardened(dir, policy)
    }
}

/// Harden `dir` and its non-linked descendants natively (no subprocesses), parent-first, skipping vanished
/// listed entries.
pub(crate) fn harden_directory_tree(dir: &Path, policy: AclPolicy) -> Result<(), String> {
    fn inspect_entry(path: &Path, listed: bool) -> Result<Option<fs::Metadata>, String> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if listed && vanished(&error) => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "directory entry {} cannot be inspected ({error})",
                    sanitized_path(path)
                ));
            }
        };
        if is_link(&meta) {
            return Err(format!(
                "directory entry {} is a link or reparse point",
                sanitized_path(path)
            ));
        }
        if !meta.is_file() && !meta.is_dir() {
            return Err(format!(
                "directory entry {} is not a regular file or directory",
                sanitized_path(path)
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions.
            if meta.uid() != unsafe { libc::geteuid() } || (meta.is_file() && meta.nlink() != 1) {
                if listed && meta.nlink() == 0 {
                    return Ok(None);
                }
                return Err(format!(
                    "directory entry {} does not belong exclusively to the current account",
                    sanitized_path(path)
                ));
            }
        }
        Ok(Some(meta))
    }

    fn collect(
        path: &Path,
        listed: bool,
        entries: &mut Vec<(PathBuf, fs::Metadata, bool)>,
    ) -> Result<(), String> {
        let Some(meta) = inspect_entry(path, listed)? else {
            return Ok(());
        };
        if meta.is_dir() {
            let children = match fs::read_dir(path) {
                Ok(children) => children,
                Err(error) if listed && vanished(&error) => return Ok(()),
                Err(error) => {
                    return Err(format!(
                        "directory {} cannot be read ({error})",
                        sanitized_path(path)
                    ));
                }
            };
            for child in children {
                let child = match child {
                    Ok(child) => child.path(),
                    Err(error) if listed && vanished(&error) => break,
                    Err(error) => {
                        return Err(format!(
                            "directory {} entry cannot be read ({error})",
                            sanitized_path(path)
                        ));
                    }
                };
                collect(&child, true, entries)?;
            }
        }
        entries.push((path.to_path_buf(), meta, listed));
        Ok(())
    }

    let root_meta = inspect_entry(dir, false)?
        .ok_or_else(|| format!("directory {} is unavailable", sanitized_path(dir)))?;
    if !root_meta.is_dir() {
        return Err(format!("{} is not a directory", sanitized_path(dir)));
    }
    let mut entries = Vec::new();
    collect(dir, false, &mut entries)?;
    // Parent-first closes inherited/external access before hardening children.
    for (path, meta, listed) in entries.into_iter().rev() {
        let mut current_meta = meta;
        let mut retried = false;
        loop {
            match restrict_entry(&path, &current_meta, policy) {
                Ok(()) => break,
                Err(AclError::Refused(reason)) => return Err(reason),
                Err(AclError::Missing(reason)) => {
                    let Some(fresh) = inspect_entry(&path, listed)? else {
                        break;
                    };
                    if retried {
                        return Err(reason);
                    }
                    current_meta = fresh;
                    retried = true;
                }
            }
        }
    }
    fs::read_dir(dir).map_err(|error| {
        format!(
            "directory {} is unreadable after hardening ({error})",
            sanitized_path(dir)
        )
    })?;
    Ok(())
}

/// Format the operator manual hardening command for `dir`.
pub(crate) fn manual_harden_command(dir: &Path) -> String {
    let dir_str = dir.display().to_string();
    #[cfg(windows)]
    {
        let d = format!("\"{dir_str}\"");
        [
            format!(
                "icacls {d} /T /C /Q /grant \"%USERNAME%:F\" /grant \"*S-1-5-18:F\" /grant \"*S-1-5-32-544:F\""
            ),
            format!(
                "icacls {d} /Q /grant \"%USERNAME%:(OI)(CI)F\" /grant \"*S-1-5-18:(OI)(CI)F\" /grant \"*S-1-5-32-544:(OI)(CI)F\""
            ),
            format!("icacls {d} /inheritance:r /C /Q"),
            format!("icacls {d} /remove:g \"*S-1-5-11\" \"*S-1-5-32-545\" \"*S-1-1-0\" /T /C /Q"),
        ]
        .join("\n             ")
    }
    #[cfg(not(windows))]
    {
        format!("chmod -R go-rwx \"{dir_str}\"")
    }
}

/// Open an existing regular file with no link following and a single hard link (`nlink == 1`).
pub(crate) fn open_protected_file(path: &Path) -> io::Result<Option<File>> {
    crate::guarded_file::open_no_follow_regular(path, true)
}

/// Read up to `limit` bytes from a protected regular file and re-verify resolved path identity via `recheck`.
pub(crate) fn read_protected_file_with_recheck(
    path: &Path,
    limit: u64,
    recheck: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(bytes) = crate::guarded_file::read_bounded_no_follow(path, limit, true)
        .map_err(|error| format!("protected file read failed: {error}"))?
    else {
        return Ok(None);
    };
    if recheck()? != path {
        return Err("protected file path changed during read".into());
    }
    Ok(Some(bytes))
}

/// Remove a protected regular file if present (checked via `open_protected_file`) and sync its parent directory.
pub(crate) fn remove_protected_file_with(
    path: &Path,
    remove: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if open_protected_file(path)
        .map_err(|error| format!("protected file inspect failed: {error}"))?
        .is_some()
    {
        remove()?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "protected file has no parent directory".to_string())?;
    sync_directory(parent).map_err(|error| format!("protected directory sync failed: {error}"))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    pub(super) fn check_ownership(path: &Path, meta: &fs::Metadata) -> Result<(), AclError> {
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || (meta.is_file() && meta.nlink() != 1) {
            return Err(AclError::Refused(format!(
                "private path {}: must belong only to the HER account",
                sanitized_path(path)
            )));
        }
        Ok(())
    }

    pub(super) fn apply_mode(path: &Path, mode: u32) -> Result<(), AclError> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            let reason = format!(
                "private permissions could not be applied: {}: set_permissions failed ({})",
                sanitized_path(path),
                native_error_code(&error)
            );
            if vanished(&error) {
                AclError::Missing(reason)
            } else {
                AclError::Refused(reason)
            }
        })
    }

    pub(super) fn ensure_entry_private(
        path: &Path,
        expected_dir: Option<bool>,
        policy: AclPolicy,
    ) -> Result<RestrictOutcome, AclError> {
        let _ = policy;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if expected_dir == Some(true) {
            flags |= libc::O_DIRECTORY;
        }
        options.custom_flags(flags);
        let file = options.open(path).map_err(|error| {
            let reason = format!(
                "private path {}: open failed ({})",
                sanitized_path(path),
                native_error_code(&error)
            );
            if vanished(&error) {
                AclError::Missing(reason)
            } else {
                AclError::Refused(reason)
            }
        })?;
        let meta = file.metadata().map_err(|error| {
            let reason = format!(
                "private path {}: metadata failed ({})",
                sanitized_path(path),
                native_error_code(&error)
            );
            if vanished(&error) {
                AclError::Missing(reason)
            } else {
                AclError::Refused(reason)
            }
        })?;
        if is_link(&meta)
            || !(meta.is_file() || meta.is_dir())
            || expected_dir.is_some_and(|dir| dir != meta.is_dir())
        {
            return Err(AclError::Refused(format!(
                "private path {}: unexpected entry type",
                sanitized_path(path)
            )));
        }
        if meta.nlink() == 0 {
            return Err(AclError::Missing(format!(
                "private path {}: entry was unlinked",
                sanitized_path(path)
            )));
        }
        check_ownership(path, &meta)?;
        let current_mode = meta.permissions().mode() & 0o7777;
        let target_mode = if meta.is_dir() {
            0o700
        } else {
            (current_mode & 0o700) | 0o600
        };
        if current_mode == target_mode {
            return Ok(RestrictOutcome::AlreadyPrivate);
        }
        file.set_permissions(fs::Permissions::from_mode(target_mode))
            .map_err(|error| {
                let reason = format!(
                    "private permissions could not be applied: {}: set_permissions failed ({})",
                    sanitized_path(path),
                    native_error_code(&error)
                );
                if vanished(&error) {
                    AclError::Missing(reason)
                } else {
                    AclError::Refused(reason)
                }
            })?;
        Ok(RestrictOutcome::Hardened)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{
        GetLastError, LocalFree, ERROR_DELETE_PENDING, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W,
        NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl,
        InitializeSecurityDescriptor, IsValidAcl, IsValidSid, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW,
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
        WRITE_DAC,
    };

    struct LocalDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for LocalDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: allocated with LocalAlloc by GetSecurityInfo.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn windows_acl_error(path: &Path, step: &str, code: u32) -> String {
        format!(
            "private ACL {}: {step} failed (Win32 error {code})",
            sanitized_path(path)
        )
    }

    fn windows_acl_err(path: &Path, step: &str, code: u32) -> AclError {
        let reason = windows_acl_error(path, step, code);
        if win32_vanished(code) || code == ERROR_DELETE_PENDING {
            AclError::Missing(reason)
        } else {
            AclError::Refused(reason)
        }
    }

    fn policy_sids(path: &Path, policy: AclPolicy) -> Result<Vec<Vec<u8>>, AclError> {
        let user = crate::process_identity::current_user().map_err(|error| {
            AclError::Refused(format!(
                "private ACL {}: process token SID: {error}",
                sanitized_path(path)
            ))
        })?;
        let mut sids = vec![user];
        if policy == AclPolicy::OwnerSystemAdmins {
            for kind in [WinLocalSystemSid, WinBuiltinAdministratorsSid] {
                let mut buffer = [0u32; 17];
                let mut size = std::mem::size_of_val(&buffer) as u32;
                // SAFETY: aligned writable SECURITY_MAX_SID_SIZE storage and a valid size pointer.
                if unsafe {
                    CreateWellKnownSid(
                        kind,
                        std::ptr::null_mut(),
                        buffer.as_mut_ptr().cast(),
                        &mut size,
                    )
                } == 0
                {
                    let code = io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or_default() as u32;
                    return Err(AclError::Refused(windows_acl_error(
                        path,
                        "CreateWellKnownSid",
                        code,
                    )));
                }
                // SAFETY: CreateWellKnownSid initialized `size` bytes inside `buffer`.
                let sid =
                    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size as usize) };
                if !sids.iter().any(|known| known.as_slice() == sid) {
                    sids.push(sid.to_vec());
                }
            }
        }
        Ok(sids)
    }

    fn with_policy_acl<T>(
        path: &Path,
        directory: bool,
        policy: AclPolicy,
        use_acl: impl FnOnce(*mut ACL, &[Vec<u8>]) -> T,
    ) -> Result<T, AclError> {
        let mut sids = policy_sids(path, policy)?;
        let inheritance = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        let entries: Vec<_> = sids
            .iter_mut()
            .map(|sid| EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: inheritance,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: sid.as_mut_ptr().cast(),
                },
            })
            .collect();
        let mut acl = std::ptr::null_mut();
        // SAFETY: SID vectors and EXPLICIT_ACCESS_W entries remain live; null old ACL creates only these grants.
        let result = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                std::ptr::null(),
                &mut acl,
            )
        };
        if result != 0 {
            return Err(AclError::Refused(windows_acl_error(
                path,
                "SetEntriesInAclW",
                result,
            )));
        }
        let value = use_acl(acl, &sids);
        // SAFETY: SetEntriesInAclW allocated `acl` with LocalAlloc.
        unsafe { LocalFree(acl.cast()) };
        Ok(value)
    }

    fn open_handle_no_follow(path: &Path, access: u32) -> Result<(File, bool), AclError> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated; flags open the node itself (including directories) without following reparse points.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: GetLastError reads the calling thread's last Win32 error immediately after CreateFileW.
            let code = unsafe { GetLastError() };
            return Err(windows_acl_err(path, "CreateFileW", code));
        }
        // SAFETY: CreateFileW returned a valid owned handle, transferred to File once.
        let file = unsafe { File::from_raw_handle(handle) };
        // SAFETY: plain C struct zero-initialized before GetFileInformationByHandle.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid open handle and writable info pointer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            // SAFETY: immediate error capture.
            let code = unsafe { GetLastError() };
            return Err(windows_acl_err(path, "GetFileInformationByHandle", code));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(AclError::Refused(format!(
                "private ACL {}: reparse points are refused",
                sanitized_path(path)
            )));
        }
        if info.nNumberOfLinks == 0 {
            return Err(AclError::Missing(format!(
                "private ACL {}: entry was unlinked",
                sanitized_path(path)
            )));
        }
        let is_dir = (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
        Ok((file, is_dir))
    }

    pub(super) fn create_protected_file(path: &Path, policy: AclPolicy) -> io::Result<File> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        with_policy_acl(path, false, policy, |acl, _| {
            let mut descriptor = std::mem::MaybeUninit::<SECURITY_DESCRIPTOR>::uninit();
            let ptr = descriptor.as_mut_ptr().cast();
            // SAFETY: aligned descriptor storage; `acl` outlives CreateFileW.
            if unsafe { InitializeSecurityDescriptor(ptr, 1) } == 0
                || unsafe { SetSecurityDescriptorDacl(ptr, 1, acl, 0) } == 0
                || unsafe { SetSecurityDescriptorControl(ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                    == 0
            {
                return Err(io::Error::last_os_error());
            }
            let attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: ptr,
                bInheritHandle: 0,
            };
            // SAFETY: NUL-terminated path and valid security descriptor live through CreateFileW.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_WRITE,
                    FILE_SHARE_READ,
                    &attrs,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateFileW returned a valid owned handle, transferred to File once.
            Ok(unsafe { File::from_raw_handle(handle) })
        })
        .map_err(io::Error::from)?
    }

    pub(super) fn ensure_entry_private(
        path: &Path,
        expected_dir: Option<bool>,
        policy: AclPolicy,
    ) -> Result<RestrictOutcome, AclError> {
        let (file, is_dir) = open_handle_no_follow(path, READ_CONTROL | WRITE_DAC)?;
        if expected_dir.is_some_and(|dir| dir != is_dir) {
            return Err(AclError::Refused(format!(
                "private ACL {}: unexpected entry type",
                sanitized_path(path)
            )));
        }
        let sids = policy_sids(path, policy)?;
        let expected: Vec<&[u8]> = sids.iter().map(Vec::as_slice).collect();
        match check_handle_exact_grants(&file, path, is_dir, &expected)? {
            VerifyGrants::Match => Ok(RestrictOutcome::AlreadyPrivate),
            VerifyGrants::Mismatch(_) => {
                restrict_handle(&file, path, is_dir, policy)?;
                Ok(RestrictOutcome::Hardened)
            }
        }
    }

    pub(super) fn restrict_path(
        path: &Path,
        expected_dir: Option<bool>,
        policy: AclPolicy,
    ) -> Result<(), AclError> {
        let (file, is_dir) = open_handle_no_follow(path, WRITE_DAC | READ_CONTROL)?;
        if expected_dir.is_some_and(|dir| dir != is_dir) {
            return Err(AclError::Refused(format!(
                "private ACL {}: unexpected entry type",
                sanitized_path(path)
            )));
        }
        restrict_handle(&file, path, is_dir, policy)
    }

    pub(super) fn restrict_handle(
        file: &File,
        path: &Path,
        directory: bool,
        policy: AclPolicy,
    ) -> Result<(), AclError> {
        with_policy_acl(path, directory, policy, |acl, sids| {
            // SAFETY: open file handle and valid ACL; PROTECTED disables parent inheritance.
            let result = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    acl,
                    std::ptr::null(),
                )
            };
            if result != 0 {
                return Err(windows_acl_err(path, "SetSecurityInfo", result));
            }
            let expected: Vec<&[u8]> = sids.iter().map(Vec::as_slice).collect();
            verify_handle_exact_grants(file, path, directory, &expected)
        })?
    }

    #[cfg(test)]
    pub(super) fn verify_path_exact_acl(
        path: &Path,
        directory: bool,
        policy: AclPolicy,
    ) -> Result<(), AclError> {
        let (file, is_dir) = open_handle_no_follow(path, READ_CONTROL)?;
        if is_dir != directory {
            return Err(AclError::Refused(format!(
                "private ACL {}: verify failed: unexpected entry type",
                sanitized_path(path)
            )));
        }
        let sids = policy_sids(path, policy)?;
        let expected: Vec<&[u8]> = sids.iter().map(Vec::as_slice).collect();
        verify_handle_exact_grants(&file, path, directory, &expected)
    }

    enum VerifyGrants {
        Match,
        Mismatch(String),
    }

    fn check_handle_exact_grants(
        file: &File,
        path: &Path,
        directory: bool,
        sids: &[&[u8]],
    ) -> Result<VerifyGrants, AclError> {
        let mut descriptor = LocalDescriptor(std::ptr::null_mut());
        let mut acl = std::ptr::null_mut();
        // SAFETY: open handle and initialized output pointers; `descriptor` owns `acl`.
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor.0,
            )
        };
        if result != 0 {
            return Err(windows_acl_err(path, "GetSecurityInfo (verify)", result));
        }
        let mismatch = |reason: &str| {
            Ok(VerifyGrants::Mismatch(format!(
                "private ACL {}: verify failed: {reason}",
                sanitized_path(path)
            )))
        };
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: `descriptor.0` is valid from GetSecurityInfo.
        if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
            // SAFETY: immediate Win32 error lookup.
            let code = unsafe { GetLastError() };
            return Err(windows_acl_err(
                path,
                "GetSecurityDescriptorControl (verify)",
                code,
            ));
        }
        if control & SE_DACL_PROTECTED == 0 {
            return mismatch("DACL is not protected; parent inheritance is enabled");
        }
        // SAFETY: non-null `acl` belongs to retained `descriptor.0`.
        if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
            return mismatch("DACL is missing or invalid");
        }
        // SAFETY: IsValidAcl validated header and ACE count.
        if usize::from(unsafe { (*acl).AceCount }) != sids.len() {
            return mismatch("DACL contains an unexpected number of grants");
        }
        let expected_flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let mut remaining = sids.to_vec();
        for index in 0..sids.len() {
            let mut ace = std::ptr::null_mut();
            // SAFETY: valid ACL, in-range index, initialized output pointer.
            if unsafe { GetAce(acl, index as u32, &mut ace) } == 0 {
                // SAFETY: immediate Win32 error lookup.
                let code = unsafe { GetLastError() };
                return Err(windows_acl_err(path, "GetAce (verify)", code));
            }
            // SAFETY: GetAce returned a validated ACE header inside `acl`.
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERITED_ACE != 0
                || header.AceType != 0
                || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            {
                return mismatch("DACL contains an unexpected ACE type or flags");
            }
            let sid_bytes = usize::from(header.AceSize) - sid_offset;
            // SAFETY: ACCESS_ALLOWED_ACE fixed fields and SID bytes fit inside `header.AceSize`.
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = unsafe {
                std::slice::from_raw_parts(ace.cast::<u8>().add(sid_offset), sid_bytes)
            };
            let Some(found) = remaining.iter().position(|expected| *expected == sid) else {
                return mismatch("DACL contains a repeated or unexpected SID");
            };
            remaining.swap_remove(found);
            if allowed.Mask != FILE_ALL_ACCESS || u32::from(header.AceFlags) != expected_flags {
                return mismatch("ACE has incorrect access or inheritance flags");
            }
        }
        Ok(VerifyGrants::Match)
    }

    fn verify_handle_exact_grants(
        file: &File,
        path: &Path,
        directory: bool,
        sids: &[&[u8]],
    ) -> Result<(), AclError> {
        match check_handle_exact_grants(file, path, directory, sids)? {
            VerifyGrants::Match => Ok(()),
            VerifyGrants::Mismatch(reason) => Err(AclError::Refused(reason)),
        }
    }

    fn check_owner_and_acl_trustees(
        owner: *mut std::ffi::c_void,
        acl: *mut ACL,
        sids: &[Vec<u8>],
        reject_inherited: bool,
        required_grant_sid: Option<&[u8]>,
    ) -> Result<(), AclError> {
        // SAFETY: `sid` points into either `descriptor` or local `sids` vectors kept alive here.
        let sid_allowed = |sid: *mut std::ffi::c_void| unsafe {
            !sid.is_null()
                && IsValidSid(sid) != 0
                && sids
                    .iter()
                    .any(|trusted| EqualSid(sid, trusted.as_ptr().cast_mut().cast()) != 0)
        };
        // SAFETY: `acl` belongs to retained `descriptor.0`.
        if !sid_allowed(owner) || acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
            return Err(AclError::Refused(
                "file owner or DACL is untrusted".into(),
            ));
        }
        let mut saw_required = required_grant_sid.is_none();
        // SAFETY: IsValidAcl validated header and ACE bounds.
        let count = unsafe { (*acl).AceCount } as u32;
        for index in 0..count {
            let mut ace = std::ptr::null_mut();
            // SAFETY: valid ACL and in-range index; `descriptor.0` remains allocated.
            unsafe {
                if GetAce(acl, index, &mut ace) == 0 || ace.is_null() {
                    return Err(AclError::Refused("GetAce failed".into()));
                }
                let header = &*ace.cast::<ACE_HEADER>();
                if reject_inherited && (u32::from(header.AceFlags) & INHERITED_ACE) != 0 {
                    return Err(AclError::Refused("DACL contains an inherited ACE".into()));
                }
                match header.AceType {
                    0 => {
                        if usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>() {
                            return Err(AclError::Refused("malformed ACCESS_ALLOWED_ACE".into()));
                        }
                        let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                        let sid_ptr = std::ptr::addr_of!(entry.SidStart).cast_mut().cast();
                        if entry.Mask != 0 {
                            if !sid_allowed(sid_ptr) {
                                return Err(AclError::Refused(
                                    "DACL grants access to an untrusted SID".into(),
                                ));
                            }
                            if let Some(required) = required_grant_sid {
                                if EqualSid(sid_ptr, required.as_ptr().cast_mut().cast()) != 0 {
                                    saw_required = true;
                                }
                            }
                        }
                    }
                    1 => {} // ACCESS_DENIED_ACE_TYPE cannot broaden access.
                    _ => return Err(AclError::Refused("unexpected ACE type in DACL".into())),
                }
            }
        }
        if !saw_required {
            return Err(AclError::Refused(
                "DACL does not grant access to the required trustee".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn verify_handle_trustees(file: &File, policy: AclPolicy) -> Result<(), AclError> {
        let sids = policy_sids(Path::new("handle"), policy)?;
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut descriptor = LocalDescriptor(std::ptr::null_mut());
        // SAFETY: owned handle and initialized out pointers.
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor.0,
            )
        };
        if result != 0 {
            return Err(AclError::Refused(format!(
                "GetSecurityInfo failed (Win32 error {result})"
            )));
        }
        check_owner_and_acl_trustees(owner, acl, &sids, false, None)
    }

    pub(super) fn is_directory_hardened(dir: &Path, policy: AclPolicy) -> bool {
        let Ok((file, true)) = open_handle_no_follow(dir, READ_CONTROL) else {
            return false;
        };
        let Ok(sids) = policy_sids(dir, policy) else {
            return false;
        };
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut descriptor = LocalDescriptor(std::ptr::null_mut());
        // SAFETY: open handle and initialized output pointers.
        if unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor.0,
            )
        } != 0
        {
            return false;
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: valid descriptor from GetSecurityInfo.
        if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0
            || (control & SE_DACL_PROTECTED) == 0
        {
            return false;
        }
        check_owner_and_acl_trustees(owner, acl, &sids, true, Some(&sids[0])).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-guarded-fs-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sanitized_path_replaces_control_and_bidi_characters() {
        let p = PathBuf::from("/tmp/a\nb\u{202e}c\u{061c}d");
        assert_eq!(sanitized_path(&p), "/tmp/a?b?c?d");
    }

    #[test]
    fn stage_protected_file_and_read_verified_round_trip() {
        let dir = TempDir::new("stage-read");
        let target = dir.0.join("secret.txt");
        let staged = stage_protected_file(&target, &mut &b"top-secret"[..], AclPolicy::OwnerOnly).unwrap();
        staged.commit(&mut || Ok(())).unwrap();
        assert!(verify_file_private(&target, AclPolicy::OwnerOnly));
        assert_eq!(read_verified_private_file(&target, AclPolicy::OwnerOnly).unwrap(), b"top-secret");

        let settings = dir.0.join("settings.db");
        let staged = stage_protected_file(&settings, &mut &b"db-bytes"[..], AclPolicy::OwnerSystemAdmins).unwrap();
        staged.commit(&mut || Ok(())).unwrap();
        assert!(verify_file_private(&settings, AclPolicy::OwnerSystemAdmins));
        assert_eq!(
            read_verified_private_file(&settings, AclPolicy::OwnerSystemAdmins).unwrap(),
            b"db-bytes"
        );
    }

    #[test]
    fn restrict_file_refuses_missing_and_directory_targets() {
        let dir = TempDir::new("restrict-file");
        let missing = dir.0.join("missing.db");
        assert!(matches!(restrict_file(&missing, AclPolicy::OwnerSystemAdmins), Err(AclError::Missing(_))));
        let sub = dir.0.join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(matches!(restrict_file(&sub, AclPolicy::OwnerSystemAdmins), Err(AclError::Refused(_))));
        restrict_path(&sub, AclPolicy::OwnerOnly).unwrap();
    }

    #[test]
    fn harden_directory_tree_makes_directory_hardened_and_readable() {
        let dir = TempDir::new("harden-tree");
        let nested = dir.0.join("sub");
        fs::create_dir(&nested).unwrap();
        let file = nested.join("state.json");
        fs::write(&file, b"{}").unwrap();

        harden_directory_tree(&dir.0, AclPolicy::OwnerSystemAdmins).unwrap();
        assert!(is_directory_hardened(&dir.0, AclPolicy::OwnerSystemAdmins));
        assert!(is_directory_hardened(&nested, AclPolicy::OwnerSystemAdmins));
        assert_eq!(fs::read(&file).unwrap(), b"{}");
        assert!(!manual_harden_command(&dir.0).is_empty());
    }

    #[test]
    fn protected_file_open_read_recheck_and_remove_work() {
        let dir = TempDir::new("protected-file");
        let path = dir.0.join("token");
        assert!(open_protected_file(&path).unwrap().is_none());
        let mut file = create_protected_file(&path, AclPolicy::OwnerOnly).unwrap();
        io::Write::write_all(&mut file, b"abc").unwrap();
        drop(file);

        assert!(open_protected_file(&path).unwrap().is_some());
        let bytes = read_protected_file_with_recheck(&path, 16, || Ok(path.clone())).unwrap();
        assert_eq!(bytes, Some(b"abc".to_vec()));
        assert!(read_protected_file_with_recheck(&path, 2, || Ok(path.clone())).is_err());
        assert!(read_protected_file_with_recheck(&path, 16, || Ok(dir.0.join("other"))).is_err());

        remove_protected_file_with(&path, || fs::remove_file(&path).map_err(|e| e.to_string())).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn restrict_handle_applies_and_verifies_policy_on_open_handle() {
        let dir = TempDir::new("restrict-handle");
        let path = dir.0.join("secret.bin");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
            use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
            options.access_mode(GENERIC_READ | GENERIC_WRITE | WRITE_DAC | READ_CONTROL);
        }
        let file = options.open(&path).unwrap();
        restrict_handle(&file, &path, false, AclPolicy::OwnerOnly).unwrap();
        assert!(verify_file_private(&path, AclPolicy::OwnerOnly));
        restrict_handle(&file, &path, false, AclPolicy::OwnerSystemAdmins).unwrap();
        assert!(verify_file_private(&path, AclPolicy::OwnerSystemAdmins));
    }

    #[cfg(unix)]
    #[test]
    fn unix_links_and_permissive_modes_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("unix-links");
        let real = dir.0.join("real");
        fs::write(&real, b"data").unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_protected_file(&link).is_err());
        assert!(read_verified_private_file(&link, AclPolicy::OwnerOnly).is_err());
        assert!(restrict_path(&link, AclPolicy::OwnerOnly).is_err());

        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!verify_file_private(&real, AclPolicy::OwnerOnly));
        assert!(read_verified_private_file(&real, AclPolicy::OwnerOnly).is_err());

        let hard = dir.0.join("hard");
        fs::hard_link(&real, &hard).unwrap();
        assert!(open_protected_file(&real).is_err());
        assert!(matches!(
            ensure_entry_private(&real, Some(false), AclPolicy::OwnerOnly),
            Err(AclError::Refused(_))
        ));
    }

    #[test]
    fn ensure_entry_private_skips_verified_entries_and_hardens_exposed_ones() {
        let dir = TempDir::new("ensure-private");
        let sub = dir.0.join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("secret.txt");
        fs::write(&file, b"synthetic").unwrap();

        assert_eq!(
            ensure_entry_private(&sub, Some(true), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_eq!(
            ensure_entry_private(&sub, Some(true), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::AlreadyPrivate)
        );
        assert_eq!(
            ensure_entry_private(&file, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_eq!(
            ensure_entry_private(&file, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::AlreadyPrivate)
        );
        assert!(verify_file_private(&file, AclPolicy::OwnerOnly));

        assert!(matches!(
            ensure_entry_private(&sub.join("missing"), Some(false), AclPolicy::OwnerOnly),
            Err(AclError::Missing(_))
        ));
        assert!(matches!(
            ensure_entry_private(&sub, Some(false), AclPolicy::OwnerOnly),
            Err(AclError::Refused(_))
        ));
        assert!(matches!(
            ensure_entry_private(&file, Some(true), AclPolicy::OwnerOnly),
            Err(AclError::Refused(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_entry_private_unix_preserves_exec_and_refuses_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("ensure-unix");
        let exec = dir.0.join("relay-bin");
        fs::write(&exec, b"bin").unwrap();
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            ensure_entry_private(&exec, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_eq!(
            fs::symlink_metadata(&exec).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            ensure_entry_private(&exec, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::AlreadyPrivate)
        );

        fs::set_permissions(&exec, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            ensure_entry_private(&exec, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );

        let outside = TempDir::new("ensure-unix-out");
        let target = outside.0.join("target");
        fs::write(&target, b"outside").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            ensure_entry_private(&link, Some(false), AclPolicy::OwnerOnly),
            Err(AclError::Refused(_))
        ));
        assert_eq!(
            fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}

#[cfg(all(test, windows))]
mod native_tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetAce, GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-guarded-fs-win-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated with LocalAlloc by Win32 security APIs.
            unsafe { LocalFree(self.0) };
        }
    }

    fn apply_sddl(path: &Path, sddl_str: &str, protected: bool) {
        let sddl: Vec<u16> = sddl_str.encode_utf16().chain(Some(0)).collect();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = Descriptor(std::ptr::null_mut());
        // SAFETY: NUL-terminated strings and valid out-pointers.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor.0,
                    std::ptr::null_mut(),
                ),
                0
            );
            let mut acl = std::ptr::null_mut();
            let mut present = 0;
            let mut defaulted = 0;
            assert_ne!(
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted),
                0
            );
            assert_ne!(present, 0);
            let info = if protected {
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                DACL_SECURITY_INFORMATION
            };
            assert_eq!(
                SetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    info,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    acl,
                    std::ptr::null(),
                ),
                0
            );
        }
    }

    fn assert_dacl_owner_only(path: &Path, protected: bool, flags: u8) {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = Descriptor(std::ptr::null_mut());
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: terminated path and valid output pointers; descriptor retains the ACL/SID.
        unsafe {
            assert_eq!(
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut acl,
                    std::ptr::null_mut(),
                    &mut descriptor.0,
                ),
                0
            );
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision),
                0
            );
            assert_eq!(control & SE_DACL_PROTECTED != 0, protected);
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            assert_ne!(GetAce(acl, 0, &mut ace), 0);
            let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(allowed.Header.AceType, 0);
            assert_eq!(allowed.Header.AceFlags, flags);
            assert_eq!(allowed.Mask, 0x001f01ff);
            let sid = std::ptr::addr_of!(allowed.SidStart).cast::<u8>();
            let length = GetLengthSid(sid.cast_mut().cast()) as usize;
            assert_eq!(
                std::slice::from_raw_parts(sid, length),
                crate::process_identity::current_user().unwrap()
            );
        }
    }

    #[test]
    fn private_credential_creation_allows_read_sharing() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let fixture = Fixture::new("share");
        let path = fixture.0.join("capture.out");
        let _writer = create_protected_file(&path, AclPolicy::OwnerOnly).unwrap();
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path);
        assert!(reader.is_ok(), "credential creation refused a concurrent reader");
        assert_dacl_owner_only(&path, true, 0);
    }

    #[test]
    fn acl_failure_identifies_file_step_and_win32_code() {
        let fixture = Fixture::new("errors");
        for (missing, code) in [
            (fixture.0.join("missing.key"), "Win32 error 2"),
            (fixture.0.join("gone").join("missing.key"), "Win32 error 3"),
        ] {
            let Err(AclError::Missing(error)) = restrict_path(&missing, AclPolicy::OwnerOnly) else {
                panic!("a missing entry must be reported as missing");
            };
            assert!(error.contains("CreateFileW"), "{error}");
            assert!(error.contains(code), "{error}");
            assert!(error.contains(&missing.display().to_string()), "{error}");
        }
        let invalid = fixture.0.join("invalid<name.key");
        let Err(AclError::Refused(error)) = restrict_path(&invalid, AclPolicy::OwnerOnly) else {
            panic!("an invalid name must be refused");
        };
        assert!(error.contains("CreateFileW"), "{error}");
        assert!(error.contains("Win32 error"), "{error}");
        assert!(error.contains(&invalid.display().to_string()), "{error}");
    }

    #[test]
    fn only_file_and_path_not_found_count_as_vanished() {
        assert!(win32_vanished(2));
        assert!(win32_vanished(3));
        for code in [0, 5, 32, 123] {
            assert!(!win32_vanished(code), "Win32 error {code}");
        }
        assert!(vanished(&io::Error::from_raw_os_error(2)));
        assert!(vanished(&io::Error::from_raw_os_error(3)));
        assert!(!vanished(&io::Error::from_raw_os_error(5)));
    }

    #[test]
    fn verification_and_read_refuse_protected_foreign_sid_and_permissive_dir() {
        let fixture = Fixture::new("foreign");
        restrict_path(&fixture.0, AclPolicy::OwnerOnly).unwrap();
        let file = fixture.0.join("synthetic.key");
        fs::write(&file, b"synthetic fixture").unwrap();
        apply_sddl(&file, "D:P(A;;FA;;;WD)", true);
        let Err(AclError::Refused(error)) =
            verify_path_exact_acl(&file, false, AclPolicy::OwnerOnly)
        else {
            panic!("a foreign SID grant must be refused");
        };
        assert!(error.contains("SID"), "{error}");
        assert!(error.contains("verify failed"), "{error}");
        assert!(read_verified_private_file(&file, AclPolicy::OwnerSystemAdmins).is_err());

        harden_directory_tree(&fixture.0, AclPolicy::OwnerSystemAdmins).unwrap();
        assert!(is_directory_hardened(&fixture.0, AclPolicy::OwnerSystemAdmins));
        assert_eq!(
            read_verified_private_file(&file, AclPolicy::OwnerSystemAdmins).unwrap(),
            b"synthetic fixture"
        );

        // Adding Everyone (WD) makes directory unhardened until rehardened.
        apply_sddl(&fixture.0, "D:P(A;;FA;;;WD)", true);
        assert!(!is_directory_hardened(&fixture.0, AclPolicy::OwnerSystemAdmins));
        harden_directory_tree(&fixture.0, AclPolicy::OwnerSystemAdmins).unwrap();
        assert!(is_directory_hardened(&fixture.0, AclPolicy::OwnerSystemAdmins));
    }

    #[test]
    fn reparse_point_junction_is_refused_by_restrict_and_harden() {
        let fixture = Fixture::new("junction");
        let outside = Fixture::new("outside");
        let secret = outside.0.join("secret.txt");
        fs::write(&secret, b"outside").unwrap();
        // Control: hardening the plain outside directory succeeds.
        harden_directory_tree(&outside.0, AclPolicy::OwnerSystemAdmins).unwrap();
        assert!(is_directory_hardened(&outside.0, AclPolicy::OwnerSystemAdmins));

        let junction = fixture.0.join("junc");
        let output = crate::subprocess::command_without_console("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(restrict_path(&junction, AclPolicy::OwnerOnly).is_err());
        assert!(ensure_entry_private(&junction, Some(true), AclPolicy::OwnerOnly).is_err());
        assert!(!is_directory_hardened(&junction, AclPolicy::OwnerSystemAdmins));
        assert!(harden_directory_tree(&junction, AclPolicy::OwnerSystemAdmins).is_err());
    }

    #[test]
    fn ensure_entry_private_skips_exact_dacl_and_handles_open_writer() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let fixture = Fixture::new("ensure-win");
        assert_eq!(
            ensure_entry_private(&fixture.0, Some(true), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_eq!(
            ensure_entry_private(&fixture.0, Some(true), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::AlreadyPrivate)
        );

        let part = fixture.0.join("request-log-parts-request-body-1");
        let writer = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .open(&part)
            .unwrap();
        // Inherited ACE only (`!SE_DACL_PROTECTED`), so handle-based check must harden it once,
        // even while an active writer holds the file open without FILE_SHARE_WRITE/DELETE.
        assert_dacl_owner_only(&part, false, 16);
        assert_eq!(
            ensure_entry_private(&part, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_dacl_owner_only(&part, true, 0);
        assert_eq!(
            ensure_entry_private(&part, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::AlreadyPrivate)
        );
        drop(writer);

        // Foreign SID triggers mismatch and re-hardening on the handle.
        apply_sddl(&part, "D:P(A;;FA;;;WD)", true);
        assert_eq!(
            ensure_entry_private(&part, Some(false), AclPolicy::OwnerOnly),
            Ok(RestrictOutcome::Hardened)
        );
        assert_dacl_owner_only(&part, true, 0);
    }
}
