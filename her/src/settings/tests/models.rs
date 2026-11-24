// Model and cost settings (#150): each is read through the registry by its real reader.
use super::*;

const MODEL_KEYS: [&str; 6] = [
    "harness.model",
    "harness.maxSteps",
    "harness.maxTokens",
    "memory.digest.model",
    "memory.digest.timeoutSecs",
    "webSearch.model",
];
const ROUTES_KEY: &str = "harness.routes";

fn set_setting(context: &RuntimeContext, key: &str, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": key, "value": value})))
}

/// A fixture runtime whose relay route can be built: the fixture has the `model` dependency, this adds the credential.
fn model_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    std::fs::write(root.0.join("config/harness-api-key"), "fixture-harness-credential-150").unwrap();
    (root, context)
}

/// The model id the next harness turn uses (`ModelRoute::from_runtime` on the loader's configuration), or its error.
fn next_turn_model(context: &RuntimeContext) -> Result<String, String> {
    crate::harness::ModelRoute::from_runtime(context, &context.load_config().unwrap())
        .map(|route| route.model_name().to_string())
}

/// The step and token budgets the next harness turn captures.
fn next_turn_budgets(context: &RuntimeContext) -> (usize, u32) {
    crate::harness::Limits::from_config(&context.load_config().unwrap()).budgets()
}

/// The model id and the request timeout, in seconds, of the next digest run.
fn next_digest(context: &RuntimeContext) -> (Option<String>, u64) {
    let cfg = context.load_config().unwrap();
    (
        crate::harness::one_shot::digest_model_id(&cfg),
        crate::harness::one_shot::digest_timeout(&cfg).as_secs(),
    )
}

/// The model the next web search asks (`aux_mcp` reads the loader's document).
fn next_search_model(context: &RuntimeContext) -> String {
    crate::aux_search::load_search_config(&context.load_config_document().unwrap()).model
}

