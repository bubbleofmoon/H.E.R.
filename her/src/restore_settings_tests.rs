// Backup and restore of the settings store (`data/settings.db`). Included in `restore::tests`, so the fixture,
// `FakeControl`, `memory` and `mutate` above are in scope. Every test name starts with `restore_settings_`, so one
// libtest filter runs the original restore cases. Credential-specific cases use the ticket219 filters.

const QUIET: &str = "/socialV2/reply/wakeQuietMs";

/// Sets `reply.wakeQuietMs` through the settings store, as the console does.
fn set_quiet(f: &Fixture, value: i64) {
    crate::settings::apply(&f.context(), "reply.wakeQuietMs", &json!(value), "administrator", "console").unwrap();
}

/// What the next use reads through the configuration loader; `None` without a stored value (the fixture's
/// `config.json` has no `socialV2`).
fn quiet(f: &Fixture) -> Option<i64> {
    f.context().load_config_document().unwrap().pointer(QUIET).and_then(Value::as_i64)
}

/// The rows of the settings history.
fn settings_audit_rows(f: &Fixture) -> i64 {
    crate::settings::StoreProbe::at(&f.0).audit_count()
}

/// Appends `count` history rows of 1 KiB each.
fn grow_settings_history(f: &Fixture, count: usize) {
    let text = "x".repeat(1024);
    let rows: Vec<crate::settings::AuditRow> = (0..count)
        .map(|i| crate::settings::AuditRow {
            at_ms: i as i64,
            actor: "fixture",
            source: "fixture",
            scope: "global",
            key: "reply.wakeQuietMs",
            old: None,
            new: Some(text.clone()),
            request: format!("grow-{i}"),
        })
        .collect();
    crate::settings::StoreProbe::at(&f.0).insert_audit(&rows);
}

/// This process's open handles on the settings store and its sidecars (Linux `/proc/self/fd`). Other hosts report
/// none; there the restore itself fails while this process holds the store open.
fn settings_handles(f: &Fixture) -> usize {
    #[cfg(target_os = "linux")]
    {
        let store = fs::canonicalize(f.0.join("data")).unwrap().join("settings.db");
        let store = store.to_string_lossy().into_owned();
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.to_string_lossy().starts_with(&store))
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = f;
        0
    }
}

/// The fixture's backup destination.
fn destination(f: &Fixture) -> PathBuf {
    crate::backup::Settings::from_config_resolved(&f.0, &f.context().load_config().unwrap()).unwrap().destination
}

/// The archives published in the fixture's backup destination.
fn archives(f: &Fixture) -> Vec<String> {
    match fs::read_dir(destination(f)) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".zip"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn restore_settings_full_restore_brings_the_store_back_and_memory_only_leaves_it_alone() {
    for memory_only in [false, true] {
        let f = Fixture::new();
        set_quiet(&f, 4000);
        // 3000 rows of 1 KiB: the store spans several online-backup steps (256 pages each).
        grow_settings_history(&f, 3000);
        let archive = f.backup();
        assert!(
            crate::backup::verify_archive(&archive, &|| Ok(())).unwrap().files.contains_key("settings.db"),
            "the archive holds settings.db"
        );
        mutate(&f);
        set_quiet(&f, 5000);
        assert_eq!(settings_audit_rows(&f), 3002);
        // Reads through the configuration loader and the settings page hold no handle on the store between reads; on
        // Windows an open handle would refuse the replacement below (#146 F1).
        assert_eq!(quiet(&f), Some(5000));
        assert_eq!(crate::settings::view(&f.context()).unwrap()["settings"][0]["stored"], 5000);
        assert_eq!(settings_handles(&f), 0, "a settings read left a handle on the store open");
        let plan = preview(&f.context(), &archive, memory_only).unwrap();
        let listed = plan["replaces"].as_array().unwrap().iter().any(|v| v == "data/settings.db");
        assert_eq!(listed, !memory_only, "only a full restore replaces settings.db: {plan}");
        let current = fs::read(f.0.join("data/settings.db")).unwrap();
        let mut control = FakeControl { leave_settings_sidecars: true, ..Default::default() };
        let result = execute_with(
            &f.context(),
            &archive,
            memory_only,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut control,
        )
        .unwrap();
        if memory_only {
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = f.0.join(format!("data/settings.db{suffix}"));
                assert_eq!(fs::read(&sidecar).unwrap(), b"stale settings sidecar", "memory-only keeps settings.db{suffix}");
                fs::remove_file(sidecar).unwrap();
            }
            assert_eq!(
                fs::read(f.0.join("data/settings.db")).unwrap(),
                current,
                "memory-only keeps every byte of settings.db"
            );
        } else {
            // Reading the restored store for scope notices can create fresh SQLite sidecars.
            assert_eq!(control.settings_sidecars_after_replace, Some([false; 3]), "full restore removes all settings sidecars at replacement");
        }
        assert_eq!(memory(&f), "old memory");
        assert_eq!(control.stops, 1);
        assert_eq!(quiet(&f), Some(if memory_only { 5000 } else { 4000 }));
        assert_eq!(
            settings_audit_rows(&f),
            if memory_only { 3002 } else { 3001 },
            "a full restore brings the archived history back"
        );
        let pre = Path::new(result["preRestoreBackup"].as_str().unwrap());
        assert_eq!(
            crate::backup::verify_archive(pre, &|| Ok(())).unwrap().files.contains_key("settings.db"),
            !memory_only,
            "the pre-restore backup is scoped to the restore"
        );
    }
}

#[test]
fn restore_settings_archive_without_the_store_restores_and_only_a_full_restore_removes_it() {
    for memory_only in [false, true] {
        let f = Fixture::new();
        // No store yet: the archive is laid out as one from a release before settings.db.
        let archive = f.backup();
        assert!(!crate::backup::verify_archive(&archive, &|| Ok(())).unwrap().files.contains_key("settings.db"));
        let stale = preview(&f.context(), &archive, memory_only).unwrap();
        set_quiet(&f, 5000);
        let plan = preview(&f.context(), &archive, memory_only).unwrap();
        let removes = plan["removes"].as_array().unwrap().iter().any(|v| v == "data/settings.db");
        assert_eq!(removes, !memory_only, "only a full restore removes settings.db: {plan}");
        if memory_only {
            assert_eq!(
                stale["confirmation"], plan["confirmation"],
                "a memory-only plan does not depend on the settings store"
            );
        } else {
            // The store appeared after the preview, so the confirmed plan no longer matches.
            let error = execute_with(
                &f.context(),
                &archive,
                false,
                stale["confirmation"].as_str().unwrap(),
                "fixture-admin",
                &mut FakeControl::default(),
            )
            .unwrap_err();
            assert!(error.contains("preview again"), "{error}");
            assert_eq!(quiet(&f), Some(5000));
        }
        let current = fs::read(f.0.join("data/settings.db")).unwrap();
        let mut control = FakeControl { leave_settings_sidecars: true, ..Default::default() };
        let result = execute_with(
            &f.context(),
            &archive,
            memory_only,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut control,
        )
        .unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = f.0.join(format!("data/settings.db{suffix}"));
            if memory_only {
                assert_eq!(fs::read(&sidecar).unwrap(), b"stale settings sidecar", "memory-only keeps settings.db{suffix}");
                fs::remove_file(sidecar).unwrap();
            } else {
                assert!(!sidecar.exists(), "a full restore removes settings.db{suffix}");
            }
        }
        if memory_only {
            assert_eq!(fs::read(f.0.join("data/settings.db")).unwrap(), current);
            assert_eq!(quiet(&f), Some(5000), "memory-only keeps the current settings");
        } else {
            assert!(!f.0.join("data/settings.db").exists(), "the archive had no store, so a full restore removes it");
            assert_eq!(quiet(&f), None, "the archived config.json applies as it is");
        }
        assert_eq!(memory(&f), "old memory");
        let pre = Path::new(result["preRestoreBackup"].as_str().unwrap());
        assert_eq!(
            crate::backup::verify_archive(pre, &|| Ok(())).unwrap().files.contains_key("settings.db"),
            !memory_only,
            "the pre-restore backup keeps the current store only for full restore"
        );
    }
}

