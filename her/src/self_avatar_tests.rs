use super::*;
use base64::Engine as _;
use crate::self_avatar::{avatar_url, current_with, url_allowed, valid_uin, CACHE_FILE, FAILURE_BACKOFF_MS, MAX_BYTES, TTL_MS};
use std::cell::Cell;

// #206: her live QQ avatar for `qq_get_self_image`. No test here contacts qlogo: the fetch is a closure
// or `self_avatar::FETCH_TEST_HOOK`, and the hook's absence makes the fetch fail.

const T0: i64 = 1_800_000_000_000;
const PIXELS: u64 = 64_000_000;

type Fetched = Result<Vec<u8>, String>;

fn png(w: u32, h: u32) -> Vec<u8> {
    crate::media_normalize::fixtures::still(image::ImageFormat::Png, w, h, false)
}

/// A valid 2x2 JPEG of exactly `total` bytes: comment segments after the start marker fill it up.
fn padded_jpeg(total: usize) -> Vec<u8> {
    let jpeg = crate::media_normalize::fixtures::still(image::ImageFormat::Jpeg, 2, 2, false);
    let mut out = jpeg[..2].to_vec();
    let mut missing = total - jpeg.len();
    while missing > 0 {
        // One segment is 4 to 65537 bytes (marker, length, text); never leave 1 to 3 bytes over.
        let mut size = missing.min(65_537);
        if (1..4).contains(&(missing - size)) {
            size -= 4;
        }
        let length = size - 2;
        out.extend_from_slice(&[0xFF, 0xFE, (length >> 8) as u8, (length & 0xFF) as u8]);
        out.extend(std::iter::repeat(b'x').take(length - 2));
        missing -= size;
    }
    out.extend_from_slice(&jpeg[2..]);
    assert_eq!(out.len(), total);
    out
}

fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let decoded = image::load_from_memory(bytes).unwrap();
    (decoded.width(), decoded.height())
}

fn with_identity(label: &str, identity: Value) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = fixture_runtime(label, "fixture-self-avatar-console-token");
    context.write_atomic(RuntimeArea::Data, "identity.json", identity.to_string().as_bytes()).unwrap();
    (root, context)
}

fn record(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("data").join(CACHE_FILE)).unwrap()).unwrap()
}

/// Clears the test fetch hook when the test ends, also on a failed assertion.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        crate::self_avatar::FETCH_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

#[test]
fn self_avatar_url_allowlist_and_uin_rules() {
    assert_eq!(avatar_url("10001"), "https://q1.qlogo.cn/g?b=qq&nk=10001&s=640");
    assert!(url_allowed(&avatar_url("10001")));
    for refused in [
        "http://q1.qlogo.cn/g?b=qq&nk=10001&s=640",
        "https://q2.qlogo.cn/g?b=qq&nk=10001&s=640",
        "https://q1.qlogo.cn.example.test/g?b=qq&nk=10001&s=640",
        "https://example.test/q1.qlogo.cn/g?b=qq&nk=10001&s=640",
        "https://q1.qlogo.cn:8443/g?b=qq&nk=10001&s=640",
        "https://a@q1.qlogo.cn/g?b=qq&nk=10001&s=640",
        "https://q1.qlogo.cn/g?b=qq&nk=10001&s=100",
        "https://q1.qlogo.cn/g?b=qq&nk=10001&s=640&x=1",
        "https://q1.qlogo.cn/g?b=qq&nk=10001&s=640#a",
        "https://q1.qlogo.cn/h?b=qq&nk=10001&s=640",
        "https://q1.qlogo.cn/g?b=qq&nk=1000&s=640",
        "https://q1.qlogo.cn/g?b=qq&nk=01000&s=640",
        "https://q1.qlogo.cn/g?b=qq&nk=10001&s=1&s=640",
        "file:///g?b=qq&nk=10001&s=640",
    ] {
        assert!(!url_allowed(refused), "{refused}");
    }
    assert!(!url_allowed(&avatar_url(&"1".repeat(12))));
    assert!(valid_uin("10000"), "five digits");
    assert!(!valid_uin("1000"), "four digits");
    assert!(valid_uin(&"1".repeat(11)), "eleven digits");
    assert!(!valid_uin(&"1".repeat(12)), "twelve digits");
    for refused in ["", "01234", "12a45", "12 45", "１２３４５", "123&s=1", "-12345", "+12345"] {
        assert!(!valid_uin(refused), "{refused}");
    }
    // State backups take the data folder's `.json` files only; the cache stays out of them.
    assert!(!CACHE_FILE.ends_with(".json"), "{CACHE_FILE}");
}