#[test]
fn model_and_cost_settings_are_live_on_the_next_use() {
    let (root, context) = model_fixture("settings model live");
    // Nothing in the file or the store: the registry's defaults.
    assert_eq!(next_turn_model(&context), Err("harness model is not configured".to_string()));
    assert_eq!(next_turn_budgets(&context), (8, 32_768));
    assert_eq!(next_digest(&context), (None, 120));
    assert_eq!(next_search_model(&context), "deepseek-v4-flash");
    // The file's values while the store holds none.
    write_config(&root, |config| {
        config["harness"] = json!({"model": "file-model", "maxSteps": 3, "maxTokens": 5000});
        config["memory"] = json!({"digest": {"timeoutSecs": 30}});
        config["webSearch"] = json!({"model": "file-search"});
    });
    let config_path = root.0.join("config/config.json");
    let stored_file = std::fs::read(&config_path).unwrap();
    assert_eq!(next_turn_model(&context), Ok("file-model".to_string()));
    assert_eq!(next_turn_budgets(&context), (3, 5000));
    assert_eq!(next_digest(&context), (Some("file-model".to_string()), 30), "the digest falls back to the conversation model");
    assert_eq!(next_search_model(&context), "file-search");

    let changes = [
        ("harness.model", json!("store-model")),
        ("harness.maxSteps", json!(12)),
        ("harness.maxTokens", json!(64_000)),
        ("memory.digest.model", json!("store-digest")),
        ("memory.digest.timeoutSecs", json!(45)),
        ("webSearch.model", json!("store-search")),
    ];
    // The first write seeds every value the file holds, in registry order, as revision 1; the digest model is not in
    // the file, so its change is its first.
    let seeded = ["harness.model", "harness.maxSteps", "harness.maxTokens", "memory.digest.timeoutSecs", "webSearch.model"];
    for (key, value) in &changes {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 200, "{key}: {body}");
        let revision = if seeded.contains(key) { 2 } else { 1 };
        assert_eq!(body, json!({"ok": true, "setting": key, "value": value, "revision": revision, "changed": true}), "{key}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    assert_eq!(next_turn_budgets(&context), (12, 64_000));
    assert_eq!(next_digest(&context), (Some("store-digest".to_string()), 45));
    assert_eq!(next_search_model(&context), "store-search");
    let file = stored_config(&root);
    for (key, value) in &changes {
        let pointer = crate::settings::find(key).unwrap().pointer;
        assert_eq!(file.pointer(pointer), Some(value), "{key} is written back into config.json");
    }
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), stored_file, "config.json from before the first write is kept");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), seeded.len() + changes.len(), "{rows:?}");
    for (row, key) in rows.iter().zip(seeded) {
        assert_eq!((row["key"].as_str(), row["source"].as_str(), row["actor"].as_str()), (Some(key), Some("migration"), Some("system")));
    }
    for (row, (key, value)) in rows[seeded.len()..].iter().zip(&changes) {
        assert_eq!(
            (row["key"].as_str(), row["source"].as_str(), row["scope"].as_str()),
            (Some(*key), Some("console"), Some("global"))
        );
        assert_eq!(row["new"], json!(value.to_string()), "{key}");
    }
    for key in MODEL_KEYS {
        let view = setting_view(&context, key);
        assert_eq!((view["group"].as_str(), view["applies"].as_str()), (Some("model and cost"), Some("live")), "{key}");
        assert_eq!(view["stored"], view["value"], "{key}: the next use reads the stored value");
    }
    assert_eq!(setting_view(&context, "harness.model")["kind"], json!({"type": "model id", "maxChars": 200}));
    assert_eq!(setting_view(&context, "harness.model")["default"], Value::Null);
    assert_eq!(setting_view(&context, "webSearch.model")["default"], "deepseek-v4-flash");
    assert_eq!(setting_view(&context, "harness.maxSteps")["kind"], json!({"type": "integer", "min": 1, "max": 64}));
    assert_eq!(setting_view(&context, "harness.maxTokens")["default"], 32_768);
    let request = json!({"setting": "harness.maxTokens", "value": 1_000_000});
    let (status, body) = agent_request(&context, "POST", "/api/settings/set", Some(&request), "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    assert_eq!(next_turn_budgets(&context), (12, 64_000), "an agent never changes its own budget");
}

#[test]
fn model_and_cost_settings_refuse_invalid_values_and_keep_the_old_value() {
    let (root, context) = model_fixture("settings model refused");
    let accepted = [
        ("harness.model", json!("x".repeat(200)), json!("x".repeat(200))),
        ("harness.model", json!("  padded-model  "), json!("padded-model")),
        ("harness.model", json!("模型".repeat(100)), json!("模型".repeat(100))),
        ("harness.maxSteps", json!(1), json!(1)),
        ("harness.maxSteps", json!(64), json!(64)),
        ("harness.maxTokens", json!(1), json!(1)),
        ("harness.maxTokens", json!(1_000_000), json!(1_000_000)),
        ("memory.digest.timeoutSecs", json!(1), json!(1)),
        ("memory.digest.timeoutSecs", json!(600), json!(600)),
    ];
    for (key, value, stored) in &accepted {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 200, "{key} {value}: {body}");
        assert_eq!(&body["value"], stored, "{key} {value}: stored trimmed");
    }
    let model = "harness.model must be a model id of 1 to 200 characters without control characters";
    let tokens = format!("harness.maxTokens must be an integer from 1 to {}", 1_000_000);
    let refused = [
        ("harness.model", json!(""), model),
        ("harness.model", json!("   "), model),
        ("harness.model", json!("x".repeat(201)), model),
        ("harness.model", json!("模".repeat(201)), model),
        ("harness.model", json!("line\nbreak"), model),
        ("harness.model", json!("bell\u{7}"), model),
        ("harness.model", json!(7), model),
        ("harness.model", Value::Null, model),
        ("harness.model", json!(true), model),
        ("harness.model", json!(["m"]), model),
        ("harness.model", json!({"id": "m"}), model),
        ("webSearch.model", json!(""), "webSearch.model must be a model id of 1 to 200 characters without control characters"),
        (
            "memory.digest.model",
            json!("tab\tinside"),
            "memory.digest.model must be a model id of 1 to 200 characters without control characters",
        ),
        ("harness.maxSteps", json!(0), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!(65), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!(8.5), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!("8"), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxTokens", json!(0), tokens.as_str()),
        ("harness.maxTokens", json!(1_000_001), tokens.as_str()),
        ("memory.digest.timeoutSecs", json!(0), "memory.digest.timeoutSecs must be an integer from 1 to 600"),
        ("memory.digest.timeoutSecs", json!(601), "memory.digest.timeoutSecs must be an integer from 1 to 600"),
    ];
    for (key, value, error) in &refused {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 400, "{key} {value}: {body}");
        assert_eq!(body["error"], *error, "{key} {value}");
    }
    assert_eq!(next_turn_model(&context), Ok("模型".repeat(100)), "every refusal keeps the old value");
    assert_eq!(next_turn_budgets(&context), (64, 1_000_000));
    assert_eq!(next_digest(&context), (Some("模型".repeat(100)), 600));
    assert_eq!(next_search_model(&context), "deepseek-v4-flash");
    assert_eq!(audit_rows(&root).len(), accepted.len(), "a refusal writes no audit row");
}

