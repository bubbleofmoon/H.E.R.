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
        // Only status is needed. Never retain a response body (including credential echoes),
        // follow redirects, or let a peer extend the absolute deadline by dribbling bytes.
        let mut line = Vec::new();
        for _ in 0..512 {
            stream
                .set_read_timeout(Some(remaining()?))
                .map_err(|_| "relay health timeout is unavailable")?;
            let mut byte = [0];
            stream
                .read_exact(&mut byte)
                .map_err(|_| "relay health response failed or timed out")?;
            line.push(byte[0]);
            if byte[0] == b'\n' {
                let status = std::str::from_utf8(&line).ok().and_then(|line| {
                    let mut fields = line.split_whitespace();
                    matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1"))
                        .then(|| fields.next())
                        .flatten()
                });
                return if status == Some("200") {
                    Ok(())
                } else {
                    Err("relay models health request was refused".into())
                };
            }
        }
        Err("relay health response is invalid".into())
    }
}

/// Restrict the entire version tree, so config and future login files are protected too.
/// Reject redirected entries before changing any permissions; never traverse a foreign tree.
pub(super) fn restrict(folder: &Path) -> Result<(), String> {
    fn collect(path: &Path, entries: &mut Vec<(PathBuf, std::fs::Metadata)>) -> Result<(), String> {
        regular_tree_path(path)?;
        let meta =
            std::fs::symlink_metadata(path).map_err(|_| "relay private files are unavailable")?;
        if !meta.is_file() && !meta.is_dir() {
            return Err("relay private files must be regular files or directories".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != unsafe { libc::geteuid() } || (meta.is_file() && meta.nlink() != 1) {
                return Err("relay private files must belong only to the HER account".into());
            }
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(path)
                .map_err(|_| "relay private directory cannot be inspected")?
            {
                collect(
                    &entry
                        .map_err(|_| "relay private directory cannot be inspected")?
                        .path(),
                    entries,
                )?;
            }
        }
        entries.push((path.to_path_buf(), meta));
        Ok(())
    }
    let mut entries = Vec::new();
    collect(folder, &mut entries)?;
    // Parent first closes external access before processing its children.
    for (path, meta) in entries.into_iter().rev() {
        super::os::private(&path, &meta)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn unix_private(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if meta.is_dir() {
        0o700
    } else {
        (meta.permissions().mode() & 0o700) | 0o600
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|_| "relay private permissions could not be applied".into())
}

#[cfg(windows)]
pub(super) fn windows_private(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
        SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    let mut user = identity::current_user().map_err(|error| {
        format!(
            "relay private ACL {}: process token SID: {error}",
            path.display()
        )
    })?;
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: user.as_mut_ptr().cast(),
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if meta.is_dir() {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        },
        Trustee: trustee,
    };
    let mut acl = std::ptr::null_mut();
    // SAFETY: retained SID and initialized ACL out-pointer; a null old ACL creates only
    // the HER account ACE. No environment username or broad trustee grants participate.
    let result = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) };
    if result != 0 {
        return Err(windows_acl_error(path, "SetEntriesInAclW", result));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: NUL-terminated path and retained ACL; PROTECTED disables parent inheritance.
    // Only WRITE_DAC is required: do not set the owner or request ownership privileges.
    // The scheduled task and relay run as this account, so SYSTEM needs no separate ACE.
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
    // SAFETY: SetEntriesInAclW allocated acl with LocalAlloc.
    unsafe {
        LocalFree(acl.cast());
    }
    if result != 0 {
        return Err(windows_acl_error(path, "SetNamedSecurityInfoW", result));
    }
    windows_verify_private(path, meta.is_dir(), &user)
}

#[cfg(windows)]
fn windows_acl_error(path: &Path, step: &str, code: u32) -> String {
    format!(
        "relay private ACL {}: {step} failed (Win32 error {code})",
        path.display()
    )
}

#[cfg(windows)]
fn windows_verify_private(path: &Path, directory: bool, user: &[u8]) -> Result<(), String> {
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
        return Err(windows_acl_error(
            path,
            "GetNamedSecurityInfoW (verify)",
            result,
        ));
    }
    let invalid = |reason| {
        format!(
            "relay private ACL {}: verify failed: {reason}",
            path.display()
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
        if unsafe { (*acl).AceCount } != 1 {
            return Err(invalid("DACL must contain exactly one HER account ACE"));
        }
        let mut ace = std::ptr::null_mut();
        // SAFETY: valid ACL, in-range ACE index and initialized output pointer.
        if unsafe { GetAce(acl, 0, &mut ace) } == 0 {
            return Err(windows_acl_error(path, "GetAce (verify)", unsafe {
                GetLastError()
            }));
        }
        // SAFETY: GetAce returned a validated ACE header within the retained ACL.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERITED_ACE != 0 {
            return Err(invalid("DACL contains an inherited ACE"));
        }
        let flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        // ACCESS_ALLOWED_ACE_TYPE is 0. Reject other ACE layouts before interpreting the SID.
        if header.AceType != 0 || usize::from(header.AceSize) != sid_offset + user.len() {
            return Err(invalid("DACL contains an unexpected ACE type or SID size"));
        }
        // SAFETY: ACE type and full SID bounds were checked; user is the valid token SID.
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid =
            unsafe { std::slice::from_raw_parts(ace.cast::<u8>().add(sid_offset), user.len()) };
        if sid != user {
            return Err(invalid("DACL contains an ACE for another SID"));
        }
        if allowed.Mask != FILE_ALL_ACCESS || u32::from(header.AceFlags) != flags {
            return Err(invalid("HER ACE has incorrect access or inheritance flags"));
        }
        Ok(())
    })();
    // SAFETY: GetNamedSecurityInfoW allocated descriptor with LocalAlloc.
    unsafe { LocalFree(descriptor) };
    verified
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
        let error = windows_verify_private(&refresh, false, &identity::current_user().unwrap())
            .unwrap_err();
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
        let missing = fixture.0.join("missing.key");
        let meta = std::fs::metadata(&fixture.0).unwrap();
        let error = windows_private(&missing, &meta).unwrap_err();
        assert!(error.contains("SetNamedSecurityInfoW"), "{error}");
        assert!(error.contains("Win32 error 2"), "{error}"); // ERROR_FILE_NOT_FOUND
        assert!(error.contains(&missing.display().to_string()), "{error}");
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
        let error =
            windows_verify_private(&file, false, &identity::current_user().unwrap()).unwrap_err();
        assert!(error.contains("SID"), "{error}");
        assert!(error.contains("verify failed"), "{error}");
        // Restore the ordinary preparation contract, and prove that it is accepted again.
        restrict(&fixture.0).unwrap();
        assert_dacl(&file, true, 0);
    }
}