#[test]
fn restore_settings_reports_and_preserves_an_inherited_rollout_gap() {
    let f = Fixture::new();
    let context = f.context();
    let config = f.0.join("config/config.json");
    let archived = json!({
        "consoleToken": "fixture",
        "backends": {"mcpRustBridge": true},
        "allow": {"groups": [7001], "private": []},
        "rollout": {"rustGroups": ["group:7001", "private:7002"]}
    });
    fs::write(&config, archived.to_string()).unwrap();
    set_quiet(&f, 4000);
    let archive = f.backup();
    crate::settings::reset(&context, "rollout.rustGroups", "administrator").unwrap();
    for memory_only in [true, false] {
        let plan = preview(&context, &archive, memory_only).unwrap();
        let result = execute_with(
            &context,
            &archive,
            memory_only,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut FakeControl::default(),
        )
        .unwrap();
        let effective = context.load_config().unwrap();
        if memory_only {
            assert_eq!(effective["rollout"]["rustGroups"], json!([]));
            assert!(result.get("scopeNotices").is_none());
        } else {
            assert_eq!(effective["rollout"]["rustGroups"], archived["rollout"]["rustGroups"]);
            assert_eq!(effective["allow"]["private"], json!([]), "full restore does not widen inherited access");
            assert_eq!(crate::watchdog::rollout_scope_gaps(&effective).not_allowed, ["private:7002"]);
            assert!(result["scopeNotices"].as_array().is_some_and(|lines| lines.iter().any(
                |line| line.as_str().is_some_and(|line| line.contains("private:7002"))
            )), "full restore reports the inherited rollout gap: {result}");
        }
    }
}

#[test]
fn restore_settings_failed_replacement_or_health_rolls_the_store_back() {
    for during_replace in [true, false] {
        let f = Fixture::new();
        set_quiet(&f, 4000);
        let archive = f.backup();
        set_quiet(&f, 5000);
        let plan = preview(&f.context(), &archive, false).unwrap();
        let mut control = FakeControl {
            unhealthy: !during_replace,
            fail_relative: during_replace.then_some("settings.db"),
            ..Default::default()
        };
        let error = execute_with(
            &f.context(),
            &archive,
            false,
            plan["confirmation"].as_str().unwrap(),
            "rollback-admin",
            &mut control,
        )
        .unwrap_err();
        assert!(error.contains("pre-restore backup restored"), "{error}");
        let settings_writes =
            control.replaced.iter().filter(|label| label.as_str() == "data/settings.db").count();
        assert_eq!(settings_writes, 2, "the restore and its rollback both replace settings.db: {:?}", control.replaced);
        assert_eq!(control.stops, 2);
        assert_eq!(quiet(&f), Some(5000), "the rollback brings the current settings back");
        let temps: Vec<String> = fs::read_dir(f.0.join("data"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "an interrupted replacement leaves no temporary file: {temps:?}");
    }
}

#[test]
fn restore_settings_store_is_replaced_while_another_reader_keeps_reading() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    let (stop, reads) = (AtomicBool::new(false), AtomicUsize::new(0));
    let result = std::thread::scope(|scope| {
        // The supervisor keeps running through a restore and reads settings through the configuration loader.
        scope.spawn(|| {
            let context = f.context();
            while !stop.load(SeqCst) {
                let _ = context.load_config();
                reads.fetch_add(1, SeqCst);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let result = execute_with(
            &f.context(),
            &archive,
            false,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut FakeControl::default(),
        );
        stop.store(true, SeqCst);
        result
    });
    result.unwrap();
    assert!(reads.load(SeqCst) > 0, "the reader ran during the restore");
    assert_eq!(quiet(&f), Some(4000));
}

#[test]
fn restore_settings_removal_retries_only_windows_sharing_violations() {
    let later = || Instant::now() + Duration::from_secs(5);
    for (windows, errors, calls, ok) in [
        (true, vec![32, 33, 5], 4, true),
        (true, vec![2], 1, false),
        (false, vec![32], 1, false),
    ] {
        let mut left = errors.clone().into_iter();
        let mut count = 0;
        let result = retry_open_file(windows, later(), || {
            count += 1;
            left.next().map_or(Ok(()), |code| Err(std::io::Error::from_raw_os_error(code)))
        });
        assert_eq!((count, result.is_ok()), (calls, ok), "{windows} {errors:?}");
    }
    let mut count = 0;
    let result = retry_open_file(true, Instant::now(), || {
        count += 1;
        Err(std::io::Error::from_raw_os_error(32))
    });
    assert_eq!((count, result.is_err()), (1, true), "a passed deadline tries once");
}

#[test]
fn restore_settings_unusable_store_fails_the_backup_and_refuses_a_restore_before_stopping() {
    let mut cases = vec!["hard link", "not a database"];
    if cfg!(unix) {
        cases.push("symlink");
    }
    for case in cases {
        let f = Fixture::new();
        let archive = f.backup();
        let published = archives(&f);
        let store = f.0.join("data/settings.db");
        match case {
            "hard link" => {
                set_quiet(&f, 5000);
                fs::hard_link(&store, f.0.join("data/settings-second-name.db")).unwrap();
            }
            "not a database" => fs::write(&store, b"not a SQLite database").unwrap(),
            _ => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(f.0.join("config/config.json"), &store).unwrap();
            }
        }
        let error = crate::backup::create(&f.context(), &|| Ok(())).expect_err(case);
        assert!(error.contains("settings"), "{case}: {error}");
        assert_eq!(archives(&f), published, "{case}: a failed backup publishes nothing");

        let full_plan = preview(&f.context(), &archive, false);
        let full_confirmation = full_plan
            .as_ref()
            .map(|p| p["confirmation"].as_str().unwrap().to_string())
            .unwrap_or_default();
        let mut full_control = FakeControl::default();
        let full_error = execute_with(
            &f.context(),
            &archive,
            false,
            &full_confirmation,
            "fixture-admin",
            &mut full_control,
        )
        .expect_err(case);
        assert!(full_error.contains("settings"), "{case}: {full_error}");
        if case == "symlink" {
            assert_eq!(full_control.stops, 0, "{case}: symlink refused before stopping");
        } else {
            assert_eq!(full_control.stops, 1, "{case}: full pre-restore backup runs after quiesce");
            assert_eq!(full_control.writes, 0, "{case}: nothing replaced");
            assert_eq!(full_control.restarts, 1, "{case}: owners restarted after refusal");
            assert!(
                full_control
                    .alerts
                    .iter()
                    .any(|alert| alert.contains("Restore refused after stopping")),
                "{case}: {:?}",
                full_control.alerts
            );
        }

        let mem_plan = preview(&f.context(), &archive, true).unwrap();
        let mut mem_control = FakeControl::default();
        execute_with(
            &f.context(),
            &archive,
            true,
            mem_plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut mem_control,
        )
        .unwrap();
        assert_eq!(memory(&f), "old memory");
    }
}

#[test]
fn restore_settings_full_refuses_an_archive_from_a_newer_release() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    let future = rewrite_settings_archive(&f, &archive, "future-settings.zip", "PRAGMA user_version=4;");
    set_quiet(&f, 5000);
    mutate(&f);
    let store = crate::settings::StoreProbe::at(&f.0);
    let before = fs::read(store.path()).unwrap();
    let config = fs::read(f.0.join("config/config.json")).unwrap();
    assert!(preview(&f.context(), &future, false).unwrap_err().contains("newer"));
    let mut control = FakeControl::default();
    assert!(execute_with(&f.context(), &future, false, "unused", "fixture", &mut control)
        .unwrap_err().contains("newer"));
    assert_eq!(control.stops, 0);
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert_eq!(fs::read(f.0.join("config/config.json")).unwrap(), config);
    assert_eq!(memory(&f), "current memory");

    let plan = preview(&f.context(), &future, true).unwrap();
    assert_eq!(plan["warnings"], json!([]));
    execute_with(
        &f.context(), &future, true, plan["confirmation"].as_str().unwrap(), "fixture", &mut control,
    ).unwrap();
    assert_eq!(memory(&f), "old memory");
    assert_eq!(fs::read(store.path()).unwrap(), before);
}

#[test]
fn restore_settings_backup_refuses_a_snapshot_from_a_newer_release() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let store = crate::settings::StoreProbe::at(&f.0);
    store.edit("PRAGMA user_version=4;");
    let before = fs::read(store.path()).unwrap();
    assert!(crate::backup::create(&f.context(), &|| Ok(())).unwrap_err().contains("newer"));
    assert!(archives(&f).is_empty());
    assert_eq!(fs::read(store.path()).unwrap(), before);
}