#[test]
fn settings_followups_digest_timeout_hint_describes_the_route_walk() {
    let (_root, context) = settings_fixture("digest route timeout hint");
    let view = setting_view(&context, "memory.digest.timeoutSecs");
    let hint = view["hint"].as_str().unwrap();
    assert!(
        hint.contains("Total seconds") && hint.contains("walk the model route chain"),
        "the digest timeout hint must describe the total route walk: {hint}"
    );
}

#[test]
fn settings_followups_undo_restores_the_clamped_file_value() {
    for (file_value, clamped) in [(0, 1), (100, 64)] {
        let (root, context) = settings_fixture(&format!("clamped undo {file_value}"));
        write_config(&root, |config| config["harness"] = json!({"maxSteps": file_value}));
        assert_eq!(next_turn_budgets(&context).0, clamped);

        let key = "harness.maxSteps";
        let (status, body) = set_setting(&context, key, json!(12));
        assert_eq!(status, 200, "{body}");
        assert_eq!(next_turn_budgets(&context).0, 12);
        let (entries, _) = history_page(&context, "?setting=harness.maxSteps");
        let (status, body) = admin_request(
            &context,
            "POST",
            "/api/settings/undo",
            Some(&json!({"entry": entries[0]["entry"]})),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            next_turn_budgets(&context).0,
            clamped,
            "undo must restore the clamped file value for {file_value}"
        );
        assert_eq!(stored_config(&root)["harness"]["maxSteps"], json!(clamped));
    }
}

