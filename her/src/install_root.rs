//! Production install-root layout and the shared development boundary.
#![allow(dead_code)]
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_INSTALL_ROOT: &str = if cfg!(windows) { "C:/HER" } else { "/HER" };

pub struct InstallRoot {
    lexical: PathBuf,
    resolved: PathBuf,
}

impl InstallRoot {
    pub fn select(args: &mut Vec<String>) -> Result<Self, String> {
        let mut root = std::env::var_os("HER_INSTALL_ROOT").map(PathBuf::from);
        let mut index = 1;
        while index < args.len() && args[index] != "--" {
            if args[index] == "--install-root" {
                if root.is_some() {
                    return Err("install root was selected more than once".into());
                }
                let value = args
                    .get(index + 1)
                    .filter(|s| !s.trim().is_empty() && !s.starts_with("--"))
                    .ok_or("install root value is missing")?;
                root = Some(PathBuf::from(value));
                args.drain(index..index + 2);
            } else {
                index += 1;
            }
        }
        if root.is_none() && (args.len() == 1 || args.get(1).is_some_and(|arg| arg == "app")) {
            // Resolve `current` before deriving the root. Only desktop dispatch has this
            // default; explicit selections and every other command retain their boundary.
            if let Ok(image) = std::env::current_exe().and_then(std::fs::canonicalize) {
                if let Some(releases) = image.parent().and_then(Path::parent) {
                    if releases.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case("releases")) {
                        if let Some(candidate) = releases.parent().and_then(|path| Self::new(path).ok()) {
                            if candidate.require_release_image(&image).is_ok() { return Ok(candidate); }
                        }
                    }
                }
            }
        }
        Self::new(&root.unwrap_or_else(|| PathBuf::from(DEFAULT_INSTALL_ROOT)))
    }

    pub fn configured() -> Result<Self, String> {
        Self::new(
            &std::env::var_os("HER_INSTALL_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTALL_ROOT)),
        )
    }

    pub fn new(root: &Path) -> Result<Self, String> {
        Ok(Self {
            lexical: normalize(root, false)?,
            resolved: normalize(root, true)?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.lexical
    }

    /// Installed runtimes keep backups beside the runtime; legacy roots retain their own base.
    /// Lexical only: loading settings never touches the destination (it may be a slow share).
    pub fn backup_destination(&self, runtime: &Path, configured: &Path) -> Result<PathBuf, String> {
        let base = if self.protects(runtime)? {
            self.root()
        } else {
            runtime
        };
        let destination = normalize(&base.join(configured), false)?;
        self.refuse_backup_destination(&destination)?;
        Ok(destination)
    }

    /// For console saves and backup runs: also resolve existing local aliases (symlinks,
    /// junctions) before the same refusal. UNC destinations are never contacted here; mapped
    /// drive letters are ordinary local paths. An unavailable destination remains editable;
    /// actual I/O still rejects redirected or unreadable components before creating anything.
    pub fn check_resolved_backup_destination(&self, destination: &Path) -> Result<(), String> {
        #[cfg(windows)]
        let network = matches!(destination.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)));
        #[cfg(not(windows))]
        let network = false;
        if network {
            return Ok(());
        }
        match normalize(destination, true) {
            Ok(resolved) => self.refuse_backup_destination(&resolved),
            Err(_) => Ok(()),
        }
    }

    fn refuse_backup_destination(&self, candidate: &Path) -> Result<(), String> {
        for root in [&self.lexical, &self.resolved] {
            for area in ["releases", "deps", "runtime"] {
                if within(candidate, &root.join(area))? {
                    return Err("backup destination cannot be inside install-root releases, deps or runtime".into());
                }
            }
        }
        Ok(())
    }

    pub fn layout(&self) -> Vec<(&'static str, PathBuf)> {
        [
            ("installRoot", ""),
            ("updater", "herupdate.exe"),
            ("current", "current"),
            ("lastGood", "last-good"),
            ("releases", "releases"),
            ("dependencies", "deps"),
            ("snowLuma", "deps/snowluma"),
            ("dsh", "deps/dsh"),
            ("relay", "deps/relay"),
            ("runtime", "runtime"),
            ("dshHome", "runtime/dsh/home"),
            ("dshWork", "runtime/dsh/work"),
            ("backups", "backups"),
        ]
        .into_iter()
        .map(|(name, relative)| (name, self.lexical.join(relative)))
        .collect()
    }

    pub fn protects(&self, runtime: &Path) -> Result<bool, String> {
        if !runtime.is_absolute() {
            return Err(
                "runtime root is unavailable: --runtime-root must be an absolute path".into(),
            );
        }
        if within(&normalize(runtime, false)?, &self.lexical)?
            || within(&normalize(runtime, true)?, &self.resolved)?
        {
            return Ok(true);
        }
        // A redirected ancestor can enter production even when a following `..` leaves it.
        for ancestor in runtime.ancestors() {
            if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
                #[cfg(windows)]
                let redirected = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let redirected = metadata.file_type().is_symlink();
                if redirected && within(&normalize(ancestor, true)?, &self.resolved)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn require_release_image(&self, image: &Path) -> Result<(), String> {
        let image = normalize(image, true)?;
        let releases = self.resolved.join("releases");
        if within(&image, &releases)?
            && image.components().count() >= releases.components().count() + 2
        {
            Ok(())
        } else {
            Err("supervisor executable must live under the install root's releases".into())
        }
    }

    pub fn check_development(&self, runtime: &Path) -> Result<(), String> {
        if self.protects(runtime)? {
            Err("development runtime root is under the production install root".into())
        } else {
            Ok(())
        }
    }

    pub fn check_runtime(
        &self,
        runtime: &Path,
        image: &Path,
        supervisor: bool,
    ) -> Result<(), String> {
        if self.protects(runtime)? {
            self.require_release_image(image).map_err(|error| {
                if supervisor {
                    error
                } else {
                    "development runtime root is under the production install root".into()
                }
            })?;
        }
        Ok(())
    }
}

/// Called on a command's parsed selection, before runtime lookup or mutation.
pub fn check_runtime(runtime: &Path, supervisor: bool) -> Result<(), String> {
    let image = std::env::current_exe().map_err(|_| "executable identity is unavailable")?;
    InstallRoot::configured()?.check_runtime(runtime, &image, supervisor)
}

/// Caller-chosen development outputs may be relative to the working directory.
pub fn check_development_path(path: &Path) -> Result<(), String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "working directory is unavailable")?
            .join(path)
    };
    InstallRoot::configured()?.check_development(&path)
}