#[test]
fn restore_settings_payload_names_never_include_its_sidecars() {
    assert!(matches!(location("settings.db"), Ok((RuntimeArea::Data, ref relative)) if relative == "settings.db"));
    for name in [
        "settings.db-wal",
        "settings.db-shm",
        "settings.db-journal",
        "state/settings.db",
        "locks/settings-store.lock",
        "console-auth-generation",
        "state/console-auth-generation",
    ] {
        assert!(location(name).is_err(), "{name}");
    }
}

#[test]
fn restore_settings_backup_budget_ending_in_the_settings_snapshot_publishes_nothing() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let backups = destination(&f);
    // The job's check (budget, cancel, held jobs) runs in every online-backup step. This one fails once the settings
    // snapshot has started, as a job timeout reached there would.
    let in_settings_snapshot = || {
        let started = fs::read_dir(&backups)
            .map(|entries| entries.filter_map(Result::ok).any(|entry| entry.path().join("settings.db").exists()))
            .unwrap_or(false);
        if started {
            Err("backup timed out".to_string())
        } else {
            Ok(())
        }
    };
    let error = crate::backup::create(&f.context(), &in_settings_snapshot)
        .expect_err("the budget ran out in the settings snapshot");
    assert_eq!(error, "backup timed out");
    let left: Vec<String> = fs::read_dir(&backups)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "no archive and no staging folder is left: {left:?}");
}

#[test]
fn restore_settings_archive_with_a_damaged_store_is_refused() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    let path = rewrite_archive(&archive, "damaged-settings.zip", "settings.db", b"not a SQLite database");
    let error = crate::backup::verify_archive(&path, &|| Ok(())).unwrap_err();
    assert!(error.contains("integrity check"), "{error}");
    for memory_only in [false, true] {
        assert!(preview(&f.context(), &path, memory_only).is_err(), "a damaged archive is refused for every scope");
    }
}

fn rewrite_archive(archive: &Path, name: &str, payload: &str, bytes: &[u8]) -> PathBuf {
    let mut source = zip::ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
    let mut files = Vec::new();
    for index in 0..source.len() {
        let mut entry = source.by_index(index).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        files.push((entry.name().to_string(), bytes));
    }
    drop(source);
    files.retain(|(name, _)| name != payload);
    files.push((payload.to_string(), bytes.to_vec()));
    let (_, manifest) = files.iter_mut().find(|(name, _)| name == "manifest.json").unwrap();
    let mut value: Value = serde_json::from_slice(manifest).unwrap();
    value["files"][payload] =
        json!({"size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))});
    *manifest = serde_json::to_vec(&value).unwrap();
    let path = archive.with_file_name(name);
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    for (name, bytes) in files {
        writer.start_file(name, zip::write::FileOptions::default()).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
    path
}

fn extract_settings(f: &Fixture, archive: &Path) -> PathBuf {
    let path = f.0.join("extracted-settings.db");
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    std::io::copy(&mut zip.by_name("settings.db").unwrap(), &mut File::create(&path).unwrap()).unwrap();
    path
}

fn rewrite_settings_archive(f: &Fixture, archive: &Path, name: &str, sql: &str) -> PathBuf {
    let db = extract_settings(f, archive);
    rusqlite::Connection::open(&db).unwrap().execute_batch(sql).unwrap();
    rewrite_archive(archive, name, "settings.db", &fs::read(db).unwrap())
}

#[test]
fn restore_settings_backup_while_the_console_writes_settings() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::sync::Mutex;
    let f = Fixture::new();
    set_quiet(&f, 1000);
    let stop = AtomicBool::new(false);
    // Every value a console write stored, the initial 1000 included; the restored value must be one of them.
    let stored = Mutex::new(vec![1000_i64]);
    let completed = AtomicUsize::new(0);
    let (archive, before, after) = std::thread::scope(|scope| {
        scope.spawn(|| {
            let context = f.context();
            let mut value = 1000;
            while !stop.load(SeqCst) {
                value = if value >= 30_000 { 1000 } else { value + 1 };
                if crate::settings::apply(&context, "reply.wakeQuietMs", &json!(value), "administrator", "console")
                    .is_ok()
                {
                    stored.lock().unwrap().push(value);
                    completed.fetch_add(1, SeqCst);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let before = completed.load(SeqCst);
        let archive = crate::backup::create(&f.context(), &|| Ok(()));
        let after = completed.load(SeqCst);
        stop.store(true, SeqCst);
        (archive, before, after)
    });
    let archive = archive.unwrap();
    assert!(after > before, "no console write completed during the backup ({before} before, {after} after)");
    assert!(
        crate::backup::verify_archive(&archive, &|| Ok(())).unwrap().files.contains_key("settings.db"),
        "the archive holds a consistent copy of settings.db"
    );
    let plan = preview(&f.context(), &archive, false).unwrap();
    execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut FakeControl::default(),
    )
    .unwrap();
    let restored = quiet(&f);
    assert!(
        restored.is_some_and(|value| stored.lock().unwrap().contains(&value)),
        "{restored:?} was never written by the console"
    );
}

/// The restore stopped the owners, could not take the store gate, and refused with nothing changed: the owners are
/// asked to run again, no rollback ran, and no file was replaced.
fn assert_refused_without_rollback(f: &Fixture, control: &FakeControl, error: &str, case: &str) {
    assert!(error.contains("settings"), "{case}: {error}");
    assert_eq!(control.stops, 1, "{case}: a refused restore never runs a rollback stop");
    assert_eq!(control.writes, 0, "{case}: nothing was replaced");
    assert_eq!(control.restarts, 1, "{case}: the stopped owners are asked to start again");
    assert_eq!(control.alerts.len(), 1, "{case}: {:?}", control.alerts);
    assert!(!control.alerts[0].contains("rolled back"), "{case}: {:?}", control.alerts);
    assert_eq!(quiet(f), Some(5000), "{case}: nothing changed");
    assert!(
        matches!(crate::lifecycle::DesiredStore::new(f.0.join("data/lifecycle")).read(), crate::lifecycle::DesiredRead::Present(r) if r.state == crate::lifecycle::DesiredState::Running),
        "{case}: the desired state is running again"
    );
}

#[test]
fn restore_settings_held_store_gate_refuses_the_restore_after_the_stop_without_a_rollback() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    // Another process holds the store gate, as a standalone MCP server does while it writes.
    let gate = f.context().open_locked_file(RuntimeArea::Data, "locks/settings-store.lock").unwrap();
    let mut control = FakeControl::default();
    let error = execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut control,
    )
    .unwrap_err();
    drop(gate);
    assert_refused_without_rollback(&f, &control, &error, "gate held before the restore");
}

#[test]
fn restore_settings_writer_that_takes_the_gate_during_the_stop_refuses_the_restore_without_a_rollback() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    // A writer gets the gate while the restore waits for the owners: the gate is not the restore's yet.
    let mut control = FakeControl { writer_takes_gate_during_stop: true, ..Default::default() };
    let error = execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut control,
    )
    .unwrap_err();
    assert!(control.writer_hold.is_some(), "the writer took the gate during the stop");
    assert_refused_without_rollback(&f, &control, &error, "gate taken during the stop");
}

#[test]
fn restore_settings_console_write_is_refused_while_the_store_is_replaced() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    // The probe runs at the first replacement, after the sidecar cleanup, where the restore holds the gate.
    let mut control = FakeControl { probe_settings_write: true, ..Default::default() };
    execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut control,
    )
    .unwrap();
    assert_eq!(
        control.settings_probe,
        Some(Err(503)),
        "a settings write between the sidecar cleanup and the replacement is refused"
    );
    assert_eq!(quiet(&f), Some(4000));
}