#[test]
fn self_avatar_fetches_once_keeps_thirty_days_and_refetches_after() {
    let (root, context) = with_identity("206 avatar ttl", json!({"userId": "10001", "nickname": "n"}));
    let (first, second) = (png(2, 2), png(3, 2));
    let urls = std::cell::RefCell::new(Vec::new());
    let mut fetch = |url: &str| -> Fetched {
        urls.borrow_mut().push(url.to_string());
        Ok(if urls.borrow().len() == 1 { first.clone() } else { second.clone() })
    };
    let live = current_with(&context, T0, PIXELS, &mut fetch).expect("fetched avatar");
    assert_eq!(dimensions(&live.images[0].bytes), (2, 2));
    let stored = record(&root);
    assert_eq!((stored["version"].clone(), stored["uin"].clone(), stored["fetchedAt"].clone()), (json!(1), json!("10001"), json!(T0)));
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(stored["data"].as_str().unwrap()).unwrap(), first);
    let kept = current_with(&context, T0 + TTL_MS - 1, PIXELS, &mut fetch).expect("still fresh");
    assert_eq!(dimensions(&kept.images[0].bytes), (2, 2));
    assert_eq!(urls.borrow().len(), 1, "fresh for the last millisecond of the TTL");
    let renewed = current_with(&context, T0 + TTL_MS, PIXELS, &mut fetch).expect("refetched at the TTL");
    assert_eq!(dimensions(&renewed.images[0].bytes), (3, 2));
    assert_eq!(record(&root)["fetchedAt"], json!(T0 + TTL_MS));
    assert_eq!(*urls.borrow(), vec![avatar_url("10001"), avatar_url("10001")], "only her own avatar address, twice");
}

#[test]
fn self_avatar_failure_falls_back_backs_off_fifteen_minutes_then_retries() {
    let (root, context) = with_identity("206 avatar failure", json!({"userId": "10001"}));
    let calls = Cell::new(0);
    let mut failing = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Err("unreachable".to_string())
    };
    assert!(current_with(&context, T0, PIXELS, &mut failing).is_none());
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS - 1, PIXELS, &mut failing).is_none());
    assert_eq!(calls.get(), 1, "no new fetch inside the backoff");
    let stored = record(&root);
    assert_eq!((stored["uin"].clone(), stored["failedAt"].clone()), (json!("10001"), json!(T0)));
    assert!(stored.get("data").is_none(), "{stored}");
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS, PIXELS, &mut failing).is_none());
    assert_eq!(calls.get(), 2, "the backoff ends at 15 minutes");

    // Bytes the model must not see are failures too: not an image, one byte over the cap, a broken
    // image, too many pixels. Each is retried only after the backoff.
    let mut truncated = png(2, 2);
    truncated.truncate(truncated.len() / 2);
    let cases: Vec<(Vec<u8>, u64)> = vec![
        (b"<html>not an image</html>".to_vec(), PIXELS),
        (padded_jpeg(MAX_BYTES + 1), PIXELS),
        (truncated, PIXELS),
        (png(2, 2), 3),
    ];
    let mut at = T0 + 2 * FAILURE_BACKOFF_MS;
    for (bytes, pixels) in cases {
        let mut fetch = |_: &str| -> Fetched { Ok(bytes.clone()) };
        assert!(current_with(&context, at, pixels, &mut fetch).is_none(), "{} bytes, {pixels} pixels", bytes.len());
        assert_eq!(record(&root)["failedAt"], json!(at));
        at += FAILURE_BACKOFF_MS;
    }
    let exact = padded_jpeg(MAX_BYTES);
    let mut fetch = |_: &str| -> Fetched { Ok(exact.clone()) };
    let live = current_with(&context, at, PIXELS, &mut fetch).expect("exactly the byte cap is accepted");
    assert_eq!(dimensions(&live.images[0].bytes), (2, 2));
    assert_eq!(record(&root)["fetchedAt"], json!(at));
}