/// Shared by fixture allocators before their first create/remove, including worktree-local roots.
pub fn development_root(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    InstallRoot::configured()
        .and_then(|install| install.check_development(path))
        .unwrap_or_else(|reason| panic!("fixture root refused: {reason}"));
    path.to_path_buf()
}

// Compare components, including a path separator boundary: HER-other is never inside HER.
fn within(path: &Path, root: &Path) -> Result<bool, String> {
    Ok(comparison_path(path)?.starts_with(comparison_path(root)?))
}

fn comparison_path(path: &Path) -> Result<PathBuf, String> {
    let text = path.to_str().ok_or("guard path is not Unicode")?;
    #[cfg(windows)]
    {
        let text = text.replace('/', "\\");
        let text = if text
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case(r"\\?\UNC\"))
        {
            format!(r"\\{}", &text[8..])
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
        };
        return Ok(PathBuf::from(text.to_lowercase()));
    }
    #[cfg(not(windows))]
    Ok(PathBuf::from(text))
}

/// Resolve each existing component before applying `..`, so aliases followed by a parent cannot
/// escape the check. Missing suffixes are preserved: the guard runs before any directory is made.
fn normalize(path: &Path, resolve: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("guard path must be absolute".into());
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                #[cfg(windows)]
                {
                    let name = name.to_str().ok_or("guard path is not Unicode")?;
                    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
                    if name.ends_with([' ', '.'])
                        || name.contains([':', '"', '<', '>', '|', '?', '*'])
                        || name.chars().any(|c| c < ' ')
                        || ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
                            .contains(&stem.as_str())
                        || (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && matches!(
                                &stem[3..],
                                "1" | "2"
                                    | "3"
                                    | "4"
                                    | "5"
                                    | "6"
                                    | "7"
                                    | "8"
                                    | "9"
                                    | "¹"
                                    | "²"
                                    | "³"
                            )
                    {
                        return Err("guard path has an ambiguous Windows name".into());
                    }
                }
                out.push(name);
                if resolve {
                    match std::fs::symlink_metadata(&out) {
                        Ok(_) => {
                            out = std::fs::canonicalize(&out).map_err(|e| {
                                format!("guard path cannot be resolved: {}", e.kind())
                            })?
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(format!("guard path cannot be inspected: {}", e.kind()))
                        }
                    }
                }
            }
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                {
                    use std::path::Prefix;
                    if matches!(prefix.kind(), Prefix::DeviceNS(_) | Prefix::Verbatim(_)) {
                        return Err("guard path uses a device namespace".into());
                    }
                }
                out.push(prefix.as_os_str());
            }
            Component::RootDir => out.push(component.as_os_str()),
        }
    }
    Ok(out)
}