#[test]
fn restore_settings_gate_that_is_not_a_plain_file_fails_the_backup_and_refuses_the_restore() {
    for case in ["hard link", "directory", "hard link without the store"] {
        let f = Fixture::new();
        set_quiet(&f, 4000);
        let archive = f.backup();
        set_quiet(&f, 5000);
        let plan = preview(&f.context(), &archive, false).unwrap();
        // The gate lives in the data area (`data/locks/`). Create the plain file first when it is absent, so the case
        // below builds its bad gate deliberately rather than depending on the writes that ran before.
        let gate = f.0.join("data/locks/settings-store.lock");
        fs::create_dir_all(gate.parent().unwrap()).unwrap();
        if !gate.exists() {
            fs::write(&gate, b"").unwrap();
        }
        if case == "directory" {
            fs::remove_file(&gate).unwrap();
            fs::create_dir(&gate).unwrap();
        } else {
            fs::hard_link(&gate, f.0.join("data/locks/settings-store-second-name.lock")).unwrap();
        }
        if case == "hard link without the store" {
            for name in ["settings.db", "settings.db-wal", "settings.db-shm", "settings.db-journal"] {
                let _ = fs::remove_file(f.0.join("data").join(name));
            }
        }
        let error = crate::backup::create(&f.context(), &|| Ok(())).expect_err(case);
        assert!(error.contains("settings"), "{case}: {error}");
        let mut control = FakeControl::default();
        let error = execute_with(
            &f.context(),
            &archive,
            false,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut control,
        )
        .expect_err(case);
        assert!(error.contains("settings"), "{case}: {error}");
        assert_eq!(control.stops, 0, "{case}: refused before anything stops");
    }
}

#[test]
fn restore_settings_store_gate_is_not_held_while_the_restore_waits_for_the_owners_and_task_gates() {
    // A settings writer holds the configuration gate while it waits for the store gate, and a queued console task holds
    // its task gate while it waits for the configuration gate, so a restore that holds the store gate while it waits
    // for the task gates closes a cycle. The gate comes after the stop, for the restore and for its rollback.
    for unhealthy in [false, true] {
        let f = Fixture::new();
        set_quiet(&f, 4000);
        let archive = f.backup();
        set_quiet(&f, 5000);
        let plan = preview(&f.context(), &archive, false).unwrap();
        let mut control = FakeControl { unhealthy, ..Default::default() };
        let result = execute_with(
            &f.context(),
            &archive,
            false,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut control,
        );
        assert_eq!(result.is_err(), unhealthy);
        assert_eq!(
            control.store_free_at_stop,
            vec![true; if unhealthy { 2 } else { 1 }],
            "the store gate was held while a stop waited (unhealthy: {unhealthy})"
        );
        assert_eq!(quiet(&f), Some(if unhealthy { 5000 } else { 4000 }));
    }
}

#[test]
fn restore_settings_store_gate_is_released_before_conversation_reconciliation() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    // Reconciliation takes configuration gates, which a settings writer takes before the store gate, so the store
    // gate must be free by then.
    let mut control = FakeControl { probe_store_replaced: true, ..Default::default() };
    execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut control,
    )
    .unwrap();
    assert_eq!(
        control.settings_after_replace,
        Some(Ok(())),
        "a settings write after the replacement and before reconciliation is accepted"
    );
}

#[test]
fn ticket219_private_credentials_use_owner_only_files() {
    let f = Fixture::new();
    set_quiet(&f, 4100);
    let archive = f.backup();
    let restored = f.0.join("data/settings.db");
    assert!(credential_file_is_private(&restored, true), "settings.db has an unexpected ACL or mode");
    assert!(credential_file_is_private(&archive, false), "published backup is not owner-only");
    let plan = preview(&f.context(), &archive, false).unwrap();
    execute_with(
        &f.context(), &archive, false, plan["confirmation"].as_str().unwrap(), "fixture-admin", &mut FakeControl::default(),
    ).unwrap();
    assert!(credential_file_is_private(&restored, true), "restored settings.db has an unexpected ACL or mode");
}

#[cfg(unix)]
fn credential_file_is_private(path: &Path, _settings: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600
}

#[cfg(windows)]
fn credential_file_is_private(path: &Path, settings: bool) -> bool {
    let policy = if settings {
        crate::guarded_fs::AclPolicy::OwnerSystemAdmins
    } else {
        crate::guarded_fs::AclPolicy::OwnerOnly
    };
    crate::guarded_fs::verify_file_private(path, policy)
}

#[cfg(windows)]
#[test]
fn ticket219_private_settings_acl_leaves_the_data_directory_unchanged() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetAce, GetLengthSid, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACL,
        DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    type Grant = (u8, u8, u32, Vec<u8>);

    fn dacl(path: &Path) -> (bool, Vec<Grant>) {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: terminated path and writable outputs. The descriptor owns the ACL until LocalFree.
        let result = unsafe {
            assert_eq!(GetNamedSecurityInfoW(
                wide.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(), std::ptr::null_mut(), &mut acl, std::ptr::null_mut(), &mut descriptor,
            ), 0);
            assert_ne!(GetSecurityDescriptorControl(descriptor, &mut control, &mut revision), 0);
            assert!(!acl.is_null());
            let mut entries = Vec::new();
            for index in 0..u32::from((*acl).AceCount) {
                let mut ace = std::ptr::null_mut();
                assert_ne!(GetAce(acl, index, &mut ace), 0);
                // The fixture ACLs contain ordinary allow/deny ACEs with a mask followed by a SID.
                let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                assert!(matches!(entry.Header.AceType, 0 | 1));
                let sid = std::ptr::addr_of!(entry.SidStart).cast::<u8>();
                let len = GetLengthSid(sid.cast_mut().cast()) as usize;
                entries.push((entry.Header.AceType, entry.Header.AceFlags, entry.Mask,
                    std::slice::from_raw_parts(sid, len).to_vec()));
            }
            let protected = control & SE_DACL_PROTECTED != 0;
            LocalFree(descriptor);
            (protected, entries)
        };
        result
    }

    let f = Fixture::new();
    let data = f.0.join("data");
    let sibling = data.join("acl-sibling.json");
    fs::write(&sibling, b"{}").unwrap();
    let before = (dacl(&data), dacl(&sibling));
    set_quiet(&f, 4100);
    assert_eq!((dacl(&data), dacl(&sibling)), before, "settings changed the runtime data ACL");
    let (protected, mut entries) = dacl(&data.join("settings.db"));
    entries.sort_unstable();
    let mut sids = vec![
        crate::process_identity::current_user().unwrap(),
        vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0], // SYSTEM
        vec![1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0], // Administrators
    ];
    sids.sort_unstable();
    sids.dedup();
    let expected: Vec<Grant> = sids.into_iter().map(|sid| (0, 0, FILE_ALL_ACCESS, sid)).collect();
    assert!(protected && entries == expected, "settings lost documented Windows grants");
}