#[test]
fn self_avatar_wrong_old_or_damaged_records_refetch() {
    let (root, context) = with_identity("206 avatar records", json!({"userId": "10001"}));
    let path = root.0.join("data").join(CACHE_FILE);
    let good = base64::engine::general_purpose::STANDARD.encode(png(3, 2));
    // Each record is tried one backoff after the previous one: every fetch needs a new attempt reservation.
    let records = |now: i64| -> Vec<String> {
        vec![
            json!({"version": 1, "uin": "10002", "fetchedAt": now, "data": good}).to_string(),
            json!({"version": 1, "uin": "10001", "fetchedAt": now, "data": "!!not base64"}).to_string(),
            json!({"version": 1, "uin": "10001", "fetchedAt": now + 1, "data": good}).to_string(),
            json!({"version": 1, "uin": "10001", "fetchedAt": i64::MIN, "data": good}).to_string(),
            json!({"version": 2, "uin": "10001", "fetchedAt": now, "data": good}).to_string(),
            json!({"version": 1, "uin": "10001", "failedAt": i64::MIN}).to_string(),
            "not json".to_string(),
            " ".repeat(MAX_BYTES * 4 / 3 + 4097),
        ]
    };
    let count = records(T0).len();
    for index in 0..count {
        let now = T0 + index as i64 * FAILURE_BACKOFF_MS;
        std::fs::write(&path, &records(now)[index]).unwrap();
        let calls = Cell::new(0);
        let mut fetch = |_: &str| -> Fetched {
            calls.set(calls.get() + 1);
            Ok(png(2, 2))
        };
        let live = current_with(&context, now, PIXELS, &mut fetch).unwrap_or_else(|| panic!("record {index} must refetch"));
        assert_eq!(dimensions(&live.images[0].bytes), (2, 2), "record {index}");
        assert_eq!(calls.get(), 1, "record {index}");
        assert_eq!(record(&root)["fetchedAt"], json!(now), "record {index} is replaced");
    }
    // A record of exactly the size cap is read (trailing JSON whitespace pads it); one byte more was refused above.
    let now = T0 + count as i64 * FAILURE_BACKOFF_MS;
    let mut exact = json!({"version": 1, "uin": "10001", "fetchedAt": now, "data": good}).to_string();
    exact.push_str(&" ".repeat(MAX_BYTES * 4 / 3 + 4096 - exact.len()));
    std::fs::write(&path, &exact).unwrap();
    let mut refused = |_: &str| -> Fetched { panic!("a record at the cap is used, not refetched") };
    let kept = current_with(&context, now, PIXELS, &mut refused).expect("record at the cap");
    assert_eq!(dimensions(&kept.images[0].bytes), (3, 2));
}

#[test]
fn self_avatar_needs_her_own_valid_uin_and_a_selected_runtime() {
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    for (index, identity) in [
        json!({"userId": "1000"}),
        json!({"userId": "123&s=1"}),
        json!({"userId": 10001}),
        json!({"userId": ""}),
        json!({"nickname": "n"}),
        json!("10001"),
    ]
    .into_iter()
    .enumerate()
    {
        let (root, context) = with_identity(&format!("206 avatar identity {index}"), identity);
        assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "identity {index}");
        assert!(!root.0.join("data").join(CACHE_FILE).exists(), "identity {index}");
    }
    let (root, context) = fixture_runtime("206 avatar no identity", "fixture-self-avatar-console-token");
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none());
    assert!(!root.0.join("data").join(CACHE_FILE).exists());
    assert_eq!(calls.get(), 0);
    // Positive control: her own valid uin in a selected runtime fetches once and stores the avatar.
    let (valid_root, valid) = with_identity("206 avatar identity valid", json!({"userId": "10001"}));
    assert!(current_with(&valid, T0, PIXELS, &mut fetch).is_some(), "a valid uin serves her avatar");
    assert_eq!(calls.get(), 1, "a valid uin fetches");
    assert_eq!(record(&valid_root)["uin"], json!("10001"));
}

