//! The one production root (#66): every production path HER resolves for a selected root lies
//! inside it, and the fixed-path layout's paths lie inside the default root.
use crate::install_root::InstallRoot;
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Removes the disposable folder on every exit, including a panic during setup.
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// A disposable production root `<temp>/her-66-<id>/qqbot` with a minimal runtime.
struct Fixture { install: InstallRoot, runtime: PathBuf, _cleanup: Cleanup }

impl Fixture {
    fn new() -> Self {
        let base = crate::install_root::development_root(
            std::env::temp_dir().join(crate::turn_ledger::mint_id("her-66-")),
        );
        let cleanup = Cleanup(base.clone());
        let root = base.join("qqbot");
        let runtime = root.join("runtime");
        for dir in ["config", "data/logs", "resources", "user/dsh", "dsh/work"] {
            std::fs::create_dir_all(runtime.join(dir)).unwrap();
        }
        std::fs::write(runtime.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
        std::fs::write(runtime.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            runtime.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        ).unwrap();
        let install = InstallRoot::new(&root).unwrap();
        Self { install, runtime, _cleanup: cleanup }
    }
    fn layout(&self) -> BTreeMap<&'static str, PathBuf> { self.install.layout().into_iter().collect() }
    fn assert_inside(&self, name: &str, path: &Path) {
        assert!(self.install.protects(path).unwrap(),
            "{name} resolves outside the production root: {}", path.display());
    }
}

#[test]
fn every_resolved_production_path_is_inside_the_selected_root() {
    let fixture = Fixture::new();
    let layout = fixture.layout();
    for (name, path) in &layout { fixture.assert_inside(name, path); }

    // The bridge and the supervisor resolve every runtime area through the selected runtime.
    let context = RuntimeContext::from_root(&fixture.runtime).unwrap();
    let runtime = context.runtime_root().unwrap();
    for (name, area) in [
        ("config", RuntimeArea::Config), ("data", RuntimeArea::Data), ("memory", RuntimeArea::Memory),
        ("logs", RuntimeArea::Logs), ("resources", RuntimeArea::Resources),
        ("dshHome", RuntimeArea::DshUser), ("dshWork", RuntimeArea::DshWork),
    ] {
        let path = context.resolve_path(area, "probe", PathIntent::ReadOptionalFile).unwrap();
        fixture.assert_inside(name, &path);
    }
    for (name, path) in [("config file", context.config_file()), ("data", context.data_dir()),
                         ("resources", context.resources_dir())] {
        fixture.assert_inside(name, path);
    }

    // The layout names the folders the runtime actually resolves (logs first: it is new).
    for (name, area, relative) in [
        ("logs", RuntimeArea::Logs, "logs/probe"),
        ("dshHome", RuntimeArea::DshUser, "probe"),
        ("dshWork", RuntimeArea::DshWork, "probe"),
    ] {
        let folder = layout.get(name).unwrap_or_else(|| panic!("the layout names {name}"));
        let expected = runtime.join(folder.strip_prefix(&layout["runtime"]).unwrap()).join("probe");
        assert_eq!(
            context.resolve_path(area, relative, PathIntent::ReadOptionalFile).unwrap(),
            expected,
            "layout {name} must be the folder the runtime resolves"
        );
    }

    // Backups: the root's own runtime backs up into the layout's backups by default.
    assert_eq!(fixture.install.backup_destination(runtime, Path::new("backups")).unwrap(), layout["backups"]);

    // Dependencies: the root's own runtime keeps them in the layout's deps.
    assert_eq!(
        crate::supervisor_dependencies::default_dependency_directory(&fixture.install, &context).unwrap(),
        layout["dependencies"]
    );

    // The logon task starts the current release on this root's runtime.
    let task = crate::scheduled_task::TaskDefinition::for_install(&fixture.install).unwrap();
    fixture.assert_inside("task command", Path::new(&task.command));
    fixture.assert_inside("task working directory", Path::new(&task.working_directory));
    for (name, path) in [("runtime", layout["runtime"].as_path()), ("install root", fixture.install.root())] {
        let spelled = crate::subprocess::external_path(path).unwrap();
        assert!(task.arguments.contains(&spelled), "task arguments name the {name}: {}", task.arguments);
    }
}

#[test]
fn fixed_path_layout_lies_inside_the_default_production_root() {
    // ReadOptionalFile on the legacy context never touches the filesystem.
    let root = Path::new(crate::legacy_path!());
    let legacy = RuntimeContext::legacy();
    let fixed = [
        legacy.config_file(), legacy.data_dir(), legacy.resources_dir(),
        Path::new(crate::memory::DB_PATH), Path::new(crate::ledger::LEDGER_FILE),
        Path::new(crate::ledger::RUST_LEDGER_FILE), Path::new(crate::send_outbox::LEGACY_OUTBOX_FILE),
        Path::new(crate::turn_ledger::LEGACY_LEDGER_FILE), Path::new(crate::slang::SLANG_FILE),
        Path::new(crate::sticker::STICKER_FILE),
    ];
    for path in fixed { assert!(path.starts_with(root), "{} is outside {}", path.display(), root.display()); }
    // The legacy DSH home stays in the profile until the owner's move; it is not checked here.
    for area in [RuntimeArea::Config, RuntimeArea::Data, RuntimeArea::Resources, RuntimeArea::Memory, RuntimeArea::Logs] {
        let path = legacy.resolve_path(area, "probe", PathIntent::ReadOptionalFile).unwrap();
        assert!(path.starts_with(root), "{area:?} resolves outside {}", root.display());
    }
    if cfg!(windows) { assert_eq!(crate::install_root::DEFAULT_INSTALL_ROOT, crate::legacy_path!()); }
}