#[test]
fn model_and_cost_readers_take_defaults_and_bounds_from_the_registry() {
    let registered = |key: &str| crate::settings::find(key).unwrap_or_else(|| panic!("{key} is registered"));
    let steps = registered("harness.maxSteps");
    let tokens = registered("harness.maxTokens");
    let timeout = registered("memory.digest.timeoutSecs");
    let harness_model = registered("harness.model");
    registered("memory.digest.model");
    registered("webSearch.model");
    // A `config.json` integer outside the bounds is clamped and any other value reads as the default: each reader's
    // rule before the registry owned it.
    for (value, expected) in [
        (json!(0), 1),
        (json!(1), 1),
        (json!(8), 8),
        (json!(64), 64),
        (json!(65), 64),
        (json!(u64::MAX), 64),
        (json!(-1), 8),
        (json!(5.5), 8),
        (json!("8"), 8),
        (Value::Null, 8),
    ] {
        let cfg = json!({"harness": {"maxSteps": value}});
        assert_eq!(crate::harness::Limits::from_config(&cfg).budgets().0, expected, "maxSteps {value}");
        assert_eq!(steps.integer(&cfg), expected as i64, "maxSteps {value}");
    }
    for (value, expected) in [
        (json!(0), 1),
        (json!(1), 1),
        (json!(32_768), 32_768),
        (json!(1_000_000), 1_000_000),
        (json!(1_000_001), 1_000_000),
        (json!(-5), 32_768),
        (json!("x"), 32_768),
    ] {
        let cfg = json!({"harness": {"maxTokens": value}});
        assert_eq!(crate::harness::Limits::from_config(&cfg).budgets().1, expected, "maxTokens {value}");
        assert_eq!(tokens.integer(&cfg), i64::from(expected), "maxTokens {value}");
    }
    for (value, expected) in
        [(json!(0), 1), (json!(1), 1), (json!(600), 600), (json!(601), 600), (json!(-1), 120), (json!(2.5), 120)]
    {
        let cfg = json!({"memory": {"digest": {"timeoutSecs": value}}});
        assert_eq!(
            crate::harness::one_shot::digest_timeout(&cfg).as_secs(),
            expected,
            "timeoutSecs {value}"
        );
        assert_eq!(timeout.integer(&cfg), expected as i64, "timeoutSecs {value}");
    }
    assert_eq!(crate::harness::Limits::from_config(&json!({})).budgets(), (8, 32_768));
    assert_eq!(crate::harness::Limits::from_config(&json!({"harness": "x"})).budgets(), (8, 32_768));
    assert_eq!(crate::harness::one_shot::digest_timeout(&json!({})).as_secs(), 120);
    // A model id is trimmed; empty, not text, too long or with a control character reads as not configured.
    for (value, expected) in [
        (json!(" m1 "), Some("m1")),
        (json!(""), None),
        (json!("  "), None),
        (json!(5), None),
        (json!("a\nb"), None),
        (json!("x".repeat(201)), None),
    ] {
        let cfg = json!({"harness": {"model": value}});
        assert_eq!(harness_model.text(&cfg).as_deref(), expected, "harness.model {value}");
        assert_eq!(
            crate::harness::one_shot::digest_model_id(&cfg).as_deref(),
            expected,
            "digest fallback {value}"
        );
    }
    let digest = |model: Value| {
        crate::harness::one_shot::digest_model_id(&json!({
            "memory": {"digest": {"model": model}},
            "harness": {"model": "h"}
        }))
    };
    assert_eq!(digest(json!(" d ")).as_deref(), Some("d"));
    assert_eq!(digest(json!("")).as_deref(), Some("h"), "an empty digest model uses the conversation model");
    assert_eq!(digest(json!("a\u{1}b")).as_deref(), Some("h"));
    assert_eq!(crate::harness::one_shot::digest_model_id(&json!({})), None);
    let search = |config: Value| crate::aux_search::load_search_config(&config).model;
    assert_eq!(search(json!({})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": " m "}})), "m");
    assert_eq!(search(json!({"webSearch": []})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": "a\u{1}b"}})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": "x".repeat(201)}})), "deepseek-v4-flash");
    // The route a turn builds carries the trimmed id; a blank id is not configured.
    let (_root, context) = model_fixture("settings model reader");
    let turn_model = |model: &str| {
        crate::harness::ModelRoute::from_runtime(&context, &json!({"harness": {"model": model}}))
            .map(|built| built.model_name().to_string())
    };
    assert_eq!(turn_model(" padded "), Ok("padded".to_string()));
    assert_eq!(turn_model("  "), Err("harness model is not configured".to_string()));
}

