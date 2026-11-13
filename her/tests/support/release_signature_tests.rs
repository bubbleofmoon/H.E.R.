//! Signed-list acceptance at the fake-feed and actual executable seams.
use super::*;

fn assert_no_release_staged(f: &Fixture) {
    for image in ["her.exe", "her-cli.exe", "herupdate.exe"] {
        assert_eq!(f.count(&format!("/{image}")), 0, "downloaded {image}");
    }
    assert!(!f.root.join("install/releases/v1.2.3").exists());
    assert!(!f.root.join("install/herupdate.exe").exists());
    assert!(fs::read_dir(f.root.join("install/releases"))
        .unwrap()
        .all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".staging-")
        }));
}

#[test]
fn primary_and_backup_signed_lists_allow_check_and_install() {
    for key in ["primary", "backup"] {
        let f = Fixture::new();
        f.sign(key);
        let out = f.run("check");
        assert!(out.status.success(), "{key}: {out:?}");
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["available"]["tag"], "v1.2.3");
        assert_no_release_staged(&f);
        let out = f.run("install");
        assert!(out.status.success(), "{key}: {out:?}");
        let target = f.root.join("install/releases/v1.2.3");
        for name in [
            "her.exe",
            "her-cli.exe",
            "herupdate.exe",
            "SHA256SUMS",
            "SHA256SUMS.minisig",
        ] {
            assert_eq!(
                fs::read(target.join(name)).unwrap(),
                f.replies.lock().unwrap()[&format!("/{name}")]
            );
        }
    }
}

#[test]
fn invalid_signed_lists_refuse_before_image_download_or_staging() {
    for case in [
        "wrong key",
        "wrong signature",
        "tampered sums",
        "missing signature",
        "missing signature file",
        "malformed signature",
        "oversized signature",
        "duplicate signature",
        "swapped asset and sums with stale signature",
    ] {
        let f = Fixture::new();
        match case {
            "wrong key" => f.sign("unknown"),
            "wrong signature" => {
                f.replies.lock().unwrap().insert(
                    "/SHA256SUMS.minisig".into(),
                    release_signing::sign(b"another release", "primary"),
                );
            }
            "tampered sums" => {
                let mut replies = f.replies.lock().unwrap();
                replies.get_mut("/SHA256SUMS").unwrap().push(b'\n');
            }
            "missing signature" | "duplicate signature" => {
                let mut feed = f.feed();
                let assets = feed[0]["assets"].as_array_mut().unwrap();
                if case == "missing signature" {
                    assets.retain(|a| a["name"] != "SHA256SUMS.minisig");
                } else {
                    assets.push(
                        assets
                            .iter()
                            .find(|a| a["name"] == "SHA256SUMS.minisig")
                            .unwrap()
                            .clone(),
                    );
                }
                f.set_feed(feed);
            }
            "missing signature file" => {
                f.replies.lock().unwrap().remove("/SHA256SUMS.minisig");
            }
            "malformed signature" => {
                f.replies.lock().unwrap().insert(
                    "/SHA256SUMS.minisig".into(),
                    b"SENSITIVE-SIGNATURE-CONTENT".to_vec(),
                );
            }
            "oversized signature" => {
                f.replies
                    .lock()
                    .unwrap()
                    .insert("/SHA256SUMS.minisig".into(), vec![b'x'; 4097]);
            }
            "swapped asset and sums with stale signature" => {
                let mut replies = f.replies.lock().unwrap();
                replies.insert("/her.exe".into(), b"replacement attacker image".to_vec());
                let mut sums = String::new();
                for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
                    sums.push_str(&format!(
                        "{:x}  {name}\n",
                        Sha256::digest(&replies[&format!("/{name}")])
                    ));
                }
                replies.insert("/SHA256SUMS".into(), sums.into_bytes());
            }
            _ => unreachable!(),
        }
        for action in ["check", "install"] {
            let out = f.run(action);
            assert_eq!(out.status.code(), Some(2), "{case}, {action}: {out:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("release checksum signature missing, invalid or untrusted"),
                "{case}: {stderr}"
            );
            assert!(!stderr.contains("SENSITIVE-SIGNATURE-CONTENT"));
            assert_no_release_staged(&f);
        }
    }
}

#[test]
fn install_reauthenticates_after_discovery_before_creating_staging() {
    let f = Fixture::new();
    let signature = {
        let replies = f.replies.lock().unwrap();
        release_signing::sign(&replies["/SHA256SUMS"], "unknown")
    };
    f.replies
        .lock()
        .unwrap()
        .insert("/SHA256SUMS.minisig#2".into(), signature);
    let out = f.run("install");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        f.count("/SHA256SUMS.minisig"),
        2,
        "installation must fetch its own signature"
    );
    assert_no_release_staged(&f);
}

#[test]
fn unknown_key_cannot_introduce_replacement_pins() {
    let f = Fixture::new();
    let replacement = format!(
        "synthetic replacement with primary {}",
        include_str!("../fixtures/release-signing/rotated.pub")
    );
    {
        let mut replies = f.replies.lock().unwrap();
        replies.insert("/her.exe".into(), replacement.into_bytes());
        let sums = ["her.exe", "her-cli.exe", "herupdate.exe"]
            .into_iter()
            .map(|name| {
                format!(
                    "{:x}  {name}\n",
                    Sha256::digest(&replies[&format!("/{name}")])
                )
            })
            .collect::<String>();
        replies.insert(
            "/SHA256SUMS".into(),
            format!(
                "# HER-RELEASE v1.2.3 2\n{}{sums}",
                release_signing::keys("rotated")
            )
            .into_bytes(),
        );
    }
    f.sign("unknown");
    let out = f.run("install");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_no_release_staged(&f);
}