#[test]
fn self_avatar_concurrent_callers_fetch_once() {
    let (_root, context) = with_identity("206 avatar concurrency", json!({"userId": "10001"}));
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let avatar = png(2, 2);
    let (context, calls, avatar) = (&context, &calls, &avatar);
    std::thread::scope(|scope| {
        let callers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(move || {
                    let mut fetch = |_: &str| -> Fetched {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        Ok(avatar.clone())
                    };
                    current_with(&context, T0, PIXELS, &mut fetch).is_some()
                })
            })
            .collect();
        for caller in callers {
            assert!(caller.join().unwrap());
        }
    });
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "the second caller reads the stored avatar");
}

#[test]
fn self_avatar_taken_gate_serves_the_portrait_until_its_holder_exits() {
    let (_root, context) = with_identity("206 avatar gate", json!({"userId": "10001"}));
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    // Another process (or a caller stuck in its fetch) holds the gate.
    let holder = context.open_locked_file(RuntimeArea::Data, crate::self_avatar::GATE_FILE).unwrap();
    let started = std::time::Instant::now();
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "a taken gate serves the portrait");
    assert!(started.elapsed() >= std::time::Duration::from_millis(1_900), "it waited for the gate: {:?}", started.elapsed());
    assert_eq!(calls.get(), 0);
    // The holder exits (a restart midway): the OS frees the lock and the next call fetches.
    drop(holder);
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_some());
    assert_eq!(calls.get(), 1);
}

#[test]
fn self_avatar_failed_store_backs_off_on_disk_for_fifteen_minutes_then_refetches() {
    let (root, context) = with_identity("206 avatar store failure", json!({"userId": "10001"}));
    let path = root.0.join("data").join(CACHE_FILE);
    std::fs::create_dir(&path).unwrap();
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_some(), "a failed store does not hide the fetched avatar");
    assert!(path.is_dir(), "the blocked record is left alone");
    assert!(current_with(&context, T0 + 1, PIXELS, &mut fetch).is_none(), "inside the fifteen minutes the portrait is served");
    assert_eq!(calls.get(), 1, "a failed store is not retried inside the fifteen minutes");
    std::fs::remove_dir(&path).unwrap();
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS - 1, PIXELS, &mut fetch).is_none());
    assert_eq!(calls.get(), 1, "still inside the backoff once the record can be written");
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS, PIXELS, &mut fetch).is_some());
    assert_eq!(calls.get(), 2, "after fifteen minutes it fetches and stores again");
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS + 1, PIXELS, &mut fetch).is_some());
    assert_eq!(calls.get(), 2, "the stored record serves the next call");
}

#[test]
fn self_avatar_unreadable_cache_counts_as_a_failure_under_the_backoff() {
    let (root, context) = with_identity("206 avatar unreadable cache", json!({"userId": "10001"}));
    let path = root.0.join("data").join(CACHE_FILE);
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_some());
    assert_eq!(calls.get(), 1);
    // The stored record exists but can no longer be read as a file.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(current_with(&context, T0 + 1, PIXELS, &mut fetch).is_none(), "an unreadable cache serves the portrait");
    assert_eq!(calls.get(), 1, "an unreadable cache is not a plain miss");
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS - 1, PIXELS, &mut fetch).is_none());
    assert_eq!(calls.get(), 1);
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS, PIXELS, &mut fetch).is_some(), "one fetch after the backoff");
    assert_eq!(calls.get(), 2);
    assert!(current_with(&context, T0 + FAILURE_BACKOFF_MS + 1, PIXELS, &mut fetch).is_none());
    assert_eq!(calls.get(), 2, "still at most one fetch per fifteen minutes");
}

#[test]
fn self_avatar_unwritable_reservation_serves_the_portrait_without_fetching() {
    let (root, context) = with_identity("206 avatar reservation", json!({"userId": "10001"}));
    let attempt = root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE);
    std::fs::create_dir(&attempt).unwrap();
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "no reservation, no fetch");
    assert_eq!(calls.get(), 0);
    assert!(!root.0.join("data").join(CACHE_FILE).exists());
    std::fs::remove_dir(&attempt).unwrap();
    assert!(current_with(&context, T0 + 1, PIXELS, &mut fetch).is_some());
    assert_eq!(calls.get(), 1);
    let reservation: Value = serde_json::from_slice(&std::fs::read(&attempt).unwrap()).unwrap();
    assert_eq!(reservation, json!({"version": 1, "attempts": [{"uin": "10001", "attemptAt": T0 + 1}]}));
}