#[test]
fn model_and_cost_settings_have_no_reader_default() {
    // Each reader reads its setting through the registry (`settings/registry/model.rs`), never by its own pointer or
    // default.
    let readers = [
        ("harness.rs", include_str!("../../harness.rs")),
        ("harness/routes.rs", include_str!("../../harness/routes.rs")),
        ("harness/one_shot.rs", include_str!("../../harness/one_shot.rs")),
        ("aux_search.rs", include_str!("../../aux_search.rs")),
    ];
    let owned = [
        "/harness/model",
        "/harness/routes",
        "/harness/maxSteps",
        "/harness/maxTokens",
        "/memory/digest/model",
        "/memory/digest/timeoutSecs",
        "/webSearch/model",
        "deepseek-v4-flash",
    ];
    for (file, source) in readers {
        for needle in owned {
            assert!(!source.contains(needle), "{file} reads {needle} itself; read it through settings::registry()");
        }
    }
    let registry = include_str!("../registry/model.rs");
    for needle in owned {
        assert!(registry.contains(needle), "settings/registry/model.rs owns {needle}");
    }
}

#[test]
fn harness_route_list_uses_store_on_next_read_and_keeps_ordered_history() {
    let (root, context) = model_fixture("route list upgrade");
    assert_eq!(set_quiet(&context, json!(4500)).0, 200, "start with an existing settings store");
    let file_routes = json!([
        {"id": "first", "model": " file-first ", "note": "ignored"},
        {"id": "second", "model": "file-second", "reasoningEffort": "high", "maxOutputTokens": 1, "retry429": false}
    ]);
    let seeded = json!([
        {"id": "first", "model": "file-first", "reasoningEffort": null, "maxOutputTokens": 4096, "retry429": true},
        {"id": "second", "model": "file-second", "reasoningEffort": "high", "maxOutputTokens": 1, "retry429": false}
    ]);
    write_config(&root, |config| config["harness"] = json!({"model": "fallback", "routes": file_routes.clone()}));
    assert_eq!(next_turn_model(&context).as_deref(), Ok("file-first"));
    assert_eq!(setting_view(&context, ROUTES_KEY)["stored"], Value::Null, "reading alone is not a migration");
    let (status, reply) = set_setting(&context, ROUTES_KEY, seeded.clone());
    assert_eq!((status, &reply["revision"], &reply["changed"]), (200, &json!(1), &json!(false)), "{reply}");
    assert_eq!(stored_config(&root)["harness"]["routes"], file_routes, "seeding keeps the equivalent legacy spelling");
    assert_eq!(StoreProbe::at(&root.0).user_version(), 1, "a global route list uses the existing schema");

    let reversed = json!([seeded[1], seeded[0]]);
    let (status, reply) = set_setting(&context, ROUTES_KEY, reversed.clone());
    assert_eq!((status, &reply["revision"], &reply["value"]), (200, &json!(2), &reversed), "{reply}");
    assert_eq!(stored_config(&root)["harness"]["routes"], reversed, "the older reader gets the new order too");
    write_config(&root, |config| config["harness"]["routes"] = file_routes);
    let view = setting_view(&context, ROUTES_KEY);
    assert_eq!(view["kind"], json!({"type": "route list", "minItems": 1, "maxItems": 8}));
    assert_eq!((&view["stored"], &view["value"], &view["default"]), (&reversed, &reversed, &Value::Null));
    assert_eq!(next_turn_model(&context).as_deref(), Ok("file-second"));
    assert_eq!(next_digest(&context).0.as_deref(), Some("file-second"));

    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, reply) = post(&context, "/api/settings/reset", json!({"setting": ROUTES_KEY}));
    assert_eq!((status, &reply["value"]), (200, &Value::Null), "{reply}");
    assert_eq!(reply["mirrorError"], "runtime config is unavailable");
    assert_eq!(stored_config(&root)["harness"]["routes"][0]["id"], "first", "the file still holds the old list");
    let context = RuntimeContext::from_root(&root.0).unwrap();
    assert_eq!(next_turn_model(&context).as_deref(), Ok("fallback"), "a restart reads the committed reset");
    finish_settings_write_back(&context);
    assert_eq!(stored_config(&root)["harness"].get("routes"), Some(&Value::Null), "start finishes writing back the reset");

    let (entries, _) = history_page(&context, "?setting=harness.routes");
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert_eq!((&entries[2]["source"], &entries[2]["new"]), (&json!("migration"), &seeded));
    assert_eq!((&entries[1]["source"], &entries[1]["old"], &entries[1]["new"]), (&json!("console"), &seeded, &reversed));
    assert_eq!((&entries[0]["source"], &entries[0]["old"], &entries[0]["new"]), (&json!("reset"), &reversed, &Value::Null));
    let (status, reply) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!((status, &reply["stored"]), (200, &reversed), "{reply}");
    assert_eq!(stored_config(&root)["harness"]["routes"], reversed);
    assert_eq!(next_turn_model(&context).as_deref(), Ok("file-second"));
    let (entries, _) = history_page(&context, "?setting=harness.routes");
    assert_eq!((&entries[0]["source"], &entries[0]["new"]), (&json!("undo"), &reversed));
}

