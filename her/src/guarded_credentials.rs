//! Private file creation and restriction for bridge credentials and settings.

use crate::guarded_file::{self, is_link, Failure, Staged};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

/// Create a new owner-only file before any content or handle is available to another account.
pub(crate) fn create_credential(path: &Path) -> io::Result<File> {
    create_protected_file(path, false)
}

pub(crate) fn create_settings_file(path: &Path) -> io::Result<File> {
    create_protected_file(path, true)
}

fn create_protected_file(path: &Path, _settings: bool) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
    }
    #[cfg(windows)]
    {
        crate::supervisor_dependencies::create_credential_file(path, _settings)
    }
}

/// Restrict a contained credential file or directory before any secret bytes are written.
pub(crate) fn restrict_credential(path: &Path) -> io::Result<()> {
    restrict_protected_path(path, false)
}

pub(crate) fn restrict_settings_file(path: &Path) -> io::Result<()> {
    restrict_protected_path(path, true)
}

fn restrict_protected_path(path: &Path, settings: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) || !(metadata.is_file() || (!settings && metadata.is_dir())) {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || (metadata.is_file() && metadata.nlink() != 1) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let mode = if metadata.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        crate::supervisor_dependencies::restrict_credential_path(path, &metadata, settings).map_err(io::Error::other)
    }
}

pub(crate) fn stage_settings_file(target: &Path, input: &mut dyn Read) -> Result<Staged, Failure> {
    guarded_file::stage_with(target, input, create_settings_file)
}