#[test]
fn ticket219_core_pre_credential_restore_preview_warns_about_both_keys() {
    let f = Fixture::new();
    let without_settings = f.backup();
    set_quiet(&f, 4100);
    let ordinary_settings = f.backup();
    let context = f.context();
    for (setting, key) in [
        (crate::settings::WEB_SEARCH_API_KEY, "fixture-search-preview-key"),
        (crate::settings::CLI_MODEL_API_KEY, "fixture-cli-preview-key"),
    ] {
        crate::settings::apply(&context, setting.key, &json!(key), "fixture-admin", "console").unwrap();
    }
    let with_credentials = f.backup();
    for archive in [&without_settings, &ordinary_settings] {
        let full = preview(&context, archive, false).unwrap();
        assert_eq!(
            full["warnings"],
            json!([
                "整库覆写恢复将会抹掉本地保存的联网搜索密钥、直连服务商密钥与模型接口密钥。",
                "完全恢复会清除管理员密码，恢复后需要首次设置密码，并退出所有浏览器会话。",
            ])
        );
        assert_eq!(preview(&context, archive, true).unwrap()["warnings"], json!([]));
    }
    assert_eq!(
        preview(&context, &with_credentials, false).unwrap()["warnings"],
        json!(["完全恢复会清除管理员密码，恢复后需要首次设置密码，并退出所有浏览器会话。"])
    );
    assert_eq!(preview(&context, &with_credentials, true).unwrap()["warnings"], json!([]));
}

#[test]
fn ticket219_core_full_restore_recovers_the_credential_and_memory_only_keeps_it() {
    use crate::settings::{CLI_MODEL_API_KEY, secret_value};
    let f = Fixture::new();
    let context = f.context();
    let store = |key: &str| {
        crate::settings::apply(&context, CLI_MODEL_API_KEY.key, &json!(key), "fixture-admin", "console").unwrap();
    };
    store("fixture-archived-credential");
    let archive = f.backup();
    store("fixture-current-credential");
    let endpoint = context.dependency_url("model").unwrap();
    for (memory_only, expected) in [(true, "fixture-current-credential"), (false, "fixture-archived-credential")] {
        let plan = preview(&context, &archive, memory_only).unwrap();
        execute_with(
            &context, &archive, memory_only, plan["confirmation"].as_str().unwrap(), "fixture-admin", &mut FakeControl::default(),
        ).unwrap();
        assert_eq!(secret_value(&context, &CLI_MODEL_API_KEY, endpoint).unwrap().as_deref(), Some(expected), "restore selected the wrong credential");
    }
}

const AUTH_PASSWORD: &str = "console.administratorPassword";
const ARCHIVED_PASSWORD: &str = "fixture archived password";
const CURRENT_PASSWORD: &str = "fixture current password";
const SEED_WAIT: &str =
    "INSERT INTO console_auth_state VALUES (1, randomblob(32), 5, 60001, 1)
     ON CONFLICT(id) DO UPDATE SET consecutive_failures=5, deadline_ms=60001, clock_high_water_ms=1;";
const SEED_SESSION: &str =
    "INSERT INTO console_browser_sessions VALUES
     (randomblob(32), 'http://her.localhost:4567', randomblob(32), randomblob(32), randomblob(32), 0, 0, 1, 1);";

fn set_auth_password(f: &Fixture, password: &str) {
    crate::console_auth::first_run(&f.context(), "fixture", password, "her-cli").unwrap();
}

fn stored_auth_password(f: &Fixture) -> Option<crate::console_auth::AdministratorPasswordHash> {
    let text = crate::settings::StoreProbe::at(&f.0).value(AUTH_PASSWORD)?;
    let phc: String = serde_json::from_str(&text).unwrap();
    Some(crate::console_auth::AdministratorPasswordHash::parse(&phc).unwrap())
}

fn auth_restore(
    f: &Fixture, archive: &Path, memory_only: bool, control: &mut dyn Control,
) -> Result<Value, String> {
    let context = f.context();
    let plan = preview(&context, archive, memory_only).unwrap();
    execute_with(
        &context, archive, memory_only, plan["confirmation"].as_str().unwrap(), "fixture", control,
    )
}

#[test]
fn console_auth_backup_excludes_sessions_and_wait() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let store = crate::settings::StoreProbe::at(&f.0);
    store.edit(SEED_WAIT);
    store.edit(SEED_SESSION);
    let marker = f.0.join("data/console-auth-generation");
    fs::write(&marker, [7; 32]).unwrap();
    let session_id: Vec<u8> = store.open()
        .query_row("SELECT id_digest FROM console_browser_sessions", [], |row| row.get(0)).unwrap();
    let before = fs::read(store.path()).unwrap();
    let archive = f.backup();
    assert_eq!(fs::read(store.path()).unwrap(), before, "the source stays read-only");
    assert_eq!(fs::read(&marker).unwrap(), [7; 32]);
    assert!(!crate::backup::verify_archive(&archive, &|| Ok(())).unwrap().files
        .contains_key("state/console-auth-generation"));
    let path = extract_settings(&f, &archive);
    let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let session_count: i64 = conn.query_row("SELECT count(*) FROM console_browser_sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(session_count, 0, "archive must contain no browser sessions");
    let state_count: i64 = conn.query_row("SELECT count(*) FROM console_auth_state", [], |r| r.get(0)).unwrap();
    assert_eq!(state_count, 0, "archive must contain no durable throttle or clock rows");
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(journal, "delete", "the archived store is standalone");
    let phc: String = conn.query_row("SELECT value FROM settings WHERE key=?1", [AUTH_PASSWORD], |r| r.get(0)).unwrap();
    assert_eq!(Some(phc), store.value(AUTH_PASSWORD));
    let markers: (String, String) = conn.query_row(
        "SELECT old_value, new_value FROM settings_audit WHERE key=?1", [AUTH_PASSWORD],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(markers, ("\"(hidden)\"".to_string(), "\"(hidden)\"".to_string()));
    assert!(!fs::read(&path).unwrap().windows(session_id.len()).any(|bytes| bytes == session_id.as_slice()));
    assert!(preview(&f.context(), &archive, false).is_ok());
}

#[test]
fn console_auth_restore_rejects_operational_rows() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let good = f.backup();
    for (name, sql) in [("sessions.zip", SEED_SESSION), ("throttle.zip", SEED_WAIT)] {
        let archive = rewrite_settings_archive(&f, &good, name, sql);
        assert!(preview(&f.context(), &archive, false).is_err(),
            "full preview must refuse a correctly hashed archive containing operational rows");
        let mut control = FakeControl::default();
        assert!(execute_with(&f.context(), &archive, false, "unused", "fixture", &mut control).is_err());
        assert_eq!(control.stops, 0);
        assert!(preview(&f.context(), &archive, true).is_ok());
    }
}