#[test]
fn harness_route_list_refuses_invalid_inputs_without_echoing_them() {
    let (root, context) = model_fixture("invalid route list");
    let invalid = json!([{"id": "first", "model": "model-a"}, {"id": "second", "model": "model-b", "retry429": "bad-input"}]);
    write_config(&root, |config| {
        config["harness"] = json!({"model": "fallback", "routes": invalid.clone()});
    });
    let error = "harness route 2 is invalid (retry429)";
    assert_eq!(next_turn_model(&context), Err(error.to_string()), "an invalid file list cannot fall back");
    let (status, reply) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!((status, &reply["error"], &reply["routes"]), (200, &json!(error), &json!([])));
    assert!(reply["conversations"].as_array().is_some_and(|rows| !rows.is_empty()));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let bounds = "harness routes must be a list of 1 to 8 routes";
    for (value, expected) in [
        (invalid, error),
        (json!([{"id": "a", "model": "first"}, {"id": "a", "model": "second"}]), "harness route 2 is invalid (id)"),
        (json!([]), bounds),
        (Value::Null, bounds),
        (json!({"id": "a", "model": "first"}), bounds),
    ] {
        let (status, reply) = set_setting(&context, ROUTES_KEY, value);
        assert_eq!(status, 400, "{reply}");
        assert_eq!(reply["error"], expected, "route validation reports a position and field or the list bounds");
    }
    let request = json!({"setting": ROUTES_KEY, "value": [{"id": "agent", "model": "model-a"}]});
    let (status, reply) = agent_request(&context, "POST", "/api/settings/set", Some(&request), "fixture-agent-token");
    assert_eq!(status, 403, "{reply}");
    assert!(!settings_db(&root).exists());
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before);
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    assert!(history_page(&context, "?setting=harness.routes").0.is_empty(), "an invalid file list is not seeded");
    let valid = json!([{"id": "new", "model": "model-new"}]);
    write_config(&root, |config| config["harness"]["routes"] = valid);
    StoreProbe::at(&root.0).insert_value(ROUTES_KEY, "[]");
    assert_eq!(next_turn_model(&context).as_deref(), Ok("model-new"), "an invalid stored list does not replace the file");
}