#[test]
fn self_avatar_identity_change_during_the_fetch_discards_the_result() {
    let (root, context) = with_identity("206 avatar identity during fetch", json!({"userId": "10001"}));
    let urls = std::cell::RefCell::new(Vec::new());
    let mut fetch = |url: &str| -> Fetched {
        urls.borrow_mut().push(url.to_string());
        if urls.borrow().len() == 1 {
            // The bridge logs in as another account while the first fetch runs.
            context.write_atomic(RuntimeArea::Data, "identity.json", json!({"userId": "10002"}).to_string().as_bytes()).unwrap();
        }
        Ok(png(2, 2))
    };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "the old account's picture is not served");
    assert!(!root.0.join("data").join(CACHE_FILE).exists(), "and not stored");
    assert!(current_with(&context, T0 + 1, PIXELS, &mut fetch).is_some(), "the new account fetches at once");
    assert_eq!(*urls.borrow(), vec![avatar_url("10001"), avatar_url("10002")]);
    assert_eq!(record(&root)["uin"], json!("10002"));
}

#[test]
fn self_avatar_console_answer_and_tool_result_name_no_file_or_address() {
    let _guard = HookGuard;
    let (root, context) = with_identity("206 avatar console", json!({"userId": "10001"}));
    std::fs::create_dir_all(root.0.join("resources/assets")).unwrap();
    std::fs::write(root.0.join("resources/assets/deepseek娘.png"), png(3, 2)).unwrap();
    let cfg = context.load_config().unwrap();
    let params = HashMap::from([("key".to_string(), "private:7001".to_string())]);
    let urls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = urls.clone();
    crate::self_avatar::FETCH_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |url: &str| -> Fetched {
            seen.borrow_mut().push(url.to_string());
            Ok(png(2, 2))
        }));
    });
    let model_texts = |content: &[Value]| -> String {
        content.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
    };
    // No denied term, file name, folder or address; the tool description legitimately uses "/" between questions.
    let clean = |text: &str| {
        assert!(crate::model_text_policy::denied_terms(text).is_empty(), "{text}");
        for leak in [".png", "assets", "qlogo", "http", "self-avatar", "identity.json", "娘"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
    };
    let clean_result = |text: &str| {
        clean(text);
        assert!(!text.contains('/') && !text.contains('\\'), "{text}");
    };
    // Removing the record and the attempt reservation lets the next call fetch again at once.
    let forget = || {
        std::fs::remove_file(root.0.join("data").join(CACHE_FILE)).unwrap();
        std::fs::remove_file(root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE)).unwrap();
    };

    let live = super::super::handle_self_image(&context, &params, &HashMap::new(), &cfg).unwrap();
    assert_eq!(live["source"], "qqAvatar");
    let mut keys: Vec<&String> = live.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["image", "images", "key", "ok", "source"]);
    let bytes = base64::engine::general_purpose::STANDARD.decode(live["image"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(dimensions(&bytes), (2, 2));
    assert_eq!(*urls.borrow(), vec![avatar_url("10001")]);
    let outcome = crate::qq_tools::self_image_outcome(&live);
    assert!(!outcome.is_error);
    assert_eq!(outcome.content[0], json!({"type": "text", "text": "这是你现在的 QQ 头像："}));
    assert_eq!(outcome.content[1]["type"], "image");
    assert_eq!(outcome.content.len(), 2);
    clean_result(&model_texts(&outcome.content));

    // A failed fetch serves the static portrait under a neutral lead line.
    forget();
    let failed = std::rc::Rc::new(Cell::new(0));
    let counted = failed.clone();
    crate::self_avatar::FETCH_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |_: &str| -> Fetched {
            counted.set(counted.get() + 1);
            Err("down".to_string())
        }));
    });
    let portrait = super::super::handle_self_image(&context, &params, &HashMap::new(), &cfg).unwrap();
    assert_eq!(failed.get(), 1, "the failing fetch ran");
    assert_eq!(portrait["source"], "portrait");
    let bytes = base64::engine::general_purpose::STANDARD.decode(portrait["image"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(dimensions(&bytes), (3, 2));
    let outcome = crate::qq_tools::self_image_outcome(&portrait);
    assert_eq!(outcome.content[0], json!({"type": "text", "text": "这是你的默认形象图："}));
    clean_result(&model_texts(&outcome.content));

    // With no hook the fetch fails: a unit test never reaches the network.
    forget();
    crate::self_avatar::FETCH_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
    let unhooked = super::super::handle_self_image(&context, &params, &HashMap::new(), &cfg).unwrap();
    assert_eq!(unhooked["source"], "portrait");
    assert!(root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE).exists(), "the unhooked call reached the fetch");

    let missing = crate::qq_tools::self_image_outcome(&json!({"ok": false, "error": "未找到形象图片"}));
    assert!(missing.is_error);
    clean_result(&model_texts(&missing.content));

    let tools = crate::qq_tools::registered_tools(&json!({}));
    let description = tools.iter().find(|tool| tool["name"] == "qq_get_self_image").unwrap()["description"].as_str().unwrap().to_string();
    assert!(description.contains("QQ 头像") && !description.contains("小鲸鱼"), "{description}");
    clean(&description);
}