#[test]
fn console_auth_archive_rejects_an_unsupported_password_and_a_generation_payload() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let good = f.backup();
    for (name, value) in [("null-password.zip", "null"), ("invalid-password.zip", "\"bad PHC\"")] {
        let sql = format!("UPDATE settings SET value='{value}' WHERE key='{AUTH_PASSWORD}'");
        let archive = rewrite_settings_archive(&f, &good, name, &sql);
        assert!(preview(&f.context(), &archive, false).unwrap_err().contains("administrator password"));
        assert!(preview(&f.context(), &archive, true).is_ok());
    }
    let injected = rewrite_archive(&good, "generation.zip", "state/console-auth-generation", &[7; 32]);
    for memory_only in [false, true] {
        assert!(preview(&f.context(), &injected, memory_only).is_err());
    }
}

#[test]
fn console_auth_full_restore_warns() {
    let f = Fixture::new();
    let without_hash = f.backup();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let with_hash = f.backup();
    for (archive, expected) in [
        (&with_hash, "完全恢复会使用备份中的管理员密码，并退出所有浏览器会话。"),
        (&without_hash, "完全恢复会清除管理员密码，恢复后需要首次设置密码，并退出所有浏览器会话。"),
    ] {
        let full = preview(&f.context(), archive, false).unwrap();
        assert!(full["warnings"].as_array().unwrap().contains(&json!(expected)),
            "full preview must explain the archived password outcome");
        assert!(!full.to_string().contains("argon2id"));
        assert_eq!(preview(&f.context(), archive, true).unwrap()["warnings"], json!([]));
    }
}

#[test]
fn console_auth_full_restore_reinstates_archived_credential() {
    let f = Fixture::new();
    let without_store = f.backup();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let archive = f.backup();
    crate::console_auth::reset_administrator_password(&f.context()).unwrap();
    let unset = f.backup();
    crate::console_auth::reset_administrator_password(&f.context()).unwrap();
    set_auth_password(&f, CURRENT_PASSWORD);
    auth_restore(&f, &archive, false, &mut FakeControl::default()).unwrap();
    let hash = stored_auth_password(&f).unwrap();
    assert!(hash.verify(ARCHIVED_PASSWORD).unwrap(), "restored hash must verify the archived password");
    assert!(!hash.verify(CURRENT_PASSWORD).unwrap());
    auth_restore(&f, &unset, false, &mut FakeControl::default()).unwrap();
    assert!(stored_auth_password(&f).is_none(), "a restored store without a hash must reopen first-run");
    set_auth_password(&f, CURRENT_PASSWORD);
    auth_restore(&f, &without_store, false, &mut FakeControl::default()).unwrap();
    assert!(!f.0.join("data/settings.db").exists());
}

#[test]
fn console_auth_full_restore_advances_destination_generation() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let context = f.context();
    let archive = f.backup();
    let initial = crate::console_auth::rotate_restore_generation(&context).ok().unwrap();
    auth_restore(&f, &archive, false, &mut FakeControl::default()).unwrap();
    let restored = crate::console_auth::restore_generation(&context).unwrap().unwrap();
    assert_ne!(restored, initial, "full restore must publish a fresh destination generation");
    assert!(credential_file_is_private(&f.0.join("data/console-auth-generation"), true));
    crate::console_auth::reset_administrator_password(&context).unwrap();
    set_auth_password(&f, CURRENT_PASSWORD);
    let mut control = FakeControl {
        unhealthy: true,
        observe_generation: true,
        ..Default::default()
    };
    assert!(auth_restore(&f, &archive, false, &mut control).unwrap_err().contains("rolled back"));
    let attempted = control.generation_at_health.unwrap();
    let recovered = crate::console_auth::restore_generation(&context).unwrap().unwrap();
    assert_ne!(restored, attempted);
    assert_ne!(attempted, recovered, "rollback must publish a second fresh destination generation");
    assert_ne!(restored, recovered);
    assert!(stored_auth_password(&f).unwrap().verify(CURRENT_PASSWORD).unwrap());
}

#[test]
fn console_auth_memory_restore_keeps_login_state() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let archive = f.backup();
    mutate(&f);
    let store = crate::settings::StoreProbe::at(&f.0);
    store.edit(SEED_WAIT);
    store.edit(SEED_SESSION);
    let generation = crate::console_auth::rotate_restore_generation(&f.context()).ok().unwrap();
    let settings = fs::read(store.path()).unwrap();
    auth_restore(&f, &archive, true, &mut FakeControl::default()).unwrap();
    assert_eq!(memory(&f), "old memory");
    assert_eq!(crate::console_auth::restore_generation(&f.context()).unwrap(), Some(generation),
        "memory-only restore must preserve the marker");
    assert_eq!(fs::read(store.path()).unwrap(), settings, "memory-only restore must preserve the auth DB bytes");
}

#[test]
fn console_auth_restore_marker_publication_failure_respects_replacement_boundary() {
    for fault in ["temp/sync", "dir/sync"] {
        let f = Fixture::new();
        let archive = f.backup();
        mutate(&f);
        let context = f.context();
        let initial = crate::console_auth::rotate_restore_generation(&context).ok().unwrap();
        let mut control = FakeControl { generation_fault: Some(fault), ..Default::default() };
        let error = auth_restore(&f, &archive, false, &mut control).unwrap_err();
        let current = crate::console_auth::restore_generation(&context).unwrap().unwrap();
        assert_eq!(memory(&f), "current memory");
        assert!(error.contains("rolled back"), "{error}");
        assert_ne!(current, initial, "rollback publishes a fresh marker after an interrupted forward phase");
        assert_eq!(control.stops, 2);
        assert!(control.writes > 0);
        assert!(!super::recovery_pending(&context));
        assert!(!fs::read_dir(f.0.join("data")).unwrap().filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")));
    }
}

#[test]
fn console_auth_backup_cancellation_within_scrub_publishes_nothing() {
    let f = Fixture::new();
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let store = crate::settings::StoreProbe::at(&f.0);
    store.edit(SEED_WAIT);
    store.edit(SEED_SESSION);
    let backups = destination(&f);
    let before = fs::read(store.path()).unwrap();
    let check = || {
        let entries = match fs::read_dir(&backups) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => panic!("cannot inspect backup staging: {error}"),
        };
        if entries
            .map(Result::unwrap)
            .any(|entry| entry.path().join("settings.db-journal").is_file())
        {
            Err("fixture cancelled during auth scrub".to_string())
        } else {
            Ok(())
        }
    };
    assert!(crate::backup::create(&f.context(), &check).unwrap_err().contains("cancelled during auth scrub"));
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert!(fs::read_dir(backups).unwrap().next().is_none(), "cancelled staging is removed");
}

#[test]
fn restore_settings_preview_retries_sidecars_during_concurrent_writes_without_blocking_on_gate() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let f = Fixture::new();
    set_quiet(&f, 1000);
    let archive = f.backup();

    // Holding the store gate never blocks restore preview.
    let gate = f
        .context()
        .open_locked_file(RuntimeArea::Data, "locks/settings-store.lock")
        .unwrap();
    let started = Instant::now();
    let gated_plan = preview(&f.context(), &archive, false).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "restore preview blocked on held store gate ({:?})",
        started.elapsed()
    );
    assert!(gated_plan["replaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "data/settings.db"));
    drop(gate);

    let barrier = std::sync::Barrier::new(2);
    let stop = AtomicBool::new(false);
    let mut previews = 0usize;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            let context = f.context();
            for value in 1001..=1030 {
                crate::settings::apply(
                    &context,
                    "reply.wakeQuietMs",
                    &json!(value),
                    "administrator",
                    "console",
                )
                .unwrap();
            }
            stop.store(true, SeqCst);
        });
        barrier.wait();
        let context = f.context();
        while !stop.load(SeqCst) || previews < 5 {
            let plan = preview(&context, &archive, false).unwrap();
            assert!(
                plan["replaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == "data/settings.db"),
                "full preview missed settings.db: {plan}"
            );
            previews += 1;
        }
    });
    assert!(previews >= 5);
}