#[test]
fn harness_route_list_masks_credentials_in_settings_responses() {
    let (root, context) = settings_fixture("route list credential masking");
    let credential = "fixture-model-credential";
    std::fs::write(root.0.join("config/harness-api-key"), credential).unwrap();
    let routes = json!([{"id": format!("route-{credential}"), "model": format!("model-{credential}")}]);
    write_config(&root, |config| config["harness"] = json!({"routes": routes}));
    assert_eq!(set_quiet(&context, json!(4500)).0, 200, "seed the list");
    let view = setting_view(&context, ROUTES_KEY);
    assert_eq!(view["stored"][0]["id"], "route-[REDACTED]", "the stored route list masks the credential");
    assert_eq!(view["value"][0]["model"], "model-[REDACTED]");
    assert_eq!(view["redacted"], true);
    assert!(!view.to_string().contains(credential));
    let edit = json!([{"id": "new", "model": format!("new-{credential}")}]);
    let (status, reply) = set_setting(&context, ROUTES_KEY, edit);
    assert_eq!((status, &reply["value"][0]["model"]), (200, &json!("new-[REDACTED]")));
    let (entries, _) = history_page(&context, "?setting=harness.routes");
    assert_eq!(entries[0]["old"][0]["model"], "model-[REDACTED]");
    assert_eq!(entries[0]["new"][0]["model"], "new-[REDACTED]");
    assert!(!json!(entries).to_string().contains(credential));
    let (status, reply) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!((status, &reply["stored"][0]["model"]), (200, &json!("model-[REDACTED]")));
    assert_eq!(stored_config(&root)["harness"]["routes"][0]["model"], format!("model-{credential}"));
}

#[test]
fn stored_model_and_cost_rows_the_registry_refuses_are_not_applied() {
    // Rows written by another release or by hand are checked again when overlaid, and never clamped.
    let (root, context) = model_fixture("settings model old rows");
    let config_path = root.0.join("config/config.json");
    let file_before = std::fs::read(&config_path).unwrap();
    for (key, value) in [("harness.model", json!("store-model")), ("harness.maxSteps", json!(12)), ("memory.digest.timeoutSecs", json!(45))] {
        let (status, body) = set_setting(&context, key, value);
        assert_eq!(status, 200, "{key}: {body}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    // config.json as it was before the write-backs, so only the store could supply these values.
    std::fs::write(&config_path, &file_before).unwrap();
    let store = StoreProbe::at(&root.0);
    store.set_value("harness.model", &json!("bad\nid").to_string());
    store.set_value("harness.maxSteps", "65");
    store.set_value("memory.digest.timeoutSecs", "not json");
    store.insert_value("harness.maxTokens", "\"4096\"");
    assert_eq!(
        next_turn_model(&context),
        Err("harness model is not configured".to_string()),
        "a stored model id with a control character is not applied"
    );
    assert_eq!(next_turn_budgets(&context), (8, 32_768), "an out-of-range or mistyped stored value is not applied, not clamped");
    assert_eq!(next_digest(&context), (None, 120));
    for key in ["harness.model", "harness.maxSteps", "harness.maxTokens", "memory.digest.timeoutSecs"] {
        assert_eq!(setting_view(&context, key)["stored"], Value::Null, "{key}");
    }
    assert_eq!(setting_view(&context, "harness.model")["value"], Value::Null, "not configured");
    let (status, body) = set_setting(&context, "harness.model", json!("fixed-model"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
    assert_eq!(next_turn_model(&context), Ok("fixed-model".to_string()));
    assert_eq!(audit_rows(&root).last().unwrap()["old"], json!("bad\nid").to_string(), "the audit keeps what was stored");
}

#[test]
fn model_id_length_counts_characters_not_utf16_units() {
    // The limit is 200 characters (`settings/kinds/model_id.rs` `model_id`), not UTF-16 units: a character outside the
    // BMP is two units.
    let (_root, context) = model_fixture("settings model length");
    let astral = |count: usize| "\u{1F600}".repeat(count);
    let (status, body) = set_setting(&context, "harness.model", json!(astral(101)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], json!(astral(101)), "101 characters (202 UTF-16 units) are accepted");
    assert_eq!(next_turn_model(&context), Ok(astral(101)));
    let (status, body) = set_setting(&context, "harness.model", json!(astral(201)));
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["error"],
        "harness.model must be a model id of 1 to 200 characters without control characters"
    );
    assert_eq!(next_turn_model(&context), Ok(astral(101)), "a refused id keeps the old value");
}

#[test]
fn model_and_cost_settings_written_at_once_all_commit() {
    let (root, context) = model_fixture("settings model concurrent");
    let apply = |key: &str, value: Value| crate::settings::apply(&context, key, &value, "administrator", "console");
    let applied = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            for _ in 0..20 {
                let (steps, tokens) = next_turn_budgets(&context);
                assert!(steps == 8 && matches!(tokens, 32_768 | 4096), "{steps} {tokens}");
            }
        });
        let writers = [
            scope.spawn(|| apply("harness.model", json!("m-a"))),
            scope.spawn(|| apply("harness.maxTokens", json!(4096))),
            scope.spawn(|| apply("webSearch.model", json!("s-a"))),
        ];
        reader.join().unwrap();
        writers.map(|writer| writer.join().unwrap().unwrap())
    });
    for (index, applied) in applied.iter().enumerate() {
        assert!(applied.changed && applied.revision == 1, "writer {index}");
    }
    assert_eq!(next_turn_model(&context), Ok("m-a".to_string()));
    assert_eq!(next_turn_budgets(&context), (8, 4096));
    assert_eq!(next_search_model(&context), "s-a");
    let mut keys: Vec<String> = audit_rows(&root).iter().map(|row| row["key"].as_str().unwrap().to_string()).collect();
    keys.sort();
    assert_eq!(keys, ["harness.maxTokens", "harness.model", "webSearch.model"]);
}

