//! Private relay configuration and authenticated, non-model readiness.
//! Process ownership, recovery and platform dispatch belong to supervisor_dependencies.
use super::*;
use std::io::{Read, Write};

pub(super) struct Settings {
    pub arguments: Vec<String>,
    pub folder: PathBuf,
    key_file: PathBuf,
}

// Fixed diagnostics only: never propagate upstream text, arguments, login paths or keys.
pub(super) fn configuration(
    context: &RuntimeContext,
    cfg: &Value,
    endpoint: SocketAddr,
) -> Result<Dependency, String> {
    let value = &cfg["dependencies"]["relay"];
    let version = pinned_version(cfg, "relay")?.ok_or("relay requires an exact pinned version")?;
    let folder = dependency_directory(context, cfg)?
        .join("relay")
        .join(version);
    regular_tree_path(&folder)?;
    let folder =
        std::fs::canonicalize(folder).map_err(|_| "relay version folder is unavailable")?;
    let path = |field: &str, directory: bool| -> Result<PathBuf, String> {
        let path = absolute(value[field].as_str())
            .map_err(|_| "relay requires absolute configured paths")?;
        regular_tree_path(&path)?;
        let path =
            std::fs::canonicalize(path).map_err(|_| "relay configured path is unavailable")?;
        if !path.starts_with(&folder) || (!directory && path == folder) {
            return Err("relay files must stay inside its pinned dependency folder".into());
        }
        let meta = std::fs::metadata(&path).map_err(|_| "relay configured path is unavailable")?;
        if if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        } {
            return Err("relay configured path has the wrong type".into());
        }
        Ok(path)
    };
    let program = path("program", false)?;
    let cwd = path("workingDirectory", true)?;
    path("loginDirectory", true)?;
    let key_file = path("clientKeyFile", false)?;
    let arguments = value["arguments"]
        .as_array()
        .ok_or("relay arguments must be an array of strings")?
        .iter()
        .map(|arg| {
            arg.as_str()
                .filter(|arg| !arg.chars().any(char::is_control))
                .map(str::to_string)
                .ok_or("relay arguments must be strings without control characters")
        })
        .collect::<Result<Vec<_>, _>>()?;
    for argument in &arguments {
        let operand = if argument.starts_with('-') {
            argument
                .split_once('=')
                .map_or(argument.as_str(), |(_, value)| value)
        } else {
            argument.as_str()
        };
        // URLs and scalar switches are not filesystem operands. File operands, including
        // --config=<path>, must not reuse an interim config or login outside the pin.
        if operand.contains("://") {
            continue;
        }
        let path = Path::new(operand);
        if path.components().any(|part| part == Component::ParentDir) {
            return Err("relay arguments must not traverse parent directories".into());
        }
        if path.is_absolute() || path.components().count() > 1 || cwd.join(path).exists() {
            let candidate = cwd.join(path);
            let mut existing = candidate.as_path();
            loop {
                match std::fs::symlink_metadata(existing) {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        existing = existing
                            .parent()
                            .ok_or("relay argument path is unavailable")?;
                    }
                    Err(_) => return Err("relay argument path is unavailable".into()),
                }
            }
            regular_tree_path(existing)?;
            if !std::fs::canonicalize(existing)
                .map_err(|_| "relay argument path is unavailable")?
                .starts_with(&folder)
            {
                return Err(
                    "relay argument files must stay inside its pinned dependency folder".into(),
                );
            }
        }
    }
    Ok(Dependency {
        name: "relay",
        program,
        script: None,
        cwd,
        endpoint,
        install: None,
        relay: Some(Settings {
            arguments,
            folder,
            key_file,
        }),
    })
}

fn regular_tree_path(path: &Path) -> Result<(), String> {
    crate::install_root::check_runtime(path, false).map_err(|_| "relay path is protected")?;
    for ancestor in path.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor).map_err(|_| "relay path is unavailable")?;
        if crate::guarded_file::is_link(&meta) {
            return Err("relay paths must not be redirected".into());
        }
    }
    Ok(())
}

impl Settings {
    fn key(&self) -> Result<String, String> {
        regular_tree_path(&self.key_file)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&self.key_file)
            .and_then(|file| file.take(4097).read_to_end(&mut bytes))
            .map_err(|_| "relay client key is unavailable")?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "relay client key is invalid")?
            .trim();
        if bytes.len() > 4096
            || text.is_empty()
            || !text.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Err("relay client key is invalid".into());
        }
        Ok(text.to_string())
    }

    pub(super) fn check_arguments(&self) -> Result<(), String> {
        let key = self.key()?;
        if self.arguments.iter().any(|arg| arg.contains(&key)) {
            return Err("relay arguments must not contain the client key".into());
        }
        Ok(())
    }

    pub(super) fn probe(&self, endpoint: SocketAddr, deadline: Instant) -> Result<(), String> {
        let key = self.key()?;
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or("relay health request timed out")
        };
        let mut stream = std::net::TcpStream::connect_timeout(&endpoint, remaining()?)
            .map_err(|_| "relay health connection failed")?;
        stream
            .set_write_timeout(Some(remaining()?))
            .map_err(|_| "relay health timeout is unavailable")?;
        write!(stream, "GET /v1/models HTTP/1.1\r\nHost: {endpoint}\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n")
            .map_err(|_| "relay health request failed")?;
        read_probe_status(|buffer| loop {
            stream
                .set_read_timeout(Some(remaining()?))
                .map_err(|_| "relay health timeout is unavailable")?;
            match stream.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err("relay health response failed or timed out"),
                Ok(count) => {
                    remaining()?;
                    return Ok(count);
                }
            }
        })
        .map_err(str::to_string)
    }
}