#[test]
fn self_avatar_animated_avatar_sends_only_the_lead_line_and_images() {
    let _guard = HookGuard;
    let (_root, context) = with_identity("206 avatar animated", json!({"userId": "10001"}));
    let cfg = context.load_config().unwrap();
    let params = HashMap::from([("key".to_string(), "private:7001".to_string())]);
    let animated = crate::media_normalize::fixtures::gif(3, 12, 9);
    crate::self_avatar::FETCH_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |_: &str| -> Fetched { Ok(animated.clone()) }));
    });
    let live = super::super::handle_self_image(&context, &params, &HashMap::new(), &cfg).unwrap();
    assert_eq!(live["source"], "qqAvatar");
    assert!(live["images"].as_array().unwrap().len() > 1, "several frames");
    let outcome = crate::qq_tools::self_image_outcome(&live);
    assert!(!outcome.is_error);
    assert_eq!(outcome.content[0], json!({"type": "text", "text": "这是你现在的 QQ 头像："}));
    assert!(outcome.content[1..].iter().all(|block| block["type"] == "image"), "{:?}", outcome.content);
}

#[test]
fn self_avatar_clock_is_read_once_the_gate_is_held() {
    let (_root, context) = with_identity("206 avatar clock", json!({"userId": "10001"}));
    let mut fetch = |_: &str| -> Fetched { Ok(png(2, 2)) };
    let gate_held_at_clock = Cell::new(None);
    let clock = || -> i64 {
        let taken = context.open_locked_file(RuntimeArea::Data, crate::self_avatar::GATE_FILE).is_err();
        gate_held_at_clock.set(Some(taken));
        T0
    };
    assert!(crate::self_avatar::current_with_clock(&context, &clock, PIXELS, &mut fetch).is_some());
    assert_eq!(gate_held_at_clock.get(), Some(true), "the clock is read while the gate is held");
}

#[test]
fn self_avatar_identity_is_read_once_the_gate_is_held() {
    let (_root, context) = with_identity("206 avatar identity switch", json!({"userId": "10001"}));
    // Another caller holds the gate. This caller starts while her uin is 10001 and, once it has found the
    // gate taken, stops in the wait hook; her uin changes to 10002 and the gate frees before it goes on.
    let holder = context.open_locked_file(RuntimeArea::Data, crate::self_avatar::GATE_FILE).unwrap();
    let urls = std::sync::Mutex::new(Vec::new());
    let (context, urls) = (&context, &urls);
    let (reached, waiting) = std::sync::mpsc::channel::<()>();
    let (go, resume) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let caller = scope.spawn(move || {
            crate::self_avatar::GATE_WAIT_TEST_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    let _ = reached.send(());
                    let _ = resume.recv();
                }));
            });
            let mut fetch = |url: &str| -> Fetched {
                urls.lock().unwrap().push(url.to_string());
                Ok(png(2, 2))
            };
            crate::self_avatar::current_with(context, T0, PIXELS, &mut fetch).is_some()
        });
        waiting.recv_timeout(std::time::Duration::from_secs(30)).expect("the caller found the gate taken");
        context.write_atomic(RuntimeArea::Data, "identity.json", json!({"userId": "10002"}).to_string().as_bytes()).unwrap();
        drop(holder);
        go.send(()).unwrap();
        drop(go);
        assert!(caller.join().unwrap());
    });
    assert_eq!(*urls.lock().unwrap(), vec![avatar_url("10002")], "the caller fetches the uin it has once the gate is held");
}

