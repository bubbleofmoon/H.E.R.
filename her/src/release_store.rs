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
    if !meta.is_file() || crate::guarded_file::is_link(&meta) || meta.len() > max as u64 {
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
        if !metadata.is_dir() || crate::guarded_file::is_link(&metadata) {
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

#[allow(dead_code)] // herupdate compiles release_store without release.rs, the only caller
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
    if fs::symlink_metadata(&lock).is_ok_and(|meta| !meta.is_file() || crate::guarded_file::is_link(&meta)) {
        return Err("release install gate is redirected".into());
    }
    let gate = crate::guarded_file::open_gate(lock, crate::guarded_file::Gate::Create).map_err(|error| match error {
        crate::guarded_file::GateError::Open(_) => "release install gate unavailable",
        _ => "release install gate is redirected",
    })?;
    gate.try_lock()
        .map_err(|_| "another release installation or restore is running")?;
    Ok(gate)
}

/// True only when another handle holds the gate (a non-blocking lock attempt reports it would
/// block). Any other outcome is false: an unusable or redirected path, another lock error, or the
/// lock acquired, which is released again before returning.
pub(crate) fn gate_contended(lock: &Path) -> bool {
    if fs::symlink_metadata(lock)
        .is_ok_and(|meta| !meta.is_file() || crate::guarded_file::is_link(&meta))
    {
        return false;
    }
    let Ok(gate) = crate::guarded_file::open_gate(lock, crate::guarded_file::Gate::Create) else {
        return false;
    };
    matches!(gate.try_lock(), Err(fs::TryLockError::WouldBlock))
}

/// Fsynced root-level state/helper publication under the caller's installation gate.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    directory(path.parent().ok_or("release state has no parent")?)?;
    let mode = if path.extension().is_some_and(|ext| ext == "exe") { 0o700 } else { 0o600 };
    crate::guarded_file::stage(path, &mut &contents[..], Some(mode))
        .and_then(|staged| staged.commit(&mut || Ok::<(), String>(())))
        .map_err(|failure| {
            String::from(match failure.class() {
                "refused" => "release state is redirected",
                "temp/create" => "release file cannot be created",
                "temp/write" | "temp/sync" => "release file cannot be saved",
                "dir/sync" => "release directory cannot sync",
                _ => "release state cannot publish",
            })
        })
}

/// Persist directory entries after publishing synced files or release links.
pub(crate) fn sync_directory(path: &Path) -> Result<(), String> {
    crate::guarded_file::sync_directory(path).map_err(|_| "release directory cannot sync".into())
}

#[cfg(test)]
mod gate_claim_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gate_contended_only_when_another_handle_holds_the_gate() {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-release-store-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("gate.lock");
        let held = installation_gate(&lock).unwrap();
        let contended = gate_contended(&lock);
        drop(held);
        assert!(
            contended,
            "a gate held through another handle reads as contended"
        );
        assert!(!gate_contended(&lock), "a free gate is not contended");
        assert!(
            installation_gate(&lock).is_ok(),
            "the probe must not leave the gate held"
        );
        let planted = dir.join("planted.lock");
        fs::create_dir(&planted).unwrap();
        assert!(
            !gate_contended(&planted),
            "a directory at the gate path is not contention"
        );
    }
}