#[test]
fn ticket443_cross_endpoint_restore_preview_warns_and_three_key_warning_applies() {
    let f = Fixture::new();
    let archive_no_keys = f.backup();
    let context = f.context();

    let empty_warn = preview(&context, &archive_no_keys, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        empty_warn.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains("联网搜索") && s.contains("直连") && s.contains("模型接口")
        }),
        "missing-credentials warning must name all three credential classes: {empty_warn:?}"
    );

    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!("https://archive-endpoint.example.test/v1"),
        "fixture-admin",
        "console",
    )
    .unwrap();
    crate::settings::apply(
        &context,
        "harness.apiKey",
        &json!("fixture-harness-archive-key"),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let archive_override = f.backup();

    // Archive with harness.apiKey counts as having endpoint credentials (no wipe warning on same endpoint).
    let same_endpoint_warnings = preview(&context, &archive_override, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !same_endpoint_warnings.iter().any(|w| w.as_str().unwrap_or("").contains("抹掉")),
        "harness.apiKey must count in has_endpoint_credentials: {same_endpoint_warnings:?}"
    );
    assert!(
        !same_endpoint_warnings.iter().any(|w| w.as_str().unwrap_or("").contains("接口地址")),
        "same endpoint must not trigger cross-endpoint warning: {same_endpoint_warnings:?}"
    );

    // Switch live endpoint back to D (or another URL) and preview restoring archive_override -> cross-endpoint warning!
    crate::settings::reset(&context, "harness.apiBaseUrl", "fixture-admin").unwrap();
    let cross_warnings = preview(&context, &archive_override, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        cross_warnings.iter().any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "cross-endpoint restore preview must warn about endpoint switch: {cross_warnings:?}"
    );
}

#[test]
fn ticket443_r3_restore_preview_ignores_credential_bearing_archived_url() {
    let f = Fixture::new();
    let context = f.context();
    let disk_val = "fixture-r3-restore-disk";
    std::fs::write(f.0.join("config/harness-api-key"), disk_val).unwrap();

    // Archive 1: config.json /harness/apiBaseUrl contains config/harness-api-key (unseeded).
    let cfg_path = f.0.join("config/config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{disk_val}")});
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let archive_cred_config = f.backup();

    let warnings = preview(&context, &archive_cred_config, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !warnings.iter().any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "restore preview must ignore credential-bearing archived config url when live uses D: {warnings:?}"
    );

    // Archive 2: stored settings.db harness.apiBaseUrl contains a post-saved archive DB credential.
    let db_tok = "fixture-r3-restore-db-tok";
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!(format!("https://api.example.com/{db_tok}")),
        "fixture-admin",
        "console",
    )
    .unwrap();
    crate::settings::apply(
        &context,
        "cliModel.apiKey",
        &json!(db_tok),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let archive_cred_stored = f.backup();
    crate::settings::reset(&context, "harness.apiBaseUrl", "fixture-admin").unwrap();
    let stored_warnings = preview(&context, &archive_cred_stored, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !stored_warnings.iter().any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "restore preview must suppress credential-bearing stored url in archive: {stored_warnings:?}"
    );
}

#[test]
fn ticket443_r4_restore_preview_ignores_destination_only_db_secret() {
    let f = Fixture::new();
    let context = f.context();
    let dest_only_tok = "fixture-r4-dest-only";
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!(format!("https://archive-r4.example.test/{dest_only_tok}")),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let archive_without_secret = f.backup();

    // Reset endpoint on live destination and store a secret ONLY in destination settings.db.
    crate::settings::reset(&context, "harness.apiBaseUrl", "fixture-admin").unwrap();
    crate::settings::apply(
        &context,
        "cliModel.apiKey",
        &json!(dest_only_tok),
        "fixture-admin",
        "console",
    )
    .unwrap();

    let warnings = preview(&context, &archive_without_secret, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "destination-only DB secret must not suppress archive endpoint switch warning: {warnings:?}"
    );
}

#[test]
fn ticket443_r7_restore_preview_suppresses_archived_scrub_hash_after_secret_rotation() {
    let f = Fixture::new();
    let context = f.context();
    let leaked_tok = "fixture-r7-archive-tok";
    let leaked_url = format!("https://archive-r7.example.test/{leaked_tok}");
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!(leaked_url),
        "fixture-admin",
        "console",
    )
    .unwrap();
    crate::settings::apply(
        &context,
        "cliModel.apiKey",
        &json!(leaked_tok),
        "fixture-admin",
        "console",
    )
    .unwrap();
    // Rotate secret so leaked_tok is no longer in archive_secrets, only its hash in __scrubbed.harness.apiBaseUrl.
    crate::settings::apply(
        &context,
        "cliModel.apiKey",
        &json!("fixture-r7-rotated-tok"),
        "fixture-admin",
        "console",
    )
    .unwrap();
    // Re-inject the leaked URL into the pre-backup store/config to verify archive_scrubbed_hashes suppresses it.
    {
        let db = rusqlite::Connection::open(f.0.join("data/settings.db")).unwrap();
        db.execute(
            "UPDATE settings SET value=?1 WHERE key='harness.apiBaseUrl'",
            [json!(leaked_url).to_string()],
        )
        .unwrap();
    }
    let mut cfg: Value =
        serde_json::from_slice(&std::fs::read(f.0.join("config/config.json")).unwrap()).unwrap();
    cfg["harness"]["apiBaseUrl"] = json!(leaked_url);
    std::fs::write(f.0.join("config/config.json"), cfg.to_string()).unwrap();
    let archive_with_scrub_hash = f.backup();

    crate::settings::reset(&context, "harness.apiBaseUrl", "fixture-admin").unwrap();
    let warnings = preview(&context, &archive_with_scrub_hash, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !warnings.iter().any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "archive_scrubbed_hashes must suppress archived leaked url even after secret rotation: {warnings:?}"
    );
    // Without archive_scrubbed_hashes, the rotated-key archive would surface leaked_url.
    let without_hashes = crate::settings::archive_effective_model_endpoint(
        Some(crate::settings::StoredEndpointOverride::Value {
            raw: leaked_url.clone(),
            canonical: leaked_url.clone(),
        }),
        &cfg,
        context.dependency_url("model"),
        &[],
        &[],
        None,
    );
    assert_eq!(without_hashes.as_deref(), Some(leaked_url.as_str()));
}