#[test]
fn self_avatar_identity_change_during_normalization_stores_and_serves_nothing() {
    let (root, context) = with_identity("206 avatar normalize fetch", json!({"userId": "10001"}));
    let identity_path = root.0.join("data").join("identity.json");
    crate::self_avatar::NORMALIZE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::write(&identity_path, json!({"userId": "10002"}).to_string()).unwrap();
        }));
    });
    let mut fetch = |_: &str| -> Fetched { Ok(png(2, 2)) };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "the account changed while the picture was normalized");
    assert!(!root.0.join("data").join(CACHE_FILE).exists(), "and nothing is stored");
    crate::self_avatar::NORMALIZE_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
}

#[test]
fn self_avatar_identity_change_during_normalization_serves_no_cache_hit() {
    let (root, context) = with_identity("206 avatar normalize hit", json!({"userId": "10001"}));
    let stored = json!({"version": 1, "uin": "10001", "fetchedAt": T0, "data": base64::engine::general_purpose::STANDARD.encode(png(2, 2))});
    std::fs::write(root.0.join("data").join(CACHE_FILE), stored.to_string()).unwrap();
    let identity_path = root.0.join("data").join("identity.json");
    crate::self_avatar::NORMALIZE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::write(&identity_path, json!({"userId": "10002"}).to_string()).unwrap();
        }));
    });
    let mut refused = |_: &str| -> Fetched { panic!("a fresh record is served, not fetched") };
    assert!(current_with(&context, T0 + 1, PIXELS, &mut refused).is_none(), "the account changed while the stored picture was normalized");
    crate::self_avatar::NORMALIZE_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
}

#[test]
fn self_avatar_each_account_keeps_its_own_backoff() {
    let (root, context) = with_identity("206 avatar two accounts", json!({"userId": "10001"}));
    let identity_path = root.0.join("data").join("identity.json");
    let urls = std::cell::RefCell::new(Vec::new());
    let mut failing = |url: &str| -> Fetched {
        urls.borrow_mut().push(url.to_string());
        Err("unreachable".to_string())
    };
    assert!(current_with(&context, T0, PIXELS, &mut failing).is_none());
    std::fs::write(&identity_path, json!({"userId": "10002"}).to_string()).unwrap();
    assert!(current_with(&context, T0 + 1_000, PIXELS, &mut failing).is_none());
    std::fs::write(&identity_path, json!({"userId": "10001"}).to_string()).unwrap();
    assert!(current_with(&context, T0 + 2_000, PIXELS, &mut failing).is_none());
    assert_eq!(*urls.borrow(), vec![avatar_url("10001"), avatar_url("10002")], "10001 is not fetched again inside its fifteen minutes");
}

#[test]
fn self_avatar_restart_keeps_the_backoff_from_disk() {
    let (root, _context) = with_identity("206 avatar restart", json!({"userId": "10001"}));
    // The reservation is written by the test itself: no fetch has run in this process.
    let reservation = json!({"version": 1, "attempts": [{"uin": "10001", "attemptAt": T0}]});
    std::fs::write(root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE), reservation.to_string()).unwrap();
    let restarted = RuntimeContext::from_root(&root.0).unwrap();
    let calls = Cell::new(0);
    let mut fetch = |_: &str| -> Fetched {
        calls.set(calls.get() + 1);
        Ok(png(2, 2))
    };
    assert!(current_with(&restarted, T0 + 1, PIXELS, &mut fetch).is_none(), "a restarted console keeps the backoff from disk");
    assert_eq!(calls.get(), 0, "no fetch inside the backoff");
}

