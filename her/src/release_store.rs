//! Shared release layout, checksums, guarded files and installation locks.
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
};
pub(crate) const IMAGES: [&str; 3] = ["her.exe", "her-cli.exe", "herupdate.exe"];

/// Bounded metadata reads for installed checksum lists and detached signatures.
pub(crate) fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|_| "release metadata unavailable")?;
    if !meta.is_file() || redirected(&meta) || meta.len() > max as u64 {
        return Err("release metadata redirected or oversized".into());
    }
    let file = fs::File::open(path).map_err(|_| "release metadata unavailable")?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "release metadata unreadable")?;
    if bytes.len() > max {
        return Err("release metadata oversized".into());
    }
    Ok(bytes)
}

pub(crate) fn version(tag: &str) -> Option<(u64, u64, u64)> {
    let parts = tag.strip_prefix('v')?.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || p.len() > 1 && p.starts_with('0')
                || !p.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

pub(crate) fn redirected(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) fn directory(path: &Path) -> Result<(), String> {
    for part in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(part) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err("release directory cannot be created".into()),
                }
            }
            Err(_) => return Err("release directory cannot be inspected".into()),
        }
        let metadata =
            fs::symlink_metadata(part).map_err(|_| "release directory cannot be inspected")?;
        if !metadata.is_dir() || redirected(&metadata) {
            return Err("release directory is redirected or not a directory".into());
        }
    }
    Ok(())
}

pub(crate) fn sums(bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "SHA256SUMS must be UTF-8")?
        .trim_start_matches('\u{feff}');
    let mut hashes = HashMap::new();
    for line in text.lines().filter(|line| {
        !line.trim().is_empty()
            && !line.starts_with("# HER-RELEASE ")
            && !line.starts_with("# HER-KEYS ")
    }) {
        let (hash, name) = line
            .split_once(char::is_whitespace)
            .ok_or("SHA256SUMS line is invalid")?;
        let name = name.trim().strip_prefix('*').unwrap_or(name.trim());
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || name.is_empty()
            || hashes
                .insert(name.to_string(), hash.to_ascii_lowercase())
                .is_some()
        {
            return Err("SHA256SUMS contains an invalid or duplicate entry".into());
        }
    }
    if IMAGES.iter().any(|name| !hashes.contains_key(*name)) {
        return Err("SHA256SUMS must cover all three release images".into());
    }
    Ok(hashes)
}

pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    directory(path.parent().ok_or("release file has no directory")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if path.extension().is_some_and(|ext| ext == "exe") {
            0o700
        } else {
            0o600
        });
    }
    let mut file = options
        .open(path)
        .map_err(|_| "release file cannot be created")?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "release file cannot be saved".into())
}

pub(crate) fn installation_gate(lock: &Path) -> Result<fs::File, String> {
    directory(lock.parent().ok_or("release gate has no directory")?)?;
    if fs::symlink_metadata(&lock).is_ok_and(|meta| !meta.is_file() || redirected(&meta)) {
        return Err("release install gate is redirected".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let gate = options
        .open(lock)
        .map_err(|_| "release install gate unavailable")?;
    if redirected(
        &gate
            .metadata()
            .map_err(|_| "release gate metadata unavailable")?,
    ) {
        return Err("release install gate is redirected".into());
    }
    gate.try_lock()
        .map_err(|_| "another release installation or restore is running")?;
    Ok(gate)
}

/// Fsynced root-level state/helper publication under the caller's installation gate.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    directory(path.parent().ok_or("release state has no parent")?)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file() || redirected(&m)) {
        return Err("release state is redirected".into());
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = path.with_file_name(format!(
        ".her-update-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    write_new(&temp, contents)?;
    #[cfg(windows)]
    let result = {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let from: Vec<u16> = temp.as_os_str().encode_wide().chain([0]).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: both vectors are NUL-terminated and live through the call.
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    #[cfg(not(windows))]
    let result = fs::rename(&temp, path);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|_| "release state cannot publish")?;
    sync_directory(path.parent().ok_or("release state has no parent")?)
}

/// Persist directory entries after publishing synced files or release links.
pub(crate) fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "release directory cannot sync")?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