/// Only inspect the bounded status line, even when a read includes headers or body bytes.
fn read_probe_status(
    mut read_chunk: impl FnMut(&mut [u8]) -> Result<usize, &'static str>,
) -> Result<(), &'static str> {
    let mut line = [0; 512];
    let mut used = 0;
    while used < line.len() {
        let count = read_chunk(&mut line[used..])?;
        if count == 0 {
            return Err("relay health response failed or timed out");
        }
        let end = used + count;
        if let Some(newline) = line[used..end].iter().position(|&byte| byte == b'\n') {
            let status = std::str::from_utf8(&line[..used + newline + 1])
                .ok()
                .and_then(|line| {
                    let mut fields = line.split_whitespace();
                    matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1"))
                        .then(|| fields.next())
                        .flatten()
                });
            return if status == Some("200") {
                Ok(())
            } else {
                Err("relay models health request was refused")
            };
        }
        used = end;
    }
    Err("relay health response is invalid")
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn relay_reads_a_buffered_status_line_at_the_byte_limit() {
        let mut status = String::from("HTTP/1.1 200 ");
        status.extend(std::iter::repeat_n('x', 511 - status.len()));
        status.push('\n');
        let mut response = std::io::Cursor::new(status.as_bytes());
        let mut reads = 0;
        assert_eq!(
            read_probe_status(|buffer| {
                reads += 1;
                Ok(response.read(buffer).unwrap())
            }),
            Ok(())
        );
        assert_eq!(reads, 1, "read a complete status line in one chunk");

        status.insert(511, 'x');
        let mut response = std::io::Cursor::new(status.as_bytes());
        assert_eq!(
            read_probe_status(|buffer| Ok(response.read(buffer).unwrap())),
            Err("relay health response is invalid")
        );
    }

    #[test]
    fn relay_assembles_split_status_and_ignores_bytes_after_the_line() {
        let mut chunks = [&b"HTTP/1.1 2"[..], &b"00 OK\r\n\xff"[..]].into_iter();
        assert_eq!(
            read_probe_status(|buffer| {
                let chunk = chunks
                    .next()
                    .expect("status line should already be complete");
                buffer[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }),
            Ok(())
        );
    }
}

/// Why one relay entry could not be made private.
#[derive(Debug)]
pub(super) enum Unprotected {
    /// The OS reported the entry missing (`vanished`); `protect` decides whether it really is
    /// gone. The diagnostic is kept for the refusal when it is not.
    Missing(String),
    /// Any other failure.
    Refused(String),
}

impl From<Unprotected> for String {
    fn from(failure: Unprotected) -> Self {
        match failure {
            Unprotected::Missing(reason) | Unprotected::Refused(reason) => reason,
        }
    }
}

/// Whether an I/O error reports the entry itself missing: Win32 ERROR_FILE_NOT_FOUND (2) or
/// ERROR_PATH_NOT_FOUND (3), Unix ENOENT. Every other error is a refusal.
fn vanished(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
            .is_some_and(win32_vanished)
    }
    #[cfg(not(windows))]
    {
        error.kind() == std::io::ErrorKind::NotFound
    }
}

/// Whether `path` itself is now reported missing.
fn gone(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| vanished(&error))
}

/// Characters that hide or reorder displayed text: control characters, the Unicode bidirectional
/// controls (Bidi_Control, which includes U+061C), line and paragraph separators, zero-width
/// characters and the byte-order mark.
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

/// A path as diagnostics show it. Such characters become `?`, so an entry name cannot add lines
/// to a log or notice or reorder its text.
fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    text.chars()
        .map(|c| if formatting(c) { '?' } else { c })
        .collect()
}

/// A sweep diagnostic naming the path that failed, as the ACL diagnostics do.
fn named(text: &str, failed: &Path) -> String {
    format!("{text}: {}", shown(failed))
}

/// The native code of an I/O error as diagnostics show it: a Win32 code on Windows, an errno
/// elsewhere, or the error kind when the OS gave no code.
fn native_code(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        #[cfg(windows)]
        Some(code) => format!("Win32 error {code}"),
        #[cfg(not(windows))]
        Some(code) => format!("errno {code}"),
        None => format!("{:?}", error.kind()),
    }
}

/// A sweep refusal naming the path, the failed step and the native code.
fn failure(text: &str, failed: &Path, step: &str, error: &std::io::Error) -> String {
    format!(
        "{text}: {}: {step} failed ({})",
        shown(failed),
        native_code(error)
    )
}

