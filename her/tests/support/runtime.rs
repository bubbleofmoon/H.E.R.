//! Disposable guarded runtime roots and the selected dependency map.
use serde_json::json;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A loopback URL nothing listens on (closed poison endpoint).
pub const CLOSED: &str = "http://127.0.0.1:1";

/// `<temp>/her-test-<label>-<pid>-<nanos>-<seq>` behind the install-root guard, with config/,
/// data/logs/, resources/ and user/dsh/. Removed on drop.
pub struct RuntimeRoot(PathBuf);

impl RuntimeRoot {
    pub fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = super::install_root::development_root(std::env::temp_dir().join(format!(
            "her-test-{label}-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        for dir in ["config", "data/logs", "resources", "user/dsh"] {
            fs::create_dir_all(path.join(dir)).expect("create runtime root directory");
        }
        Self(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for RuntimeRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for RuntimeRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `<root>/dependencies.json`; `onebot_ws` is written only when given.
pub fn write_dependencies(
    root: &Path,
    dsh: &str,
    model: &str,
    onebot: &str,
    onebot_ws: Option<&str>,
) {
    let mut map = json!({"dsh": dsh, "model": model, "onebot": onebot});
    if let Some(ws) = onebot_ws {
        map["onebot_ws"] = json!(ws);
    }
    fs::write(root.join("dependencies.json"), map.to_string()).expect("write dependencies.json");
}