#[test]
fn self_avatar_unparsable_reservation_file_means_no_fetch() {
    let (root, context) = with_identity("206 avatar bad reservation", json!({"userId": "10001"}));
    std::fs::write(root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE), "not json").unwrap();
    let mut refused = |_: &str| -> Fetched { panic!("an unusable reservation file allows no fetch") };
    assert!(current_with(&context, T0, PIXELS, &mut refused).is_none(), "an unparsable reservation serves the portrait");
    assert!(!root.0.join("data").join(CACHE_FILE).exists());
}

#[test]
fn self_avatar_identity_change_during_the_store_serves_the_portrait() {
    let (root, context) = with_identity("206 avatar store identity", json!({"userId": "10001"}));
    let identity_path = root.0.join("data").join("identity.json");
    crate::self_avatar::STORE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::write(&identity_path, json!({"userId": "10002"}).to_string()).unwrap();
        }));
    });
    let mut fetch = |_: &str| -> Fetched { Ok(png(2, 2)) };
    assert!(current_with(&context, T0, PIXELS, &mut fetch).is_none(), "the account changed while the record was stored");
    // The record that was stored carries the uin it was fetched for, so the new account does not use it.
    assert_eq!(record(&root)["uin"], json!("10001"));
    crate::self_avatar::STORE_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
}

#[test]
fn self_avatar_future_reservation_counts_as_in_force_and_is_restamped() {
    let (root, context) = with_identity("206 avatar future reservation", json!({"userId": "10001"}));
    let attempt = root.0.join("data").join(crate::self_avatar::ATTEMPT_FILE);
    // The clock moved back: her reservation and another account's are dated one minute ahead of now.
    let ahead = T0 + 60_000;
    std::fs::write(&attempt, json!({"version": 1, "attempts": [{"uin": "10001", "attemptAt": ahead}, {"uin": "10002", "attemptAt": ahead}]}).to_string()).unwrap();
    let mut refused = |_: &str| -> Fetched { panic!("a reservation ahead of the clock allows no fetch") };
    assert!(current_with(&context, T0, PIXELS, &mut refused).is_none(), "a reservation one minute ahead is in force");
    let stored: Value = serde_json::from_slice(&std::fs::read(&attempt).unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({"version": 1, "attempts": [{"uin": "10002", "attemptAt": ahead}, {"uin": "10001", "attemptAt": T0}]}),
        "her entry is restamped now, the other account's entry is kept"
    );
}

#[test]
fn self_avatar_future_failure_record_counts_as_in_force_and_is_restamped() {
    let (root, context) = with_identity("206 avatar future failure", json!({"userId": "10001"}));
    std::fs::write(root.0.join("data").join(CACHE_FILE), json!({"version": 1, "uin": "10001", "failedAt": T0 + 60_000}).to_string()).unwrap();
    let mut refused = |_: &str| -> Fetched { panic!("a failure ahead of the clock allows no fetch") };
    assert!(current_with(&context, T0, PIXELS, &mut refused).is_none(), "a failure one minute ahead is in force");
    assert_eq!(record(&root), json!({"version": 1, "uin": "10001", "failedAt": T0}));
}

#[test]
fn self_avatar_fetch_prepares_its_transfer_folder_and_clears_leftovers() {
    let _guard = HookGuard;
    let (root, context) = with_identity("206 avatar transfer folder", json!({"userId": "10001"}));
    let folder = root.0.join("data").join("self-avatar-transfer");
    crate::self_avatar::FETCH_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(|_: &str| -> Fetched { Ok(png(2, 2)) })));
    assert!(crate::self_avatar::current(&context, &|| T0, PIXELS).is_some());
    assert!(folder.is_dir(), "the transfer folder is created inside the data folder");
    // A transfer that did not finish leaves its files behind; the next fetch clears them first.
    let leftover = folder.join("her-fetch-1-2-3.bin");
    std::fs::write(&leftover, b"old body").unwrap();
    std::fs::remove_file(root.0.join("data").join(CACHE_FILE)).unwrap();
    assert!(crate::self_avatar::current(&context, &|| T0 + FAILURE_BACKOFF_MS, PIXELS).is_some());
    assert!(!leftover.exists(), "leftovers of an earlier transfer are removed before the fetch");
}