/// Checks that every ancestor of `path` can be read and is not a link. `Ok(false)` when a listed
/// entry is gone and the ancestor read failed because it was removed (`vanished`); any other
/// failure refuses, even when the entry is gone by then.
fn ancestors_checked(
    path: &Path,
    listed: bool,
    stat: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<bool, String> {
    for ancestor in path.ancestors().skip(1) {
        match stat(ancestor) {
            Ok(found) if crate::guarded_file::is_link(&found) => {
                return Err("relay paths must not be redirected".into());
            }
            Ok(_) => {}
            Err(error) if listed && vanished(&error) && gone(path) => return Ok(false),
            Err(error) => {
                return Err(failure(
                    "relay path is unavailable",
                    ancestor,
                    "symlink_metadata",
                    &error,
                ));
            }
        }
    }
    Ok(true)
}

/// Checks one entry before its permissions change: outside any protected root, no redirected
/// part, a regular file or directory, and on Unix owned by this account with one link.
/// `Ok(None)` when a listed entry vanished: the OS reports the entry itself missing, also after a
/// guard failed, an ancestor reported missing or the ownership check failed (Unix: the entry was
/// unlinked as its metadata was read) because the entry was removed meanwhile. Any other
/// read failure refuses. The version folder itself (`listed == false`) must exist.
fn inspect(path: &Path, listed: bool) -> Result<Option<std::fs::Metadata>, String> {
    inspect_with(path, listed, &|entry| std::fs::symlink_metadata(entry))
}

/// `inspect` reading metadata through `stat`, so a test can change the tree between the lookup of
/// an entry and the read of its metadata.
fn inspect_with(
    path: &Path,
    listed: bool,
    stat: &dyn Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<Option<std::fs::Metadata>, String> {
    if crate::install_root::check_runtime(path, false).is_err() {
        return if listed && gone(path) {
            Ok(None)
        } else {
            Err(named("relay path is protected", path))
        };
    }
    let meta = match stat(path) {
        Ok(meta) => meta,
        Err(error) if listed && vanished(&error) => return Ok(None),
        Err(error) => {
            return Err(failure(
                "relay path is unavailable",
                path,
                "symlink_metadata",
                &error,
            ));
        }
    };
    if crate::guarded_file::is_link(&meta) {
        return Err("relay paths must not be redirected".into());
    }
    if !ancestors_checked(path, listed, stat)? {
        return Ok(None);
    }
    if !meta.is_file() && !meta.is_dir() {
        return Err("relay private files must be regular files or directories".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || (meta.is_file() && meta.nlink() != 1) {
            // The lookup can find an entry that is unlinked before its attributes are read: the
            // metadata then comes from the unlinked inode with a link count of 0. That is a
            // vanished entry, not a second link or another owner.
            if listed && (meta.nlink() == 0 || gone(path)) {
                return Ok(None);
            }
            return Err("relay private files must belong only to the HER account".into());
        }
    }
    Ok(Some(meta))
}

/// Makes one entry private with `private`. `Ok(false)` when a listed entry vanished: the OS
/// reported it missing and a fresh inspection finds it gone. A missing report for an entry that
/// still exists (a child that vanished while a directory's ACL propagated) is retried once, with
/// the fresh inspection, so a replacement link is refused. Any other failure refuses, even for an
/// entry that is gone by then; the version folder (`listed == false`) is never skipped.
fn protect(
    path: &Path,
    meta: std::fs::Metadata,
    listed: bool,
    mut private: impl FnMut(&Path, &std::fs::Metadata) -> Result<(), Unprotected>,
) -> Result<bool, String> {
    let mut meta = meta;
    let mut retried = false;
    loop {
        let reason = match private(path, &meta) {
            Ok(()) => return Ok(true),
            Err(Unprotected::Refused(reason)) => return Err(reason),
            Err(Unprotected::Missing(reason)) => reason,
        };
        match inspect(path, listed)? {
            None => return Ok(false),
            Some(_) if retried => return Err(reason),
            Some(fresh) => {
                meta = fresh;
                retried = true;
            }
        }
    }
}

/// Restrict the entire version tree, so config and future login files are protected too.
/// Reject redirected entries before changing any permissions; never traverse a foreign tree.
/// Files the relay writes later (short-lived request-log parts included) are private from
/// creation: they inherit the version folder's HER-only ACE (Unix: they sit inside the 0700
/// folder). Returns how many listed entries vanished before they were protected; those are
/// skipped without a refusal, and every other failure still refuses.
pub(super) fn restrict(folder: &Path) -> Result<usize, String> {
    sweep(folder, &mut |_, _| {})
}

/// The points of a sweep where tests change the tree, in the order a sweep reaches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seam {
    /// A directory listed this child; it is not inspected yet.
    Listed,
    /// This directory was inspected; it is not listed yet.
    Inspected,
    /// The whole tree is listed (the path is the version folder); no permission changed yet.
    Hardening,
}

/// `restrict` with its test seams.
fn sweep(folder: &Path, seam: &mut dyn FnMut(Seam, &Path)) -> Result<usize, String> {
    fn collect(
        path: &Path,
        listed: bool,
        entries: &mut Vec<(PathBuf, std::fs::Metadata, bool)>,
        skipped: &mut usize,
        seam: &mut dyn FnMut(Seam, &Path),
    ) -> Result<(), String> {
        let Some(meta) = inspect(path, listed)? else {
            *skipped += 1;
            return Ok(());
        };
        if meta.is_dir() {
            seam(Seam::Inspected, path);
            let children = match std::fs::read_dir(path) {
                Ok(children) => children,
                Err(error) if listed && vanished(&error) => {
                    *skipped += 1;
                    return Ok(());
                }
                Err(error) => {
                    return Err(failure(
                        "relay private directory cannot be inspected",
                        path,
                        "read_dir",
                        &error,
                    ));
                }
            };
            for child in children {
                let child = match child {
                    Ok(child) => child.path(),
                    // The directory went away while it was listed; `protect` counts it below.
                    Err(error) if listed && vanished(&error) => break,
                    Err(error) => {
                        return Err(failure(
                            "relay private directory cannot be inspected",
                            path,
                            "read_dir entry",
                            &error,
                        ));
                    }
                };
                seam(Seam::Listed, &child);
                collect(&child, true, entries, skipped, seam)?;
            }
        }
        entries.push((path.to_path_buf(), meta, listed));
        Ok(())
    }
    let mut entries = Vec::new();
    let mut skipped = 0;
    collect(folder, false, &mut entries, &mut skipped, seam)?;
    seam(Seam::Hardening, folder);
    // Parent first closes external access before processing its children.
    for (path, meta, listed) in entries.into_iter().rev() {
        if !protect(&path, meta, listed, super::os::private)? {
            skipped += 1;
        }
    }
    Ok(skipped)
}

#[cfg(unix)]
pub(super) fn unix_private(path: &Path, meta: &std::fs::Metadata) -> Result<(), Unprotected> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if meta.is_dir() {
        0o700
    } else {
        (meta.permissions().mode() & 0o700) | 0o600
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        let reason = failure(
            "relay private permissions could not be applied",
            path,
            "set_permissions",
            &error,
        );
        if vanished(&error) {
            Unprotected::Missing(reason)
        } else {
            Unprotected::Refused(reason)
        }
    })
}

#[cfg(windows)]
fn credential_sids(path: &Path, settings: bool) -> Result<Vec<Vec<u8>>, Unprotected> {
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };

    let user = identity::current_user().map_err(|error| {
        Unprotected::Refused(format!(
            "relay private ACL {}: process token SID: {error}",
            shown(path)
        ))
    })?;
    let mut sids = vec![user];
    if settings {
        for kind in [WinLocalSystemSid, WinBuiltinAdministratorsSid] {
            let mut buffer = [0u32; 17];
            let mut size = std::mem::size_of_val(&buffer) as u32;
            // SAFETY: aligned writable SECURITY_MAX_SID_SIZE storage and a valid size pointer.
            if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size) } == 0 {
                return Err(Unprotected::Refused(windows_acl_error(
                    path, "CreateWellKnownSid", std::io::Error::last_os_error().raw_os_error().unwrap_or_default() as u32,
                )));
            }
            // SAFETY: the native call initialized this many bytes in the retained buffer.
            let sid = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size as usize) };
            if !sids.iter().any(|known| known.as_slice() == sid) {
                sids.push(sid.to_vec());
            }
        }
    }
    Ok(sids)
}