#[test]
fn model_setting_cut_off_before_its_audit_row_leaves_the_old_model() {
    let (root, context) = model_fixture("settings model rollback");
    let (status, body) = set_setting(&context, "harness.model", json!("first-model"));
    assert_eq!(status, 200, "{body}");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = set_setting(&context, "harness.model", json!("second-model"));
    assert_eq!(status, 503, "{body}");
    assert_eq!(next_turn_model(&context), Ok("first-model".to_string()), "the next turn uses the last committed model");
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    let (status, body) = set_setting(&context, "harness.model", json!("second-model"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
}

#[test]
fn a_store_from_before_the_model_settings_is_read_and_extended() {
    // A store an earlier release wrote holds older keys only; a full restore brings such a store back byte for byte.
    let (root, context) = model_fixture("settings model older store");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was left");
    }
    let older = std::fs::read(settings_db(&root)).unwrap();
    // This release reads it: the older key applies, the new keys read the file or their defaults.
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(next_turn_budgets(&context), (8, 32_768));
    assert_eq!(next_turn_model(&context), Err("harness model is not configured".to_string()));
    // And extends it: the same schema version, the older row untouched.
    let (status, body) = set_setting(&context, "harness.maxSteps", json!(12));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_turn_budgets(&context), (12, 32_768));
    assert_eq!(next_use_quiet(&context), Some(4500));
    let version = StoreProbe::at(&root.0).user_version();
    assert_eq!(version, 1, "these settings need no new schema");
    // Restoring the older store over it brings back exactly what it held.
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was left");
    }
    std::fs::write(settings_db(&root), &older).unwrap();
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(setting_view(&context, "harness.maxSteps")["stored"], Value::Null, "the restored store has no model settings");
    assert_eq!(next_turn_budgets(&context), (12, 32_768), "config.json holds the last written-back value");
    let (status, body) = set_setting(&context, "harness.model", json!("after-restore"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_turn_model(&context), Ok("after-restore".to_string()));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 3, "the restored audit row, the seeded copy of the file's harness.maxSteps and the new one: {rows:?}");
    assert_eq!(
        (rows[1]["key"].clone(), rows[1]["source"].clone(), rows[1]["new"].clone()),
        (json!("harness.maxSteps"), json!("migration"), json!("12"))
    );
    assert_eq!(next_turn_budgets(&context), (12, 32_768));
}