#[cfg(feature = "parity-clock")]
#[test]
fn trusted_backup_installs_rotated_keys_that_activate_only_when_current() {
    let f = Fixture::new();
    f.select_current("v0.0.0");
    let old_assets = f.replies.lock().unwrap().clone();
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 1", "v1.2.3 2")
            .replace(
                &release_signing::keys("primary"),
                &release_signing::keys("rotated"),
            );
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    f.sign("backup");
    assert!(f.run("install").status.success());
    assert!(!f.root.join("install/release-trust.json").exists());
    // Staging cannot change authority, even from another executable or after restart.
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 2", "v1.2.4 2");
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    f.sign("rotated");
    assert_eq!(f.run("check").status.code(), Some(2));
    f.select_current("v1.2.3");
    for (key, expected) in [("rotated", 0), ("primary", 2), ("backup", 0)] {
        f.sign(key);
        let out = f.run("check");
        assert_eq!(out.status.code(), Some(expected), "{key}: {out:?}");
    }
    // Exact older signed bytes relabeled by the feed still cannot replay.
    *f.replies.lock().unwrap() = old_assets;
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    assert_eq!(f.run("check").status.code(), Some(2));
}

#[cfg(feature = "parity-clock")]
#[test]
fn fixed_helper_refuses_invalid_installed_lists_before_trial_and_records_safe_alert() {
    for case in [
        "missing",
        "malformed",
        "unknown",
        "oversized",
        "swapped asset and sums",
    ] {
        let (f, platform) = handover_fixture(true, true);
        let target = f.root.join("install/releases/v1.2.3");
        let signature_path = target.join("SHA256SUMS.minisig");
        match case {
            "missing" => fs::remove_file(&signature_path).unwrap(),
            "malformed" => fs::write(&signature_path, b"SENSITIVE-SIGNATURE-CONTENT").unwrap(),
            "oversized" => fs::write(&signature_path, vec![b'x'; 4097]).unwrap(),
            "unknown" => fs::write(
                &signature_path,
                release_signing::sign(&fs::read(target.join("SHA256SUMS")).unwrap(), "unknown"),
            )
            .unwrap(),
            "swapped asset and sums" => {
                fs::write(target.join("her.exe"), b"swapped installed image").unwrap();
                let sums = ["her.exe", "her-cli.exe", "herupdate.exe"]
                    .into_iter()
                    .map(|name| {
                        format!(
                            "{:x}  {name}\n",
                            Sha256::digest(fs::read(target.join(name)).unwrap())
                        )
                    })
                    .collect::<String>();
                fs::write(target.join("SHA256SUMS"), sums).unwrap();
            }
            _ => unreachable!(),
        }
        let out = handover(&f, &platform);
        assert_eq!(out.status.code(), Some(2), "{case}: {out:?}");
        let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        assert_eq!(
            state["events"],
            json!([]),
            "{case}: must refuse before trial/retirement"
        );
        assert_eq!(state["current"], "v0.0.0");
        let alert: Value =
            serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
                .unwrap();
        assert_eq!(
            alert["reason"],
            "release checksum signature missing, invalid or untrusted"
        );
        assert_eq!(alert["rollback"], "preserved");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("SENSITIVE-SIGNATURE-CONTENT"));
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn signature_refusal_alerts_only_the_owner_and_clears_discovery_availability() {
    let f = Fixture::new();
    let mut first = f.supervisor(1_800_000_000_000);
    f.wait_count(&mut first, "/send_private_msg", 1);
    drop(first);
    f.sign("unknown");
    let mut next = f.supervisor(1_800_000_000_000 + 7 * 60 * 60 * 1000);
    f.wait_count(&mut next, "/feed", 2);
    f.wait_count(&mut next, "/send_private_msg", 2);
    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state: Value =
            serde_json::from_slice(&fs::read(f.root.join("runtime/data/releases.json")).unwrap())
                .unwrap();
        if state["error"] == "release checksum signature missing, invalid or untrusted" {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "refusal state not published: {state}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(state["available"].is_null(), "{state}");
    assert_eq!(
        state["error"],
        "release checksum signature missing, invalid or untrusted"
    );
    assert_eq!(f.count("/send_group_msg"), 0);
    let requests = f.seen.lock().unwrap();
    let notice = requests
        .iter()
        .filter(|r| r.starts_with("POST /send_private_msg "))
        .last()
        .unwrap();
    let body: Value = serde_json::from_str(notice.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["user_id"], 70001);
    let text = body["message"].to_string();
    assert!(text.contains("发布校验清单签名"), "{body}");
    assert!(!text.contains("RW"));
}

#[test]
fn replayed_signed_assets_under_a_newer_feed_tag_are_refused() {
    let f = Fixture::new();
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v9.9.9".into();
    f.set_feed(feed);
    for action in ["check", "install"] {
        let out = f.run(action);
        assert_eq!(out.status.code(), Some(2), "{action}: {out:?}");
        assert_no_release_staged(&f);
        assert!(!f.root.join("install/releases/v9.9.9").exists());
    }
}

#[test]
fn current_metadata_preserves_replay_floor_with_missing_fresh_or_interrupted_old_state() {
    let f = Fixture::new();
    let set_identity = |tag: &str, generation: u64| {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
        let hashes = sums
            .lines()
            .filter(|line| !line.starts_with("# HER-RELEASE "))
            .collect::<Vec<_>>()
            .join("\n");
        replies.insert(
            "/SHA256SUMS".into(),
            format!("# HER-RELEASE {tag} {generation}\n{hashes}\n").into_bytes(),
        );
    };
    set_identity("v1.2.3", 2);
    f.sign("backup");
    assert!(f.run("install").status.success());
    f.select_current("v1.2.3");
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    set_identity("v1.2.4", 1);
    f.sign("backup");
    for state in [
        None,
        Some(br#"{"version":1,"generation":0,"proofs":{}}"#.as_slice()),
        Some(b"{interrupted"),
    ] {
        let path = f.root.join("install/release-trust.json");
        if let Some(bytes) = state {
            fs::write(&path, bytes).unwrap();
        }
        for action in ["check", "install"] {
            let out = f.run(action);
            assert_eq!(out.status.code(), Some(2), "{action}: {out:?}");
            assert!(!f.root.join("install/releases/v1.2.4").exists());
        }
    }
    // The authenticated current version remains a floor even at the same generation.
    set_identity("v1.2.4", 2);
    f.sign("backup");
    assert!(f.run("check").status.success());
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.3".into();
    f.set_feed(feed);
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    assert!(serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"].is_null());
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix1_unsigned_last_good_is_never_started() {
    let (f, platform) = handover_fixture(true, false);
    fs::remove_file(f.root.join("install/releases/v0.0.0/SHA256SUMS")).unwrap();
    let out = handover(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(
        state["current"], "v0.0.0",
        "must preserve current before retirement"
    );
    assert!(
        !state["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "start:v0.0.0" || e.as_str().unwrap().starts_with("wait:")),
        "{state}"
    );
    let alert: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
            .unwrap();
    assert_eq!(alert["rollback"], "preserved");
}

#[test]
fn fix1_pre_signing_cached_availability_and_approval_are_discarded_on_restart() {
    let f = Fixture::new();
    let release = json!({"tag":"v1.2.3", "assets":[]});
    fs::write(
        f.root.join("runtime/data/releases.json"),
        json!({
            "version":1, "feedUrl":format!("{}/feed",f.url), "lastCheckedAt":1800000000000i64,
            "available":release, "approval":{"current":"v0.0.0", "release":release,
            "feedUrl":format!("{}/feed",f.url)}, "notified":["v1.2.3"]
        })
        .to_string(),
    )
    .unwrap();
    let (_console, port) = f.console();
    let (status, body) = http(port, "GET", "/api/releases", "", true, false);
    assert_eq!(status, 200, "{body}");
    let view: Value = serde_json::from_str(&body).unwrap();
    assert!(view["available"].is_null(), "{view}");
    assert!(view["approval"].is_null(), "{view}");
    assert!(view["lastCheckedAt"].is_null(), "{view}");
    assert_eq!(f.count("/feed"), 0);
}

#[cfg(feature = "parity-clock")]
fn recover_fixture(f: &Fixture, platform: &Path) -> Output {
    let mut cmd = Command::new(f.root.join("install/herupdate.exe"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.env_remove("HER_INSTALL_ROOT")
        .env("HER_TEST_UPDATE_PLATFORM", platform)
        .args([
            "--recover",
            "--target",
            "v1.2.3",
            "--old-pid",
            "4242",
            "--old-start",
            "17",
            "--old-exe",
        ])
        .arg(f.root.join("install/releases/v1.2.3/her.exe"))
        .arg("--install-root")
        .arg(f.root.join("install"))
        .output()
        .unwrap()
}

#[cfg(feature = "parity-clock")]
#[test]
fn recovery_refuses_missing_or_invalid_last_good_metadata_and_images_before_retirement() {
    for case in ["missing list", "invalid signature", "invalid image"] {
        let (f, platform) = handover_fixture(true, false);
        let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        state["current"] = "v1.2.3".into();
        state["events"] = json!([]);
        fs::write(&platform, state.to_string()).unwrap();
        let old = f.root.join("install/releases/v0.0.0");
        match case {
            "missing list" => fs::remove_file(old.join("SHA256SUMS")).unwrap(),
            "invalid signature" => {
                fs::write(old.join("SHA256SUMS.minisig"), b"invalid signature").unwrap()
            }
            "invalid image" => fs::write(old.join("her.exe"), b"unverified image").unwrap(),
            _ => unreachable!(),
        }
        let out = recover_fixture(&f, &platform);
        assert_eq!(out.status.code(), Some(2), "{case}: {out:?}");
        let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        assert_eq!(state["current"], "v1.2.3", "{case}: {state}");
        assert_eq!(
            state["events"],
            json!([]),
            "{case}: no stop, switch or start: {state}"
        );
        let alert: Value =
            serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
                .unwrap();
        assert_eq!(alert["rollback"], "preserved", "{case}: {alert}");
        assert_eq!(
            alert["reason"],
            "release checksum signature missing, invalid or untrusted"
        );
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn higher_generation_failed_health_restores_verified_last_good_and_recovers() {
    let (f, platform) = handover_fixture_generation(true, false, 2);
    let target = f.root.join("install/releases/v1.2.3");
    let sums = fs::read_to_string(target.join("SHA256SUMS"))
        .unwrap()
        .replace(
            &release_signing::keys("primary"),
            &release_signing::keys("rotated"),
        );
    fs::write(
        target.join("SHA256SUMS.minisig"),
        release_signing::sign(sums.as_bytes(), "backup"),
    )
    .unwrap();
    fs::write(target.join("SHA256SUMS"), sums).unwrap();
    let out = handover(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v0.0.0", "{state}");
    let events = state["events"].as_array().unwrap();
    assert!(events.contains(&json!("health:120000")));
    assert!(events.contains(&json!("start:v0.0.0")));
    assert!(events.contains(&json!("stop:v1.2.3")));
    // Crash after switching to the higher-generation candidate, before healthy judgment.
    state["current"] = "v1.2.3".into();
    state["events"] = json!([]);
    fs::write(&platform, state.to_string()).unwrap();
    let out = recover_fixture(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v0.0.0", "{state}");
    assert!(state["events"]
        .as_array()
        .unwrap()
        .contains(&json!("start:v0.0.0")));
}

#[test]
fn signed_release_identity_must_be_present_unique_and_valid() {
    for header in [
        "",
        "# HER-RELEASE v1.2.3 0\n",
        "# HER-RELEASE v1.2.3 1\n# HER-RELEASE v1.2.3 2\n",
        "# HER-RELEASE v1.2.3 invalid\n",
    ] {
        let f = Fixture::new();
        {
            let mut replies = f.replies.lock().unwrap();
            let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
            let (_, hashes) = sums.split_once('\n').unwrap();
            replies.insert(
                "/SHA256SUMS".into(),
                format!("{header}{hashes}").into_bytes(),
            );
        }
        f.sign("primary");
        for action in ["check", "install"] {
            let out = f.run(action);
            assert_eq!(out.status.code(), Some(2), "{header:?} {action}: {out:?}");
            assert_no_release_staged(&f);
        }
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn old_format_last_good_is_unsupported_and_never_retires_owners() {
    let (f, platform) = handover_fixture(true, false);
    let path = f.root.join("install/releases/v0.0.0/SHA256SUMS");
    let sums = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, &sums).unwrap();
    fs::write(
        path.with_file_name("SHA256SUMS.minisig"),
        release_signing::sign(sums.as_bytes(), "primary"),
    )
    .unwrap();
    let out = handover(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v0.0.0");
    assert_eq!(state["events"], json!([]), "{state}");
}

#[cfg(feature = "parity-clock")]
#[test]
fn discovery_repeats_if_current_changes_before_cache_publication() {
    let f = Fixture::new();
    for (tag, generation) in [("v0.0.0", 1), ("v1.0.0", 2)] {
        let dir = f.root.join("install/releases").join(tag);
        fs::create_dir_all(&dir).unwrap();
        for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
            if !dir.join(name).exists() {
                fs::write(dir.join(name), b"verified current image").unwrap();
            }
        }
        f.sign_directory(tag, generation, "backup");
    }
    f.select_current("v0.0.0");
    *f.hold.lock().unwrap() = Some("/SHA256SUMS.minisig".into());
    let mut supervisor = f.supervisor(1_800_000_000_000);
    f.wait_count(&mut supervisor, "/SHA256SUMS.minisig", 1);
    // A concurrent installer/helper completes publication while network I/O is pending.
    f.select_current("v1.0.0");
    *f.hold.lock().unwrap() = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    let state = loop {
        let state: Value =
            serde_json::from_slice(&fs::read(f.root.join("runtime/data/releases.json")).unwrap())
                .unwrap();
        if state["error"].as_str().is_some() || !state["available"].is_null() {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "discovery never settled: {state}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        state["available"].is_null(),
        "stale result published under new authority: {state}"
    );
    assert!(state["approval"].is_null());
    assert_eq!(
        state["error"],
        "release checksum signature missing, invalid or untrusted"
    );
    assert_eq!(
        f.count("/feed"),
        2,
        "must repeat discovery with current's generation-2 authority"
    );
    drop(supervisor);
    let (_console, port) = f.console();
    let (code, body) = http(port, "GET", "/api/releases", "", true, false);
    assert_eq!(code, 200, "{body}");
    let view: Value = serde_json::from_str(&body).unwrap();
    assert!(
        view["available"].is_null() && view["approval"].is_null(),
        "{view}"
    );
}

#[test]
fn missing_or_damaged_installed_metadata_never_bootstraps() {
    for case in ["missing checksum", "invalid signature"] {
        let f = Fixture::new();
        assert!(f.run("install").status.success());
        f.select_current("v1.2.3");
        let root = f.root.join("install");
        match case {
            "missing checksum" => fs::remove_file(root.join("releases/v1.2.3/SHA256SUMS")).unwrap(),
            "invalid signature" => {
                fs::write(root.join("releases/v1.2.3/SHA256SUMS.minisig"), b"invalid").unwrap()
            }
            _ => unreachable!(),
        }
        // With no independently verifiable installed authority, refusal is durable.
        fs::remove_file(root.join("releases/v0.0.0/SHA256SUMS.minisig")).unwrap();
        let requests = f.requests.load(Ordering::SeqCst);
        for action in ["check", "install"] {
            let out = f.run(action);
            assert_eq!(out.status.code(), Some(2), "{case}: {out:?}");
        }
        assert_eq!(
            f.requests.load(Ordering::SeqCst),
            requests,
            "{case}: cannot bootstrap"
        );
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix3_recovery_restores_verified_last_good_despite_damaged_current() {
    for (name, missing) in [
        ("SHA256SUMS", true),
        ("SHA256SUMS.minisig", true),
        ("SHA256SUMS.minisig", false),
        ("her.exe", true),
        ("her-cli.exe", true),
        ("herupdate.exe", true),
        ("her.exe", false),
        ("her-cli.exe", false),
        ("herupdate.exe", false),
    ] {
        let (f, platform) = handover_fixture(true, false);
        let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        state["current"] = "v1.2.3".into();
        fs::write(&platform, state.to_string()).unwrap();
        let path = f.root.join("install/releases/v1.2.3").join(name);
        if missing {
            fs::remove_file(path).unwrap();
        } else {
            fs::write(path, b"unverified; must never execute").unwrap();
        }
        let out = recover_fixture(&f, &platform);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{name} missing={missing}: {out:?}"
        );
        let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        assert_eq!(
            state["current"], "v0.0.0",
            "{name} missing={missing}: {out:?} {state}"
        );
        let events = state["events"].as_array().unwrap();
        assert!(events.contains(&json!("start:v0.0.0")), "{state}");
        assert!(
            !events.contains(&json!("start:v1.2.3")) && !events.contains(&json!("trial:v1.2.3")),
            "{state}"
        );
        let alert: Value =
            serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
                .unwrap();
        assert_eq!(alert["rollback"], "restored");
    }
}

#[test]
fn fix3_deleted_current_recovers_last_good_or_newest_installed_authority() {
    for (last_good, interrupted) in [(true, false), (false, false), (false, true)] {
        let f = Fixture::new();
        {
            let mut replies = f.replies.lock().unwrap();
            let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
                .unwrap()
                .replace("v1.2.3 1", "v1.2.3 2")
                .replace(
                    &release_signing::keys("primary"),
                    &release_signing::keys("rotated"),
                );
            replies.insert("/SHA256SUMS".into(), sums.into_bytes());
        }
        f.sign("backup");
        assert!(f.run("install").status.success());
        f.select_current("v1.2.3");
        let root = f.root.join("install");
        if last_good {
            fs::rename(root.join("current"), root.join("last-good")).unwrap();
        } else if interrupted {
            fs::rename(root.join("current"), root.join(".current-previous")).unwrap();
        } else {
            #[cfg(unix)]
            fs::remove_file(root.join("current")).unwrap();
            #[cfg(windows)]
            fs::remove_dir(root.join("current")).unwrap();
        }
        // Last-good wins over a newer staged release; without last-good, skip
        // damaged newer metadata and use the newest independently verified list.
        let newer = root.join("releases/v9.0.0");
        fs::create_dir(&newer).unwrap();
        for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
            fs::write(newer.join(name), b"newer staged image").unwrap();
        }
        f.sign_directory("v9.0.0", 3, "backup");
        if !last_good {
            fs::write(newer.join("SHA256SUMS.minisig"), b"invalid").unwrap();
        }
        // Both replaying the selected version and an actually older signed release
        // are refused, including when no selection links remain.
        for tag in ["v1.2.3", "v1.2.2"] {
            let mut feed = f.feed();
            feed[0]["tag_name"] = tag.into();
            f.set_feed(feed);
            let mut replies = f.replies.lock().unwrap();
            let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
            let (_, rest) = sums.split_once('\n').unwrap();
            replies.insert(
                "/SHA256SUMS".into(),
                format!("# HER-RELEASE {tag} 2\n{rest}").into_bytes(),
            );
            drop(replies);
            f.sign("backup");
            let out = f.run("check");
            assert!(out.status.success(), "last_good={last_good} {tag}: {out:?}");
            assert!(
                serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"].is_null(),
                "last_good={last_good}: old release re-admitted: {out:?}"
            );
            let out = f.run("install");
            assert_eq!(
                out.status.code(),
                Some(2),
                "older install admitted: {out:?}"
            );
        }
        let mut feed = f.feed();
        feed[0]["tag_name"] = "v1.2.4".into();
        f.set_feed(feed);
        for (generation, signer, expected) in
            [(1, "backup", 2), (3, "primary", 2), (2, "backup", 0)]
        {
            let mut replies = f.replies.lock().unwrap();
            let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
            let hashes = sums
                .lines()
                .filter(|line| !line.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            // The retired key signs a list containing itself at a higher generation,
            // so only the recovered authority (not self-verification) can reject it.
            let keys = release_signing::keys(if signer == "primary" {
                "primary"
            } else {
                "rotated"
            });
            replies.insert(
                "/SHA256SUMS".into(),
                format!("# HER-RELEASE v1.2.4 {generation}\n{keys}{hashes}\n").into_bytes(),
            );
            drop(replies);
            f.sign(signer);
            for action in ["check", "install"] {
                let out = f.run(action);
                assert_eq!(
                    out.status.code(),
                    Some(expected),
                    "last_good={last_good} {generation} {signer} {action}: {out:?}"
                );
            }
        }
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix3_both_releases_corrupt_stay_stopped_and_alert() {
    let (f, platform) = handover_fixture(true, false);
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["current"] = "v1.2.3".into();
    fs::write(&platform, state.to_string()).unwrap();
    for tag in ["v0.0.0", "v1.2.3"] {
        let dir = f.root.join("install/releases").join(tag);
        fs::write(dir.join("SHA256SUMS.minisig"), b"invalid signature").unwrap();
        fs::remove_file(dir.join("her-cli.exe")).unwrap();
    }
    let out = recover_fixture(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v1.2.3");
    assert_eq!(
        state["events"],
        json!([]),
        "must not execute either release: {state}"
    );
    let alert: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
            .unwrap();
    assert_eq!(alert["rollback"], "preserved");
    assert_eq!(
        alert["reason"],
        "release checksum signature missing, invalid or untrusted"
    );
}

#[test]
fn fix3_unverified_release_folders_refuse_discovery_and_install_with_durable_alert() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("install/releases/v0.0.0/SHA256SUMS.minisig")).unwrap();
    let requests = f.requests.load(Ordering::SeqCst);
    for action in ["check", "install"] {
        let out = f.run(action);
        assert_eq!(out.status.code(), Some(2), "{action}: {out:?}");
    }
    assert_eq!(f.requests.load(Ordering::SeqCst), requests);
    let alert: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
            .unwrap();
    assert_eq!(
        alert["reason"],
        "release checksum signature missing, invalid or untrusted"
    );
    assert_eq!(alert["restored"], "unchanged", "nothing was stopped");
}

#[test]
fn fix3_empty_installation_still_bootstraps_from_compiled_pins() {
    let f = Fixture::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .env_remove("HER_INSTALL_ROOT")
        .args(["release", "check", "--runtime-root"])
        .arg(f.root.join("runtime"))
        .arg("--install-root")
        .arg(f.root.join("empty-install"));
    for (signer, expected) in [("primary", 0), ("backup", 0), ("unknown", 2)] {
        f.sign(signer);
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(expected), "{signer}: {out:?}");
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix3_crash_startup_publishes_verified_last_good_helper() {
    let (f, platform) = handover_fixture(true, false);
    let target = handover_executable_release(&f);
    // An executable overlay distinguishes last-good from the existing root helper.
    let helper = f.root.join("install/releases/v0.0.0/herupdate.exe");
    let mut bytes = fs::read(&helper).unwrap();
    bytes.extend_from_slice(b"\nfixture last-good helper\n");
    fs::write(&helper, bytes).unwrap();
    f.sign_directory("v0.0.0", 1, "backup");
    for name in ["SHA256SUMS.minisig", "her-cli.exe", "herupdate.exe"] {
        fs::remove_file(target.join(name)).unwrap();
    }
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["current"] = "v1.2.3".into();
    for (attempt, now) in [1_000_000, 1_240_000, 1_540_000].into_iter().enumerate() {
        state["now"] = now.into();
        fs::write(&platform, state.to_string()).unwrap();
        let mut command = Command::new(target.join("her.exe"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = command
            .env_remove("HER_INSTALL_ROOT")
            .env("HER_TEST_UPDATE_PLATFORM", &platform)
            .args(["supervise", "--runtime-root"])
            .arg(f.root.join("install/runtime"))
            .arg("--install-root")
            .arg(f.root.join("install"))
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(if attempt == 2 { 3 } else { 2 }),
            "{out:?}"
        );
    }
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v0.0.0", "{state}");
    assert_eq!(
        fs::read(f.root.join("install/herupdate.exe")).unwrap(),
        fs::read(f.root.join("install/releases/v0.0.0/herupdate.exe")).unwrap()
    );
}

#[test]
fn fix3_invalid_current_metadata_recovers_last_good_authority_for_check_and_install() {
    let f = Fixture::new();
    f.select_current("v0.0.0");
    fs::rename(
        f.root.join("install/current"),
        f.root.join("install/last-good"),
    )
    .unwrap();
    let original = f.replies.lock().unwrap().clone();
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 1", "v1.2.3 2")
            .replace(
                &release_signing::keys("primary"),
                &release_signing::keys("rotated"),
            );
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    f.sign("backup");
    assert!(f.run("install").status.success());
    f.select_current("v1.2.3");
    fs::remove_file(f.root.join("install/releases/v1.2.3/SHA256SUMS.minisig")).unwrap();
    *f.replies.lock().unwrap() = original;
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 1", "v1.2.4 1");
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    // Both the original keys and generation come from verified last-good.
    f.sign("primary");
    for action in ["check", "install"] {
        let out = f.run(action);
        assert!(out.status.success(), "{action}: {out:?}");
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix3_crash_startup_recovers_interrupted_last_good_selection() {
    let (f, _) = handover_fixture(true, false);
    // A previously running installation retains its unlocked owner-gate files.
    // Missing gates correctly remain unknown; missing config alone induces the crashes.
    for dir in ["data/lifecycle", "data/logs"] {
        fs::create_dir_all(f.root.join("install/runtime").join(dir)).unwrap();
    }
    for gate in [
        "data/lifecycle/lifecycle.lock",
        "data/logs/rbridge-supervisor.lock",
        "data/logs/rbridge-live.lock",
        "data/logs/rbridge-console.lock",
    ] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(f.root.join("install/runtime").join(gate))
            .unwrap();
    }
    let target = handover_executable_release(&f);
    fs::copy(
        env!("CARGO_BIN_EXE_her"),
        f.root.join("install/releases/v0.0.0/her.exe"),
    )
    .unwrap();
    f.sign_directory("v0.0.0", 1, "backup");
    f.select_current("v0.0.0");
    fs::rename(
        f.root.join("install/current"),
        f.root.join("install/.last-good-previous"),
    )
    .unwrap();
    f.select_current("v1.2.3");
    for attempt in 0..3 {
        let mut command = Command::new(target.join("her.exe"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = command
            .env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_TEST_UPDATE_PLATFORM")
            .env_remove("HER_TEST_HANDOVER_CONTROL")
            .args(["supervise", "--runtime-root"])
            .arg(f.root.join("install/runtime"))
            .arg("--install-root")
            .arg(f.root.join("install"))
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(if attempt == 2 { 3 } else { 2 }),
            "{out:?}"
        );
    }
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let alert = fs::read(f.root.join("install/update-alert.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if alert
            .as_ref()
            .is_some_and(|alert| alert["rollback"] == "restored")
        {
            break;
        }
        assert!(Instant::now() < end, "recovery did not finish: {alert:?}");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::canonicalize(f.root.join("install/current")).unwrap(),
        fs::canonicalize(f.root.join("install/releases/v0.0.0")).unwrap()
    );
    assert!(!f.root.join("install/.last-good-previous").exists());
}

fn fix4_alert(f: &Fixture) -> Option<Value> {
    fs::read(f.root.join("install/update-alert.json"))
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
}

#[cfg(feature = "parity-clock")]
fn fix4_wait(what: &str, child: &mut Child, f: &Fixture, ready: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(f.root.join("supervisor.stderr")).unwrap_or_default()
        );
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix4_reads_and_polls_write_no_refusal_alert_and_admission_raises_one() {
    let f = Fixture::new();
    // Occupied installation whose only versioned release no longer verifies.
    fs::remove_file(f.root.join("install/releases/v0.0.0/SHA256SUMS.minisig")).unwrap();
    let (_console, port) = f.console();
    for _ in 0..3 {
        http(port, "GET", "/api/releases", "", true, false);
    }
    assert_eq!(fix4_alert(&f), None, "status reads must not write alerts");
    let start = 1_800_000_000_000i64;
    let mut first = f.supervisor(start);
    fix4_wait("poll check admission alert", &mut first, &f, || {
        fix4_alert(&f).is_some()
    });
    let raised = fix4_alert(&f).unwrap();
    assert_eq!(
        raised["reason"],
        "release checksum signature missing, invalid or untrusted"
    );
    assert_eq!(raised["failed"], "unselected");
    assert_eq!(raised["restored"], "unchanged", "nothing was stopped");
    assert_eq!(raised["rollback"], "preserved");
    // Several one-second polls and status reads later, the same alert remains.
    thread::sleep(Duration::from_millis(2500));
    for _ in 0..3 {
        http(port, "GET", "/api/releases", "", true, false);
    }
    assert_eq!(fix4_alert(&f), Some(raised.clone()));
    drop(first);
    // After delivery, the next due check of the same refusal is deduplicated.
    let mut delivered = raised.clone();
    delivered["delivered"] = true.into();
    fs::write(
        f.root.join("install/update-alert.json"),
        delivered.to_string(),
    )
    .unwrap();
    let due = start + 6 * 60 * 60 * 1000 + 30_000;
    let mut later = f.supervisor(due);
    fix4_wait("second admitted check", &mut later, &f, || {
        fs::read(f.root.join("runtime/data/releases.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|state| state["lastCheckedAt"].as_i64())
            .is_some_and(|at| at >= due)
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(fix4_alert(&f), Some(delivered.clone()));
    drop(later);
    // An explicit admission raises exactly one new alert and never overwrites it undelivered.
    assert_eq!(f.run("check").status.code(), Some(2));
    let admitted = fix4_alert(&f).expect("check admission must alert");
    assert_ne!(admitted["id"], raised["id"]);
    assert_eq!(admitted["delivered"], false);
    for action in ["install", "check"] {
        assert_eq!(f.run(action).status.code(), Some(2), "{action}");
    }
    assert_eq!(fix4_alert(&f), Some(admitted));
}

#[test]
fn fix4_refusal_never_overwrites_an_in_progress_rollback_alert() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("install/releases/v0.0.0/SHA256SUMS.minisig")).unwrap();
    let rollback = json!({"v":1,"id":"rollback-1","failed":"v1.2.3","restored":"v0.0.0",
        "reason":"new supervisor was not healthy within 120 seconds","rollback":"pending",
        "delivered":true,"retryAfter":0});
    fs::write(
        f.root.join("install/update-alert.json"),
        rollback.to_string(),
    )
    .unwrap();
    // A rollback is in progress only while its helper holds the update-restore gate.
    let gate = fix5_hold_restore_gate(&f);
    for action in ["check", "install"] {
        assert_eq!(f.run(action).status.code(), Some(2), "{action}");
    }
    assert_eq!(fix4_alert(&f), Some(rollback));
    drop(gate);
}

fn fix5_hold_restore_gate(f: &Fixture) -> fs::File {
    let path = f
        .root
        .join("install/runtime/data/locks/update-restore.lock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    gate.try_lock().unwrap();
    gate
}

#[test]
fn fix4_unsigned_bootstrap_folder_installs_first_signed_release_with_compiled_pins() {
    let f = Fixture::new();
    let releases = f.root.join("install/releases");
    let bootstrap = releases.join("local");
    fs::create_dir(&bootstrap).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_her-cli"), bootstrap.join("her-cli.exe")).unwrap();
    fs::remove_dir_all(releases.join("v0.0.0")).unwrap();
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v0.1.0".into();
    f.set_feed(feed);
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 1", "v0.1.0 1");
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    f.sign("primary");
    for action in ["check", "install"] {
        let mut command = Command::new(bootstrap.join("her-cli.exe"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = command
            .env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_RUNTIME_ROOT")
            .args(["release", action, "--runtime-root"])
            .arg(f.root.join("runtime"))
            .arg("--install-root")
            .arg(f.root.join("install"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{action}: {out:?}");
    }
    assert!(releases.join("v0.1.0/SHA256SUMS.minisig").is_file());
    assert!(bootstrap.join("her-cli.exe").is_file());
    assert!(fs::symlink_metadata(f.root.join("install/current")).is_err());
}

#[test]
fn fix4_lost_current_recorded_by_interrupted_switch_keeps_its_version_floor() {
    let f = Fixture::new();
    assert!(f.run("install").status.success());
    let root = f.root.join("install");
    f.select_current("v0.0.0");
    fs::rename(root.join("current"), root.join("last-good")).unwrap();
    f.select_current("v1.2.3");
    fs::rename(root.join("current"), root.join(".current-previous")).unwrap();
    let offer = |tag: &str| {
        let mut feed = f.feed();
        feed[0]["tag_name"] = tag.into();
        f.set_feed(feed);
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
        let (_, rest) = sums.split_once('\n').unwrap();
        replies.insert(
            "/SHA256SUMS".into(),
            format!("# HER-RELEASE {tag} 1\n{rest}").into_bytes(),
        );
        drop(replies);
        f.sign("primary");
    };
    // Last-good v0.0.0 is older than the lost current v1.2.3.
    offer("v1.2.2");
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    assert!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"].is_null(),
        "release below the lost current offered: {out:?}"
    );
    assert_eq!(f.run("install").status.code(), Some(2));
    assert!(!root.join("releases/v1.2.2").exists());
    offer("v1.2.4");
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"]["tag"],
        "v1.2.4"
    );
    assert!(f.run("install").status.success());
}

#[test]
fn fix4_invalid_current_metadata_alerts_once_on_admission_not_on_reads() {
    let f = Fixture::new();
    assert!(f.run("install").status.success());
    let root = f.root.join("install");
    f.select_current("v0.0.0");
    fs::rename(root.join("current"), root.join("last-good")).unwrap();
    f.select_current("v1.2.3");
    fs::write(root.join("releases/v1.2.3/SHA256SUMS.minisig"), b"invalid").unwrap();
    let (_console, port) = f.console();
    for _ in 0..3 {
        let (status, body) = http(port, "GET", "/api/releases", "", true, false);
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(fix4_alert(&f), None, "status reads must not write alerts");
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone())
            .unwrap()
            .replace("v1.2.3 1", "v1.2.4 1");
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
    }
    f.sign("primary");
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    let raised = fix4_alert(&f).expect("recovered authority must alert on admission");
    assert_eq!(raised["failed"], "v1.2.3");
    assert_eq!(raised["restored"], "unchanged");
    assert_eq!(raised["rollback"], "preserved");
    assert_eq!(
        raised["reason"],
        "current release metadata unverifiable; using verified installed authority"
    );
    for action in ["check", "install"] {
        let out = f.run(action);
        assert!(out.status.success(), "{action}: {out:?}");
    }
    assert_eq!(fix4_alert(&f), Some(raised));
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix4_retirement_uses_the_candidate_cli_unless_it_fails_verification() {
    let (f, platform) = handover_fixture(true, false);
    let out = handover(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    let events = state["events"].as_array().unwrap();
    assert!(events.contains(&json!("stop:v1.2.3")), "{state}");
    assert!(events.contains(&json!("retire-cli:v1.2.3")), "{state}");
    let (f, platform) = handover_fixture(true, false);
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["current"] = "v1.2.3".into();
    fs::write(&platform, state.to_string()).unwrap();
    fs::remove_file(f.root.join("install/releases/v1.2.3/her-cli.exe")).unwrap();
    let out = recover_fixture(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    let events = state["events"].as_array().unwrap();
    assert!(events.contains(&json!("retire-cli:v0.0.0")), "{state}");
    assert!(events.contains(&json!("start:v0.0.0")), "{state}");
}

const FIX5_REFUSED: &str = "release checksum signature missing, invalid or untrusted";
const FIX5_LOST: &str = "current release metadata unverifiable; using verified installed authority";

#[cfg(feature = "parity-clock")]
#[test]
fn fix5_helper_and_recovery_refusals_record_over_pending_or_undelivered_alerts() {
    for seeded in [
        // A rollback whose helper died before finishing it.
        json!({"v":1,"id":"stale-rollback","failed":"v1.2.2","restored":"v0.0.0",
            "reason":"new supervisor was not healthy within 120 seconds","rollback":"pending",
            "delivered":false,"retryAfter":0}),
        json!({"v":1,"id":"undelivered","failed":"v1.2.2","restored":"v0.0.0",
            "reason":"new supervisor was not healthy within 120 seconds","rollback":"restored",
            "delivered":false,"retryAfter":0}),
    ] {
        // The fixed helper refuses an invalid candidate list before trial.
        let (f, platform) = handover_fixture(true, false);
        fs::write(f.root.join("install/update-alert.json"), seeded.to_string()).unwrap();
        fs::write(
            f.root.join("install/releases/v1.2.3/SHA256SUMS.minisig"),
            b"invalid signature",
        )
        .unwrap();
        let out = handover(&f, &platform);
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let alert = fix4_alert(&f).unwrap();
        assert_ne!(alert["id"], seeded["id"], "helper refusal dropped: {alert}");
        assert_eq!(alert["failed"], "v1.2.3", "{alert}");
        assert_eq!(alert["reason"], FIX5_REFUSED);
        assert_eq!(alert["rollback"], "preserved");
        // Crash recovery finds the last-good it would restore unverifiable.
        let (f, platform) = handover_fixture(true, false);
        let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        state["current"] = "v1.2.3".into();
        state["events"] = json!([]);
        fs::write(&platform, state.to_string()).unwrap();
        fs::write(f.root.join("install/update-alert.json"), seeded.to_string()).unwrap();
        fs::write(
            f.root.join("install/releases/v0.0.0/SHA256SUMS.minisig"),
            b"invalid signature",
        )
        .unwrap();
        let out = recover_fixture(&f, &platform);
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        assert_eq!(state["events"], json!([]), "{state}");
        let alert = fix4_alert(&f).unwrap();
        assert_ne!(
            alert["id"], seeded["id"],
            "recovery refusal dropped: {alert}"
        );
        assert_eq!(alert["failed"], "v1.2.3", "{alert}");
        assert_eq!(alert["reason"], FIX5_REFUSED);
        assert_eq!(alert["rollback"], "preserved");
    }
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix5_stale_pending_rollback_alert_does_not_block_admission() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("install/releases/v0.0.0/SHA256SUMS.minisig")).unwrap();
    // Delivered, but its helper died before finishing: nobody holds the restore gate.
    let stale = json!({"v":1,"id":"rollback-1","failed":"v1.2.3","restored":"v0.0.0",
        "reason":"new supervisor was not healthy within 120 seconds","rollback":"pending",
        "delivered":true,"retryAfter":0});
    fs::write(f.root.join("install/update-alert.json"), stale.to_string()).unwrap();
    assert_eq!(f.run("check").status.code(), Some(2));
    let alert = fix4_alert(&f).expect("admission alert");
    assert_ne!(
        alert["id"], stale["id"],
        "stale pending blocked admission: {alert}"
    );
    assert_eq!(alert["reason"], FIX5_REFUSED);
    assert_eq!(alert["rollback"], "preserved");
    // The supervisor's discovery check holds the restore gate itself; that is no rollback.
    fs::write(f.root.join("install/update-alert.json"), stale.to_string()).unwrap();
    let mut supervisor = f.supervisor(1_800_000_000_000);
    fix4_wait(
        "poll admission over stale pending",
        &mut supervisor,
        &f,
        || fix4_alert(&f).is_some_and(|alert| alert["id"] != stale["id"]),
    );
    assert_eq!(fix4_alert(&f).unwrap()["reason"], FIX5_REFUSED);
}

/// Selects v1.2.3 as current with corrupt metadata over an older verified last-good.
fn fix5_lost_current(f: &Fixture) {
    assert!(f.run("install").status.success());
    let root = f.root.join("install");
    f.select_current("v0.0.0");
    fs::rename(root.join("current"), root.join("last-good")).unwrap();
    f.select_current("v1.2.3");
    fs::write(root.join("releases/v1.2.3/SHA256SUMS.minisig"), b"invalid").unwrap();
}

fn fix5_offer(f: &Fixture, tag: &str) {
    let mut feed = f.feed();
    feed[0]["tag_name"] = tag.into();
    f.set_feed(feed);
    let mut replies = f.replies.lock().unwrap();
    let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
    let (_, rest) = sums.split_once('\n').unwrap();
    replies.insert(
        "/SHA256SUMS".into(),
        format!("# HER-RELEASE {tag} 1\n{rest}").into_bytes(),
    );
    drop(replies);
    f.sign("primary");
}

#[test]
fn fix5_corrupt_current_metadata_keeps_the_lost_version_as_floor() {
    let f = Fixture::new();
    fix5_lost_current(&f);
    let root = f.root.join("install");
    // Verified last-good v0.0.0 is older than the selected current v1.2.3.
    fix5_offer(&f, "v1.2.2");
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    assert!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"].is_null(),
        "release below the lost current offered: {out:?}"
    );
    assert_eq!(f.run("install").status.code(), Some(2));
    assert!(!root.join("releases/v1.2.2").exists());
    fix5_offer(&f, "v1.2.4");
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["available"]["tag"],
        "v1.2.4"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn fix5_lost_current_alerts_once_across_feed_error_and_success() {
    let f = Fixture::new();
    fix5_lost_current(&f);
    fix5_offer(&f, "v1.2.4");
    let state = || {
        fs::read(f.root.join("runtime/data/releases.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or(Value::Null)
    };
    let checked = |at: i64, error: &dyn Fn(&Value) -> bool| {
        let state = state();
        state["lastCheckedAt"]
            .as_i64()
            .is_some_and(|last| last >= at)
            && error(&state["error"])
    };
    let start = 1_800_000_000_000i64;
    let mut first = f.supervisor(start);
    fix4_wait("lost-current admission alert", &mut first, &f, || {
        checked(start, &|error| error == FIX5_LOST)
    });
    drop(first);
    let raised = fix4_alert(&f).expect("lost current must alert once");
    assert_eq!(raised["reason"], FIX5_LOST);
    assert_eq!(raised["failed"], "v1.2.3");
    let mut delivered = raised.clone();
    delivered["delivered"] = true.into();
    fs::write(
        f.root.join("install/update-alert.json"),
        delivered.to_string(),
    )
    .unwrap();
    // A feed failure, then success again: the same lost current is not re-raised.
    let good = f.feed();
    f.set_feed(json!({"broken": true}));
    let failed = start + 6 * 60 * 60 * 1000 + 30_000;
    let mut second = f.supervisor(failed);
    fix4_wait("feed error check", &mut second, &f, || {
        checked(failed, &|error| error.is_string() && error != FIX5_LOST)
    });
    drop(second);
    f.set_feed(good);
    let recovered = start + 12 * 60 * 60 * 1000 + 60_000;
    let mut third = f.supervisor(recovered);
    fix4_wait("feed success check", &mut third, &f, || {
        checked(recovered, &|error| error == FIX5_LOST)
    });
    drop(third);
    assert_eq!(fix4_alert(&f), Some(delivered));
}