#[cfg(windows)]
fn private_acl<T>(
    path: &Path,
    directory: bool,
    settings: bool,
    use_acl: impl FnOnce(*mut windows_sys::Win32::Security::ACL, &[Vec<u8>]) -> T,
) -> Result<T, Unprotected> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        SetEntriesInAclW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
        SET_ACCESS, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let mut sids = credential_sids(path, settings)?;
    let entries: Vec<_> = sids.iter_mut().map(|sid| EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_mut_ptr().cast(),
        },
    }).collect();
    let mut acl = std::ptr::null_mut();
    // SAFETY: the SID allocations and entries stay live. A null old ACL creates only these grants.
    let result = unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), std::ptr::null(), &mut acl) };
    if result != 0 {
        return Err(Unprotected::Refused(windows_acl_error(path, "SetEntriesInAclW", result)));
    }
    let value = use_acl(acl, &sids);
    // SAFETY: SetEntriesInAclW allocated acl with LocalAlloc.
    unsafe { LocalFree(acl.cast()) };
    Ok(value)
}

#[cfg(windows)]
pub(super) fn windows_create_credential(path: &Path, settings: bool) -> std::io::Result<std::fs::File> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    private_acl(path, false, settings, |acl, _| {
        let mut descriptor = std::mem::MaybeUninit::<SECURITY_DESCRIPTOR>::uninit();
        let ptr = descriptor.as_mut_ptr().cast();
        // SAFETY: the absolute security descriptor has aligned storage and the ACL outlives CreateFileW.
        if unsafe { InitializeSecurityDescriptor(ptr, 1) } == 0
            || unsafe { SetSecurityDescriptorDacl(ptr, 1, acl, 0) } == 0
            || unsafe { SetSecurityDescriptorControl(ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr,
            bInheritHandle: 0,
        };
        // SAFETY: path is NUL-terminated, attrs and its descriptor live through the call.
        let handle = unsafe {
            CreateFileW(wide.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ, &attrs, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a valid owned handle, transferred to File once.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }).map_err(|error| std::io::Error::other(String::from(error)))?
}

#[cfg(windows)]
pub(super) fn windows_private(path: &Path, meta: &std::fs::Metadata) -> Result<(), Unprotected> {
    windows_protect(path, meta.is_dir(), false)
}

#[cfg(windows)]
pub(super) fn windows_private_settings(path: &Path, meta: &std::fs::Metadata) -> Result<(), Unprotected> {
    if !meta.is_file() {
        return Err(Unprotected::Refused("settings ACL requires a regular file".into()));
    }
    windows_protect(path, false, true)
}

#[cfg(windows)]
fn windows_protect(path: &Path, directory: bool, settings: bool) -> Result<(), Unprotected> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION};
    private_acl(path, directory, settings, |acl, sids| {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: NUL-terminated path and retained ACL; PROTECTED disables parent inheritance.
        // Only WRITE_DAC is required: do not set the owner or request ownership privileges.
        let result = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        };
        if result != 0 {
            return Err(windows_unprotected(path, "SetNamedSecurityInfoW", result));
        }
        let sids: Vec<_> = sids.iter().map(Vec::as_slice).collect();
        windows_verify_grants(path, directory, &sids)
    })?
}

#[cfg(windows)]
fn windows_acl_error(path: &Path, step: &str, code: u32) -> String {
    format!(
        "relay private ACL {}: {step} failed (Win32 error {code})",
        shown(path)
    )
}

/// Win32 ERROR_FILE_NOT_FOUND (2) and ERROR_PATH_NOT_FOUND (3).
#[cfg(windows)]
fn win32_vanished(code: u32) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

/// A failed native step on `path`: `Missing` when Windows reports the entry missing.
#[cfg(windows)]
fn windows_unprotected(path: &Path, step: &str, code: u32) -> Unprotected {
    let reason = windows_acl_error(path, step, code);
    if win32_vanished(code) {
        Unprotected::Missing(reason)
    } else {
        Unprotected::Refused(reason)
    }
}

#[cfg(all(test, windows))]
pub(super) fn credential_file_is_private(path: &Path, settings: bool) -> bool {
    credential_sids(path, settings).is_ok_and(|sids| {
        let sids: Vec<_> = sids.iter().map(Vec::as_slice).collect();
        windows_verify_grants(path, false, &sids).is_ok()
    })
}

#[cfg(all(test, windows))]
fn windows_verify_private(path: &Path, directory: bool, user: &[u8]) -> Result<(), Unprotected> {
    windows_verify_grants(path, directory, &[user])
}

#[cfg(windows)]
fn windows_verify_grants(path: &Path, directory: bool, sids: &[&[u8]]) -> Result<(), Unprotected> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetAce, GetSecurityDescriptorControl, IsValidAcl, ACCESS_ALLOWED_ACE, ACE_HEADER,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE,
        SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    // SAFETY: terminated path and initialized out-pointers. ACL storage belongs to descriptor.
    let result = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(windows_unprotected(
            path,
            "GetNamedSecurityInfoW (verify)",
            result,
        ));
    }
    let invalid = |reason| {
        format!(
            "relay private ACL {}: verify failed: {reason}",
            shown(path)
        )
    };
    // Keep every verification return inside the closure so the descriptor is always freed.
    let verified = (|| {
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor was returned successfully and remains allocated below.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(windows_acl_error(
                path,
                "GetSecurityDescriptorControl (verify)",
                unsafe { GetLastError() },
            ));
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(invalid(
                "DACL is not protected; parent inheritance is enabled",
            ));
        }
        // SAFETY: a non-null ACL belongs to the retained security descriptor.
        if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
            return Err(invalid("DACL is missing or invalid"));
        }
        // SAFETY: IsValidAcl checked the header and ACE bounds.
        if usize::from(unsafe { (*acl).AceCount }) != sids.len() {
            return Err(invalid("DACL contains an unexpected number of grants"));
        }
        let flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let mut remaining = sids.to_vec();
        for index in 0..sids.len() {
            let mut ace = std::ptr::null_mut();
            // SAFETY: valid ACL, in-range ACE index and initialized output pointer.
            if unsafe { GetAce(acl, index as u32, &mut ace) } == 0 {
                return Err(windows_acl_error(path, "GetAce (verify)", unsafe { GetLastError() }));
            }
            // SAFETY: GetAce returned a validated ACE header within the retained ACL.
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERITED_ACE != 0
                || header.AceType != 0
                || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            {
                return Err(invalid("DACL contains an unexpected ACE type or flags"));
            }
            let bytes = usize::from(header.AceSize) - sid_offset;
            // SAFETY: the allowed ACE's fixed fields and SID bytes fit in the checked ACL bounds.
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = unsafe { std::slice::from_raw_parts(ace.cast::<u8>().add(sid_offset), bytes) };
            let Some(found) = remaining.iter().position(|expected| *expected == sid) else {
                return Err(invalid("DACL contains a repeated or unexpected SID"));
            };
            remaining.swap_remove(found);
            if allowed.Mask != FILE_ALL_ACCESS || u32::from(header.AceFlags) != flags {
                return Err(invalid("ACE has incorrect access or inheritance flags"));
            }
        }
        Ok(())
    })();
    // SAFETY: GetNamedSecurityInfoW allocated descriptor with LocalAlloc.
    unsafe { LocalFree(descriptor) };
    verified.map_err(Unprotected::Refused)
}

