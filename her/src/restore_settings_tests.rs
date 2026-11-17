// #148: backup and restore of the settings store (`data/settings.db`). Included in `restore::tests`, so the fixture,
// `FakeControl`, `memory` and `mutate` above are in scope. Every test name starts with `restore_settings_`, so one
// libtest filter runs them all.

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
    rusqlite::Connection::open_with_flags(f.0.join("data/settings.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
        .query_row("SELECT count(*) FROM settings_audit", [], |r| r.get(0))
        .unwrap()
}

/// Appends `count` history rows of 1 KiB each.
fn grow_settings_history(f: &Fixture, count: usize) {
    let mut db = rusqlite::Connection::open_with_flags(
        f.0.join("data/settings.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    let text = "x".repeat(1024);
    for i in 0..count {
        tx.execute(
            "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
             VALUES(?1, 'fixture', 'fixture', 'global', 'reply.wakeQuietMs', NULL, ?2, ?3)",
            rusqlite::params![i as i64, text, format!("grow-{i}")],
        )
        .unwrap();
    }
    tx.commit().unwrap();
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
            assert_eq!(
                fs::read(f.0.join("data/settings.db")).unwrap(),
                current,
                "memory-only keeps every byte of settings.db"
            );
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
        assert!(
            crate::backup::verify_archive(pre, &|| Ok(())).unwrap().files.contains_key("settings.db"),
            "the pre-restore backup holds the current settings.db"
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
        assert!(
            crate::backup::verify_archive(pre, &|| Ok(())).unwrap().files.contains_key("settings.db"),
            "the pre-restore backup keeps the current store"
        );
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
        let plan = preview(&f.context(), &archive, true).unwrap();
        let mut control = FakeControl::default();
        let error = execute_with(
            &f.context(),
            &archive,
            true,
            plan["confirmation"].as_str().unwrap(),
            "fixture-admin",
            &mut control,
        )
        .expect_err(case);
        assert!(error.contains("settings"), "{case}: {error}");
        assert_eq!(control.stops, 0, "{case}: refused before anything stops");
        assert!(
            control.alerts.iter().any(|alert| alert.contains("Restore refused before stopping")),
            "{case}: {:?}",
            control.alerts
        );
        assert_eq!(memory(&f), "old memory");
    }
}

#[test]
fn restore_settings_archive_from_a_newer_release_restores_its_store_as_is() {
    let f = Fixture::new();
    let store = f.0.join("data/settings.db");
    let db = rusqlite::Connection::open(&store).unwrap();
    let mode: String = db.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    db.execute_batch("CREATE TABLE later(x INTEGER); INSERT INTO later VALUES(7); PRAGMA user_version=2;").unwrap();
    drop(db);
    let archive = f.backup();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(f.0.join(format!("data/settings.db{suffix}")));
    }
    set_quiet(&f, 5000);
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
    let db = rusqlite::Connection::open_with_flags(&store, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    let later: i64 = db.query_row("SELECT x FROM later", [], |r| r.get(0)).unwrap();
    drop(db);
    assert_eq!((version, later), (2, 7), "the newer store is restored as it was archived");
    let view = crate::settings::view(&f.context()).unwrap();
    assert_eq!(view["storeError"], crate::settings::NEWER_SCHEMA_ERROR, "this release refuses it, as after a downgrade");
    assert_eq!(quiet(&f), None, "and applies none of its values");
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
    let mut source = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
    let mut files = Vec::new();
    for index in 0..source.len() {
        let mut entry = source.by_index(index).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        files.push((entry.name().to_string(), bytes));
    }
    drop(source);
    let damaged = b"not a SQLite database".to_vec();
    files.retain(|(name, _)| name != "settings.db");
    files.push(("settings.db".to_string(), damaged.clone()));
    let (_, manifest) = files.iter_mut().find(|(name, _)| name == "manifest.json").unwrap();
    let mut value: Value = serde_json::from_slice(manifest).unwrap();
    value["files"]["settings.db"] =
        json!({"size": damaged.len(), "sha256": format!("{:x}", Sha256::digest(&damaged))});
    *manifest = serde_json::to_vec(&value).unwrap();
    let path = archive.with_file_name("damaged-settings.zip");
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    for (name, bytes) in files {
        writer.start_file(name, zip::write::FileOptions::default()).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
    let error = crate::backup::verify_archive(&path, &|| Ok(())).unwrap_err();
    assert!(error.contains("integrity check"), "{error}");
    for memory_only in [false, true] {
        assert!(preview(&f.context(), &path, memory_only).is_err(), "a damaged archive is refused for every scope");
    }
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

#[test]
fn restore_settings_held_store_gate_refuses_the_restore_before_stopping() {
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
    assert!(error.contains("settings"), "{error}");
    assert_eq!(control.stops, 0, "a gate the restore cannot take refuses it before anything stops");
    assert_eq!(quiet(&f), Some(5000), "nothing changed");
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
fn restore_settings_writer_cannot_take_the_store_gate_after_admission() {
    let f = Fixture::new();
    set_quiet(&f, 4000);
    let archive = f.backup();
    set_quiet(&f, 5000);
    let plan = preview(&f.context(), &archive, false).unwrap();
    // A writer tries for the store gate as soon as the restore is admitted: the restore must already hold it, so a
    // gate that is taken between admission and the mutation can never make a rollback run.
    let mut control = FakeControl { probe_writer_after_admission: true, ..Default::default() };
    let result = execute_with(
        &f.context(),
        &archive,
        false,
        plan["confirmation"].as_str().unwrap(),
        "fixture-admin",
        &mut control,
    );
    assert_eq!(control.writer_got_gate, Some(false), "a writer took the store gate after the restore was admitted");
    result.unwrap();
    assert_eq!(quiet(&f), Some(4000));
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