#[test]
fn ticket443_r9_full_restore_roundtrip_and_scrubs_suppressed_archived_config_url() {
    let f = Fixture::new();
    let context = f.context();
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();

    // (a) Clean harness.apiBaseUrl + bound harness.apiKey roundtrip across full restore.
    let clean_ep = "https://restored-clean.example.test/v1";
    let clean_key = "fixture-r9-clean-bound-key";
    crate::settings::apply(&context, "harness.apiBaseUrl", &json!(clean_ep), "fixture-admin", "console")
        .unwrap();
    crate::settings::apply(&context, "harness.apiKey", &json!(clean_key), "fixture-admin", "console")
        .unwrap();
    let archive_clean = f.backup();

    crate::settings::reset(&context, "harness.apiBaseUrl", "fixture-admin").unwrap();
    crate::settings::reset(&context, "harness.apiKey", "fixture-admin").unwrap();
    auth_restore(&f, &archive_clean, false, &mut FakeControl::default()).unwrap();
    let restored_snap = crate::settings::resolve_model_endpoint_and_key(&context).unwrap();
    assert_eq!(restored_snap.effective_endpoint.as_deref(), Some(clean_ep));
    assert_eq!(restored_snap.harness_db_key.as_deref(), Some(clean_key));

    // (b) Credential-bearing unseeded /harness/apiBaseUrl in archive config.json is suppressed in preview
    // AND stripped from restored config/config.json on full restore.
    let f2 = Fixture::new();
    let context2 = f2.context();
    let leaked_tok = "fixture-r9-restore-tok";
    crate::settings::apply(&context2, "cliModel.apiKey", &json!(leaked_tok), "fixture-admin", "console")
        .unwrap();
    let cfg_path2 = f2.0.join("config/config.json");
    let mut cfg2: Value = serde_json::from_slice(&fs::read(&cfg_path2).unwrap()).unwrap();
    cfg2["harness"] = json!({"apiBaseUrl": format!("https://leaked-r9.example.test/{leaked_tok}")});
    fs::write(&cfg_path2, serde_json::to_string_pretty(&cfg2).unwrap()).unwrap();
    let archive_leaked = f2.backup();

    cfg2["harness"] = json!({});
    fs::write(&cfg_path2, serde_json::to_string_pretty(&cfg2).unwrap()).unwrap();
    let preview_warnings = preview(&context2, &archive_leaked, false).unwrap()["warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !preview_warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("模型接口地址")),
        "suppressed archived config url must not warn on switch: {preview_warnings:?}"
    );
    auth_restore(&f2, &archive_leaked, false, &mut FakeControl::default()).unwrap();
    let restored_cfg2: Value = serde_json::from_slice(&fs::read(&cfg_path2).unwrap()).unwrap();
    assert!(
        restored_cfg2.pointer("/harness/apiBaseUrl").is_none(),
        "full restore must strip suppressed /harness/apiBaseUrl from restored config.json: {restored_cfg2}"
    );
    let snap2 = crate::settings::resolve_model_endpoint_and_key(&context2).unwrap();
    assert_eq!(snap2.effective_endpoint.as_deref(), Some(default_ep.as_str()));
}

#[test]
fn ticket443_r10_full_restore_scrubs_archived_settings_db_and_backup_raw_bytes() {
    let f = Fixture::new();
    let context = f.context();
    let host_tok = "fixture-r10-host-disk-tok";
    let leaked_url = format!("https://leaked-r10.example.test/{host_tok}");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();

    // (a) Verify backup ZIP's scrubbed settings.db has zero raw leaked/session bytes after scrub_auth_snapshot VACUUM.
    set_auth_password(&f, ARCHIVED_PASSWORD);
    let session_marker = "vac-r10-session-origin.example.test";
    crate::settings::StoreProbe::at(&f.0).edit(&format!(
        "INSERT INTO console_browser_sessions VALUES \
         (randomblob(32), 'https://{session_marker}', randomblob(32), randomblob(32), randomblob(32), 0, 0, 1, 1);"
    ));
    let db_tok = "fixture-r10-backup-vac-secret";
    let vac_host = "vac-r10-freed.example.test";
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!(format!("https://{vac_host}/{db_tok}")),
        "fixture-admin",
        "console",
    )
    .unwrap();
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!("https://clean-vac.example.test/v1"),
        "fixture-admin",
        "console",
    )
    .unwrap();
    crate::settings::apply(
        &context,
        "cliModel.apiKey",
        &json!(db_tok),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let archive_vac = f.backup();
    let archived_db_path = extract_settings(&f, &archive_vac);
    let archived_db_bytes = fs::read(&archived_db_path).unwrap();
    assert!(
        !archived_db_bytes
            .windows(vac_host.len())
            .any(|w| w == vac_host.as_bytes()),
        "backup settings.db snapshot must contain zero leaked url bytes after VACUUM"
    );
    assert!(
        !archived_db_bytes
            .windows(session_marker.len())
            .any(|w| w == session_marker.as_bytes()),
        "backup settings.db snapshot must contain zero deleted session bytes after scrub_auth_snapshot VACUUM"
    );

    // (b) Archive contains settings.db rows embedding host_tok without knowing host_tok at backup time.
    let f2 = Fixture::new();
    let context2 = f2.context();
    crate::settings::apply(
        &context2,
        "harness.apiBaseUrl",
        &json!(leaked_url),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let archive_host_leaked = f2.backup();

    // Reset live store, place host_tok in config/harness-api-key, and full-restore archive_host_leaked.
    crate::settings::reset(&context2, "harness.apiBaseUrl", "fixture-admin").unwrap();
    fs::write(f2.0.join("config/harness-api-key"), host_tok).unwrap();
    auth_restore(&f2, &archive_host_leaked, false, &mut FakeControl::default()).unwrap();

    // Restored data/settings.db must have zero raw host_tok bytes and tombstoned settings/audit rows.
    let restored_db_bytes = fs::read(f2.0.join("data/settings.db")).unwrap();
    assert!(
        !restored_db_bytes
            .windows(host_tok.len())
            .any(|w| w == host_tok.as_bytes()),
        "restored data/settings.db must contain zero raw host_tok bytes"
    );
    let restored_cfg: Value =
        serde_json::from_slice(&fs::read(f2.0.join("config/config.json")).unwrap()).unwrap();
    assert!(restored_cfg.pointer("/harness/apiBaseUrl").is_none());

    // Even after removing config/harness-api-key, restored store remains suppressed and history masks it.
    fs::remove_file(f2.0.join("config/harness-api-key")).unwrap();
    let snap = crate::settings::resolve_model_endpoint_and_key(&context2).unwrap();
    assert_eq!(snap.effective_endpoint.as_deref(), Some(default_ep.as_str()));
    assert!(snap.stored_url_suppressed);
    let hist = crate::settings::history(&context2, Some("harness.apiBaseUrl"), None, None).unwrap();
    assert!(!hist.to_string().contains(host_tok), "history must stay masked: {hist}");
}

#[test]
fn ticket443_r11_full_restore_vacuums_unmatched_freed_page_remnants() {
    let f = Fixture::new();
    let context = f.context();
    crate::settings::apply(
        &context,
        "harness.apiBaseUrl",
        &json!("https://clean-r11-restore.example.test/v1"),
        "fixture-admin",
        "console",
    )
    .unwrap();
    let base_archive = f.backup();
    let extracted_db = extract_settings(&f, &base_archive);

    let freed_marker = "freed-r11-unmatched-remnant-xyz";
    let conn = rusqlite::Connection::open(&extracted_db).unwrap();
    conn.execute_batch("PRAGMA secure_delete=OFF; PRAGMA journal_mode=DELETE;")
        .unwrap();
    conn.execute(
        "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES(?1, ?2, 1, 1, 'fixture')",
        ["freed.remnant.probe", &format!("\"https://{freed_marker}/path\"")],
    )
    .unwrap();
    conn.execute("DELETE FROM settings WHERE key='freed.remnant.probe'", [])
        .unwrap();
    conn.close().unwrap();

    let dirty_bytes = fs::read(&extracted_db).unwrap();
    assert!(
        dirty_bytes
            .windows(freed_marker.len())
            .any(|w| w == freed_marker.as_bytes()),
        "test setup requires freed-page bytes in archived settings.db"
    );
    let dirty_archive = rewrite_archive(&base_archive, "freed-remnant.zip", "settings.db", &dirty_bytes);

    auth_restore(&f, &dirty_archive, false, &mut FakeControl::default()).unwrap();
    let restored_bytes = fs::read(f.0.join("data/settings.db")).unwrap();
    assert!(
        !restored_bytes
            .windows(freed_marker.len())
            .any(|w| w == freed_marker.as_bytes()),
        "full restore must vacuum non-empty settings.db even when no row matches"
    );
}