#[cfg(all(test, windows))]
mod native_tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetAce, GetLengthSid, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACL,
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her relay ACL 80 {} {}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetNamedSecurityInfoW allocated this descriptor with LocalAlloc.
            unsafe { LocalFree(self.0) };
        }
    }

    // Independent native observation: do not use the production ACL verifier for assertions.
    fn assert_dacl(path: &Path, protected: bool, flags: u8) {
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
            assert_eq!(
                control & SE_DACL_PROTECTED != 0,
                protected,
                "{}",
                path.display()
            );
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 1, "only the HER account may have an ACE");
            let mut ace = std::ptr::null_mut();
            assert_ne!(GetAce(acl, 0, &mut ace), 0);
            let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(allowed.Header.AceType, 0, "must be an access-allowed ACE");
            assert_eq!(allowed.Header.AceFlags, flags);
            assert_eq!(allowed.Mask, 0x001f01ff, "HER needs full file access");
            let sid = std::ptr::addr_of!(allowed.SidStart).cast::<u8>();
            let length = GetLengthSid(sid.cast_mut().cast()) as usize;
            assert_eq!(
                std::slice::from_raw_parts(sid, length),
                identity::current_user().unwrap()
            );
        }
    }

    #[test]
    fn ticket219_private_credential_creation_allows_read_sharing() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let fixture = Fixture::new();
        let path = fixture.0.join("capture.out");
        let _writer = windows_create_credential(&path, false).unwrap();
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path);
        assert!(reader.is_ok(), "credential creation refused a concurrent reader");
    }

    #[test]
    fn relay_preparation_protects_existing_files_and_future_login_tokens() {
        let fixture = Fixture::new();
        let login = fixture.0.join("login");
        let nested = login.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let files = [
            fixture.0.join("client.key"),
            fixture.0.join("config.yaml"),
            nested.join("login.json"),
        ];
        for path in &files {
            std::fs::write(path, b"synthetic fixture").unwrap();
        }

        restrict(&fixture.0).unwrap();
        for path in [&fixture.0, &login, &nested] {
            assert_dacl(path, true, 3); // OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        }
        for path in &files {
            assert_dacl(path, true, 0);
        }

        let refresh = login.join("refresh.json");
        std::fs::write(&refresh, b"synthetic refreshed token").unwrap();
        assert_dacl(&refresh, false, 16); // Only the HER ACE, inherited from the private directory.
        let Err(Unprotected::Refused(error)) =
            windows_verify_private(&refresh, false, &identity::current_user().unwrap())
        else {
            panic!("an inherited DACL must be refused");
        };
        assert!(error.contains("DACL is not protected"), "{error}");
        let later_dir = login.join("later");
        std::fs::create_dir(&later_dir).unwrap();
        assert_dacl(&later_dir, false, 19); // The inherited HER ACE also propagates to descendants.
        let later_token = later_dir.join("token.json");
        std::fs::write(&later_token, b"synthetic new token").unwrap();
        assert_dacl(&later_token, false, 16);

        restrict(&fixture.0).unwrap();
        assert_dacl(&refresh, true, 0);
        assert_dacl(&later_dir, true, 3);
        assert_dacl(&later_token, true, 0);
    }

    #[test]
    fn relay_acl_failure_identifies_file_step_and_win32_code() {
        let fixture = Fixture::new();
        let meta = std::fs::metadata(&fixture.0).unwrap();
        // A missing entry is reported as missing, with the whole diagnostic kept for a refusal.
        for (missing, code) in [
            (fixture.0.join("missing.key"), "Win32 error 2"), // ERROR_FILE_NOT_FOUND
            (fixture.0.join("gone").join("missing.key"), "Win32 error 3"), // ERROR_PATH_NOT_FOUND
        ] {
            let Err(Unprotected::Missing(error)) = windows_private(&missing, &meta) else {
                panic!("a missing entry must be reported as missing");
            };
            assert!(error.contains("SetNamedSecurityInfoW"), "{error}");
            assert!(error.contains(code), "{error}");
            assert!(error.contains(&missing.display().to_string()), "{error}");
        }
        // Any other failure is a refusal: '<' is not a valid character in a Windows file name.
        let invalid = fixture.0.join("invalid<name.key");
        let Err(Unprotected::Refused(error)) = windows_private(&invalid, &meta) else {
            panic!("an invalid name must be refused");
        };
        assert!(error.contains("SetNamedSecurityInfoW"), "{error}");
        assert!(error.contains("Win32 error"), "{error}");
        assert!(error.contains(&invalid.display().to_string()), "{error}");
    }

    #[test]
    fn only_file_and_path_not_found_count_as_vanished() {
        assert!(win32_vanished(2)); // ERROR_FILE_NOT_FOUND
        assert!(win32_vanished(3)); // ERROR_PATH_NOT_FOUND
        for code in [0, 5, 32, 123] {
            assert!(!win32_vanished(code), "Win32 error {code}");
        }
        assert!(vanished(&std::io::Error::from_raw_os_error(2)));
        assert!(vanished(&std::io::Error::from_raw_os_error(3)));
        assert!(!vanished(&std::io::Error::from_raw_os_error(5)));
    }

    /// The version folder with `logs/` holding two synthetic request-log parts.
    fn request_log_parts(fixture: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
        let logs = fixture.0.join("logs");
        std::fs::create_dir(&logs).unwrap();
        let gone = logs.join("request-log-parts-request-body-1");
        let kept = logs.join("request-log-parts-request-body-2");
        for path in [&gone, &kept] {
            std::fs::write(path, b"synthetic fixture").unwrap();
        }
        (logs, gone, kept)
    }

    #[test]
    fn a_request_log_part_deleted_before_hardening_is_skipped_without_a_refusal() {
        let fixture = Fixture::new();
        let (logs, gone, kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, _| {
            if seam == Seam::Hardening {
                std::fs::remove_file(&gone).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_dacl(&fixture.0, true, 3);
        assert_dacl(&logs, true, 3);
        assert_dacl(&kept, true, 0);
    }

    #[test]
    fn a_request_log_part_deleted_after_listing_is_skipped_without_a_refusal() {
        let fixture = Fixture::new();
        let (logs, gone, kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Listed && path == gone.as_path() {
                std::fs::remove_file(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_dacl(&logs, true, 3);
        assert_dacl(&kept, true, 0);
    }

    #[test]
    fn a_folder_deleted_after_inspection_is_skipped_without_a_refusal() {
        let fixture = Fixture::new();
        let logs = fixture.0.join("logs");
        let session = logs.join("session");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("request-log-parts-request-body-1"), b"synthetic").unwrap();
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Inspected && path == session.as_path() {
                std::fs::remove_dir_all(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_dacl(&logs, true, 3);
    }

    #[test]
    fn a_part_created_after_a_skip_inherits_only_the_her_ace_before_the_next_sweep() {
        let fixture = Fixture::new();
        let (logs, gone, _kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Listed && path == gone.as_path() {
                std::fs::remove_file(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_dacl(&logs, true, 3);
        // A part the relay creates after the skip, with no further sweep: only the HER ACE,
        // inherited from the protected logs folder (INHERITED_ACE = 16).
        let created = logs.join("request-log-parts-request-body-3");
        std::fs::write(&created, b"synthetic request body").unwrap();
        assert_dacl(&created, false, 16);
    }

    #[test]
    fn relay_acl_verification_refuses_a_protected_foreign_sid_grant() {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        };

        let fixture = Fixture::new();
        restrict(&fixture.0).unwrap();
        let file = fixture.0.join("synthetic.key");
        std::fs::write(&file, b"synthetic fixture").unwrap();
        // A protected DACL alone is insufficient: this grants Everyone access to fake data.
        let sddl: Vec<u16> = "D:P(A;;FA;;;WD)".encode_utf16().chain(Some(0)).collect();
        let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = Descriptor(std::ptr::null_mut());
        // SAFETY: terminated SDDL/path, valid output pointers, retained descriptor owns ACL.
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
            assert_eq!(
                SetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    acl,
                    std::ptr::null(),
                ),
                0
            );
        }
        let Err(Unprotected::Refused(error)) =
            windows_verify_private(&file, false, &identity::current_user().unwrap())
        else {
            panic!("a foreign SID grant must be refused");
        };
        assert!(error.contains("SID"), "{error}");
        assert!(error.contains("verify failed"), "{error}");
        // Restore the ordinary preparation contract, and prove that it is accepted again.
        restrict(&fixture.0).unwrap();
        assert_dacl(&file, true, 0);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-relay-sweep-227-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // A test may have removed the folder itself.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The version folder with `logs/` holding two synthetic request-log parts.
    fn request_log_parts(fixture: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
        let logs = fixture.0.join("logs");
        std::fs::create_dir(&logs).unwrap();
        let gone = logs.join("request-log-parts-request-body-1");
        let kept = logs.join("request-log-parts-request-body-2");
        for path in [&gone, &kept] {
            std::fs::write(path, b"synthetic fixture").unwrap();
        }
        (logs, gone, kept)
    }

    /// A folder `logs/session` holding `parts` synthetic request-log parts.
    fn session(fixture: &Fixture, parts: usize) -> PathBuf {
        let session = fixture.0.join("logs").join("session");
        std::fs::create_dir_all(&session).unwrap();
        for n in 0..parts {
            let part = session.join(format!("request-log-parts-request-body-{n}"));
            std::fs::write(part, b"synthetic fixture").unwrap();
        }
        session
    }

    /// One synthetic part and its metadata, for `protect` with a stand-in `private`.
    fn part(fixture: &Fixture) -> (PathBuf, std::fs::Metadata) {
        let path = fixture.0.join("request-log-parts-request-body-1");
        std::fs::write(&path, b"synthetic fixture").unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        (path, meta)
    }

    fn missing() -> Unprotected {
        Unprotected::Missing("synthetic missing".into())
    }

    #[test]
    fn only_a_missing_entry_counts_as_vanished() {
        assert!(vanished(&std::io::Error::from_raw_os_error(libc::ENOENT)));
        for code in [libc::EACCES, libc::EPERM, libc::ENOTDIR, libc::EIO] {
            assert!(!vanished(&std::io::Error::from_raw_os_error(code)), "errno {code}");
        }
    }

    #[test]
    fn a_sweep_diagnostic_names_the_path_without_control_or_formatting_characters() {
        let path = Path::new("/relay/logs/part\nrefused: forged\u{202e}name");
        assert_eq!(
            named("relay path is unavailable", path),
            "relay path is unavailable: /relay/logs/part?refused: forged?name"
        );
        // Bidirectional controls (U+061C included), line and paragraph separators, zero-width
        // characters, the byte-order mark and C1 controls.
        for c in [
            '\u{061c}',
            '\u{200b}',
            '\u{200e}',
            '\u{200f}',
            '\u{2028}',
            '\u{2029}',
            '\u{202a}',
            '\u{2066}',
            '\u{feff}',
            '\u{85}',
        ] {
            let path = PathBuf::from(format!("/relay/logs/part{c}name"));
            assert_eq!(shown(&path), "/relay/logs/part?name", "U+{:04X}", u32::from(c));
        }
    }

    #[test]
    fn a_request_log_part_deleted_after_listing_is_skipped_and_counted() {
        let fixture = Fixture::new();
        let (logs, gone, kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Listed && path == gone.as_path() {
                std::fs::remove_file(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_eq!(mode(&fixture.0), 0o700);
        assert_eq!(mode(&logs), 0o700);
        assert_eq!(mode(&kept), 0o600);
    }

    #[test]
    fn a_request_log_part_deleted_before_hardening_is_skipped_and_counted() {
        let fixture = Fixture::new();
        let (logs, gone, kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, _| {
            if seam == Seam::Hardening {
                std::fs::remove_file(&gone).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_eq!(mode(&logs), 0o700);
        assert_eq!(mode(&kept), 0o600);
    }

    #[test]
    fn a_folder_deleted_with_its_parts_before_hardening_is_skipped_and_counted() {
        let fixture = Fixture::new();
        let session = session(&fixture, 2);
        let skipped = sweep(&fixture.0, &mut |seam, _| {
            if seam == Seam::Hardening {
                std::fs::remove_dir_all(&session).unwrap();
            }
        });
        // The session folder and both of its parts.
        assert_eq!(skipped, Ok(3));
        assert_eq!(mode(&fixture.0.join("logs")), 0o700);
    }

    #[test]
    fn a_folder_deleted_after_inspection_is_skipped_and_counted() {
        let fixture = Fixture::new();
        let session = session(&fixture, 1);
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Inspected && path == session.as_path() {
                std::fs::remove_dir_all(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        assert_eq!(mode(&fixture.0.join("logs")), 0o700);
    }

    /// `stat` as the kernel can answer it while a sweeper races a deleter: the lookup found `entry`,
    /// the entry is unlinked, and the attributes read afterwards come from the unlinked inode
    /// (owner unchanged, link count 0).
    fn unlinked_during_stat(
        entry: &Path,
    ) -> impl Fn(&Path) -> std::io::Result<std::fs::Metadata> + '_ {
        let handle = std::fs::File::open(entry).unwrap();
        move |path| {
            if path == entry {
                std::fs::remove_file(entry).unwrap();
                handle.metadata()
            } else {
                std::fs::symlink_metadata(path)
            }
        }
    }

    #[test]
    fn a_part_unlinked_while_its_metadata_is_read_is_skipped_not_refused_as_foreign() {
        let fixture = Fixture::new();
        let (path, _) = part(&fixture);
        let result = inspect_with(&path, true, &unlinked_during_stat(&path));
        assert_eq!(result.map(|found| found.is_some()), Ok(false));
    }

    #[test]
    fn an_unlisted_entry_unlinked_while_its_metadata_is_read_is_still_refused() {
        let fixture = Fixture::new();
        let (path, _) = part(&fixture);
        let result = inspect_with(&path, false, &unlinked_during_stat(&path));
        assert_eq!(
            result.map(|found| found.is_some()),
            Err("relay private files must belong only to the HER account".to_string())
        );
    }

    #[test]
    fn a_part_with_a_second_link_is_refused_as_not_owned_alone() {
        let fixture = Fixture::new();
        let (path, _) = part(&fixture);
        std::fs::hard_link(&path, fixture.0.join("alias")).unwrap();
        let result = inspect(&path, true);
        assert_eq!(
            result.map(|found| found.is_some()),
            Err("relay private files must belong only to the HER account".to_string())
        );
    }

    #[test]
    fn a_missing_version_folder_is_refused_and_named() {
        let fixture = Fixture::new();
        let absent = fixture.0.join("absent");
        assert_eq!(
            restrict(&absent),
            Err(format!(
                "relay path is unavailable: {}: symlink_metadata failed (errno {})",
                absent.display(),
                libc::ENOENT
            ))
        );
    }

    #[test]
    fn a_version_folder_deleted_during_the_sweep_is_refused() {
        let fixture = Fixture::new();
        request_log_parts(&fixture);
        let result = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Inspected && path == fixture.0.as_path() {
                std::fs::remove_dir_all(path).unwrap();
            }
        });
        assert_eq!(
            result,
            Err(format!(
                "relay private directory cannot be inspected: {}: read_dir failed (errno {})",
                fixture.0.display(),
                libc::ENOENT
            ))
        );
    }

    #[test]
    fn an_unreadable_part_is_refused_and_named() {
        if unsafe { libc::geteuid() } == 0 {
            // Root reads every entry; CI runs the suite as an unprivileged user.
            return;
        }
        let fixture = Fixture::new();
        let (logs, _, _) = request_log_parts(&fixture);
        // Listing needs read permission only; inspecting an entry needs search permission too.
        set_mode(&logs, 0o600);
        let result = restrict(&fixture.0);
        set_mode(&logs, 0o700);
        let error = result.unwrap_err();
        let prefix = format!(
            "relay path is protected: {}/request-log-parts-request-body-",
            logs.display()
        );
        assert!(error.starts_with(&prefix), "{error}");
    }

    #[test]
    fn a_missing_report_for_an_entry_that_still_exists_is_retried_once() {
        let fixture = Fixture::new();
        let (path, meta) = part(&fixture);
        let mut calls = 0;
        let result = protect(&path, meta, true, |_, _| {
            calls += 1;
            if calls == 1 {
                Err(missing())
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Ok(true));
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_repeated_missing_report_for_an_existing_entry_is_refused() {
        let fixture = Fixture::new();
        let (path, meta) = part(&fixture);
        let mut calls = 0;
        let result = protect(&path, meta, true, |_, _| {
            calls += 1;
            Err(missing())
        });
        assert_eq!(result, Err("synthetic missing".to_string()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_repeated_missing_report_for_the_version_folder_is_refused() {
        let fixture = Fixture::new();
        let meta = std::fs::symlink_metadata(&fixture.0).unwrap();
        let mut calls = 0;
        let result = protect(&fixture.0, meta, false, |_, _| {
            calls += 1;
            Err(missing())
        });
        assert_eq!(result, Err("synthetic missing".to_string()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_version_folder_that_is_gone_is_refused_and_named() {
        let fixture = Fixture::new();
        let meta = std::fs::symlink_metadata(&fixture.0).unwrap();
        let mut calls = 0;
        let result = protect(&fixture.0, meta, false, |entry, _| {
            calls += 1;
            std::fs::remove_dir(entry).unwrap();
            Err(missing())
        });
        assert_eq!(
            result,
            Err(format!(
                "relay path is unavailable: {}: symlink_metadata failed (errno {})",
                fixture.0.display(),
                libc::ENOENT
            ))
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_missing_report_for_an_entry_that_is_gone_is_skipped() {
        let fixture = Fixture::new();
        let (path, meta) = part(&fixture);
        let mut calls = 0;
        let result = protect(&path, meta, true, |entry, _| {
            calls += 1;
            std::fs::remove_file(entry).unwrap();
            Err(missing())
        });
        assert_eq!(result, Ok(false));
        assert_eq!(calls, 1);
    }

    #[test]
    fn any_other_failure_refuses_even_when_the_entry_is_gone() {
        let fixture = Fixture::new();
        let (path, meta) = part(&fixture);
        let mut calls = 0;
        let result = protect(&path, meta, true, |entry, _| {
            calls += 1;
            std::fs::remove_file(entry).unwrap();
            Err(Unprotected::Refused("synthetic refusal".into()))
        });
        assert_eq!(result, Err("synthetic refusal".to_string()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_retry_reinspects_and_refuses_an_entry_replaced_by_a_link() {
        let fixture = Fixture::new();
        let (path, meta) = part(&fixture);
        let outside = Fixture::new();
        let foreign = outside.0.join("foreign");
        std::fs::write(&foreign, b"synthetic fixture").unwrap();
        let before = mode(&foreign);
        let mut calls = 0;
        let result = protect(&path, meta, true, |entry, _| {
            calls += 1;
            std::fs::remove_file(entry).unwrap();
            std::os::unix::fs::symlink(&foreign, entry).unwrap();
            Err(missing())
        });
        assert_eq!(result, Err("relay paths must not be redirected".to_string()));
        assert_eq!(calls, 1);
        assert_eq!(mode(&foreign), before);
    }

    #[test]
    fn an_ancestor_read_error_stays_a_refusal_even_when_the_entry_is_gone() {
        let fixture = Fixture::new();
        let (path, _) = part(&fixture);
        std::fs::remove_file(&path).unwrap();
        let result = ancestors_checked(&path, true, |ancestor| {
            if ancestor == fixture.0.as_path() {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            } else {
                std::fs::symlink_metadata(ancestor)
            }
        });
        assert_eq!(
            result,
            Err(format!(
                "relay path is unavailable: {}: symlink_metadata failed (errno {})",
                fixture.0.display(),
                libc::EIO
            ))
        );
    }

    #[test]
    fn a_listing_failure_names_the_path_step_and_code() {
        if unsafe { libc::geteuid() } == 0 {
            // Root reads every directory; CI runs the suite as an unprivileged user.
            return;
        }
        let fixture = Fixture::new();
        let (logs, _, _) = request_log_parts(&fixture);
        // Search and write permission without read: the entries cannot be listed.
        set_mode(&logs, 0o300);
        let result = restrict(&fixture.0);
        set_mode(&logs, 0o700);
        assert_eq!(
            result,
            Err(format!(
                "relay private directory cannot be inspected: {}: read_dir failed (errno {})",
                logs.display(),
                libc::EACCES
            ))
        );
    }

    #[test]
    fn a_permission_failure_names_the_path_step_and_code() {
        if unsafe { libc::geteuid() } == 0 {
            // Root may change any mode; CI runs the suite as an unprivileged user.
            return;
        }
        let fixture = Fixture::new();
        let dir = fixture.0.join("dir");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("request-log-parts-request-body-1");
        std::fs::write(&file, b"synthetic fixture").unwrap();
        let meta = std::fs::symlink_metadata(&file).unwrap();
        // Without search permission on the directory, the file's mode cannot be changed.
        set_mode(&dir, 0o600);
        let result = unix_private(&file, &meta);
        set_mode(&dir, 0o700);
        let Err(Unprotected::Refused(error)) = result else {
            panic!("a denied permission change must be refused");
        };
        assert_eq!(
            error,
            format!(
                "relay private permissions could not be applied: {}: set_permissions failed (errno {})",
                file.display(),
                libc::EACCES
            )
        );
    }

    #[test]
    fn a_part_created_after_a_skip_sits_in_a_private_folder_before_the_next_sweep() {
        let fixture = Fixture::new();
        let (logs, gone, _kept) = request_log_parts(&fixture);
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Listed && path == gone.as_path() {
                std::fs::remove_file(path).unwrap();
            }
        });
        assert_eq!(skipped, Ok(1));
        // A part the relay creates after the skip, with no further sweep: the 0700 version folder
        // and the 0700 logs folder keep other accounts out of it.
        let created = logs.join("request-log-parts-request-body-3");
        std::fs::write(&created, b"synthetic request body").unwrap();
        assert_eq!(mode(&fixture.0), 0o700);
        assert_eq!(mode(&logs), 0o700);
        assert!(created.is_file());
    }

    #[test]
    fn request_log_parts_deleted_while_two_sweepers_run_never_refuse() {
        let fixture = Fixture::new();
        let logs = fixture.0.join("logs");
        std::fs::create_dir(&logs).unwrap();
        for n in 0..300 {
            let part = logs.join(format!("request-log-parts-request-body-{n}"));
            std::fs::write(part, b"synthetic fixture").unwrap();
        }
        // The first sweeper asks the deleter to remove each part it has listed, and waits for that
        // removal before it inspects the part. Every listed part is therefore a vanished entry.
        let (ask_tx, ask_rx) = std::sync::mpsc::channel::<PathBuf>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
        let deleter = std::thread::spawn(move || {
            for part in ask_rx {
                std::fs::remove_file(&part).unwrap();
                ack_tx.send(()).unwrap();
            }
        });
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A second supervisor sharing the dependency folder sweeps it at the same time.
        let other = {
            let (folder, done) = (fixture.0.clone(), done.clone());
            std::thread::spawn(move || {
                let mut skipped = 0;
                while !done.load(Ordering::SeqCst) {
                    skipped += restrict(&folder).unwrap();
                }
                skipped
            })
        };
        let skipped = sweep(&fixture.0, &mut |seam, path| {
            if seam == Seam::Listed && path.parent() == Some(logs.as_path()) {
                ask_tx.send(path.to_path_buf()).unwrap();
                ack_rx.recv().unwrap();
            }
        });
        drop(ask_tx);
        done.store(true, Ordering::SeqCst);
        deleter.join().unwrap();
        let other = other.join().unwrap();
        // This sweep listed and inspected all 300 parts, each removed in between: all are skipped.
        assert_eq!(skipped, Ok(300));
        assert!(other <= 300, "{other}");
        assert_eq!(restrict(&fixture.0), Ok(0));
        assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 0);
    }
}
