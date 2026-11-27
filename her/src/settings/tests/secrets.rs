use super::*;

const SEARCH: &str = "webSearch.apiKey";
const CLI: &str = "cliModel.apiKey";
const RELAY: &str = "http://127.0.0.1:49102";
const KEY: &str = "fixture-search-credential";
const CLI_KEY: &str = "fixture-cli-credential";

fn put(context: &RuntimeContext, key: &str, value: &str) -> (u16, Value) {
    post(context, "/api/settings/set", json!({"setting": key, "value": value}))
}

#[test]
fn ticket219_core_registry_and_chat() {
    let (_root, context) = chat_fixture("credential registry");
    let registered: Vec<_> = crate::settings::registry()
        .filter(|setting| setting.secret)
        .map(|setting| setting.key)
        .collect();
    assert_eq!(registered, [SEARCH, CLI, "harness.apiKey", "console.administratorPassword"]);
    assert!(
        crate::settings::registry().all(|setting| !setting.secret || !setting.chat),
        "a credential cannot be chat-visible",
    );
    let all = chat(&context, "settings-get", json!({})).unwrap().to_string();
    for key in registered {
        assert!(!all.contains(key), "a credential was listed in chat");
        for action in ["settings-get", "settings-set"] {
            let reply = chat(&context, action, json!({"settingKey": key, "value": KEY})).unwrap_err();
            assert!(!reply.contains(key) && !reply.contains(KEY), "chat named the credential");
        }
    }
}

#[test]
fn ticket219_core_key_bounds_are_closed() {
    let setting = crate::settings::find(SEARCH).unwrap();
    for (size, accepted) in [(7, false), (8, true), (4096, true), (4097, false)] {
        assert_eq!(
            setting.normalize(&json!("x".repeat(size))).is_some(),
            accepted,
            "credential length boundary {size}",
        );
    }
}

#[test]
fn ticket219_core_view_only_reports_presence() {
    let (root, context) = settings_fixture("credential list");
    write_config(&root, |config| config["webSearch"]["apiKey"] = json!("legacy-search-fixture"));
    assert_eq!(setting_view(&context, SEARCH)["isSet"], false);
    assert_eq!(put(&context, SEARCH, KEY).0, 200);
    let entry = setting_view(&context, SEARCH);
    assert_eq!(entry["isSet"], true);
    assert!(["value", "stored", "default"].iter().all(|field| entry.get(field).is_none()), "credential list exposed value fields");
    assert!(!entry.to_string().contains(KEY));
}

#[test]
fn ticket219_core_set_reset_and_validation_disclose_no_value() {
    let (_root, context) = settings_fixture("credential set");
    let (status, reply) = put(&context, SEARCH, KEY);
    assert_eq!(status, 200);
    assert_eq!(reply["isSet"], true, "set must return presence only");
    assert!(reply.get("value").is_none() && !reply.to_string().contains(KEY));
    assert_eq!(put(&context, SEARCH, KEY).1["changed"], false);
    for invalid in ["small", "fixture\r\ncredential"] {
        let (status, reply) = put(&context, SEARCH, invalid);
        assert_eq!(status, 400);
        assert!(!reply.to_string().contains(invalid));
    }
    let (status, reply) = post(&context, "/api/settings/reset", json!({"setting": SEARCH}));
    assert_eq!(status, 200);
    assert_eq!(reply["isSet"], false);
    assert!(reply.get("value").is_none());
    assert_eq!(setting_view(&context, SEARCH)["isSet"], false);
}

#[test]
fn ticket219_core_overlay_and_mirror_exclude_credentials() {
    let (root, context) = settings_fixture("credential projection");
    write_config(&root, |config| config["webSearch"]["apiKey"] = json!("legacy-search-fixture"));
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    assert_eq!(put(&context, SEARCH, KEY).0, 200);
    assert_eq!(set_quiet(&context, json!(4510)).0, 200);
    let effective = context.load_config_document().unwrap();
    let mut pending_write = stored_config(&root);
    crate::settings::mirror_into(&context, &mut pending_write).unwrap();
    let clean = [effective, pending_write, stored_config(&root)].iter().all(|document| {
        document.pointer("/webSearch/apiKey") == Some(&json!("legacy-search-fixture"))
            && document.pointer("/cliModel/apiKey").is_none()
    });
    assert!(clean, "credential crossed the configuration overlay or mirror");
}

#[test]
fn ticket219_core_config_undo_restores_the_legacy_search_literal() {
    let current = json!({"webSearch":{"apiKey":"fixture-current-literal","model":"current-model"}});
    let mut restored = json!({"webSearch":{"apiKey":"fixture-previous-literal","model":"previous-model"}});
    crate::settings::keep_registry_values(&current, &mut restored).unwrap();
    assert_eq!(restored["webSearch"]["apiKey"], "fixture-previous-literal", "config undo kept the current legacy literal");
    assert_eq!(restored["webSearch"]["model"], "current-model");
}

#[test]
fn ticket219_core_config_audit_tracks_the_legacy_search_literal() {
    let current = json!({"webSearch":{"apiKey":"fixture-current-literal","model":"current-model"}});
    let earlier = json!({"webSearch":{"apiKey":"fixture-previous-literal","model":"previous-model"}});
    let stripped = crate::settings::registry_free(&current);
    assert_ne!(stripped, crate::settings::registry_free(&earlier), "config audit ignored a changed legacy search literal");
    assert!(stripped.pointer("/webSearch/model").is_none());
}

#[test]
fn ticket219_core_raw_audit_and_paged_history_are_redacted() {
    let (root, context) = settings_fixture("credential history");
    assert_eq!(put(&context, SEARCH, KEY).0, 200);
    assert_eq!(put(&context, SEARCH, "fixture-search-rotation").0, 200);
    let rows = audit_rows(&root);
    let marker = json!(json!("(hidden)").to_string());
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["old"] == marker && row["new"] == marker), "audit persisted a credential");
}

#[test]
fn ticket219_core_history_redacts_imported_rows_on_every_page() {
    let (root, context) = settings_fixture("credential history cursor");
    assert_eq!(set_quiet(&context, json!(4700)).0, 200);
    let old = Some(json!(KEY).to_string());
    let new = Some(json!(CLI_KEY).to_string());
    let rows: Vec<_> = (0..101)
        .map(|i| AuditRow::test(SEARCH, old.clone(), new.clone(), &format!("fixture-history-{i}")))
        .collect();
    StoreProbe::at(&root.0).insert_audit(&rows);
    let (first, next) = history_page(&context, "?setting=webSearch.apiKey");
    let (rest, final_page) = history_page(&context, &format!("?setting=webSearch.apiKey&before={next}"));
    assert_eq!((first.len(), rest.len(), final_page), (100, 1, Value::Null));
    assert!(first.iter().chain(&rest).all(|row| row["old"] == "(hidden)" && row["new"] == "(hidden)"), "history exposed an imported credential row");
}

#[test]
fn ticket219_core_undo_refuses_a_credential_change() {
    let (_root, context) = settings_fixture("credential undo");
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    let entry = history_page(&context, "?setting=cliModel.apiKey").0[0]["entry"].clone();
    let (status, reply) = post(&context, "/api/settings/undo", json!({"entry": entry}));
    assert_eq!(status, 409);
    assert_eq!(reply["error"], "this entry cannot be undone");
    assert_eq!(crate::settings::secret_value(&context, &crate::settings::CLI_MODEL_API_KEY, RELAY).unwrap().as_deref(), Some(CLI_KEY));
}

#[test]
fn ticket219_core_prompt_text_refuses_both_credentials() {
    let (root, context) = settings_fixture("prompt credential refusal");
    let model_key = "fixture-model-credential";
    std::fs::write(root.0.join("config/harness-api-key"), model_key).unwrap();
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    for key in [model_key, CLI_KEY] {
        let (status, reply) = put(&context, "prompt.notAClerk", &format!("prefix {key} suffix"));
        assert_eq!(status, 400, "prompt text accepted a credential");
        assert_eq!(reply["error"], "setting text contains a credential");
        assert!(!reply.to_string().contains(key));
    }
}

#[test]
fn ticket219_core_model_view_preserves_credential_boundaries() {
    let (_root, context) = settings_fixture("prompt credential delivery");
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    let mut cfg = context.load_config().unwrap();
    cfg["socialV2"]["context"]["stripFromModelText"] = json!(["REMOVE"]);
    let setting = crate::settings::find("prompt.notAClerk").unwrap();
    let mut member = &mut cfg;
    for part in setting.pointer.split('/').skip(1) {
        member = &mut member[part];
    }
    *member = json!("fixture-cli-REMOVEcredential");
    let visible = crate::prompt_text::model_text(&context, &cfg, setting.key);
    assert_eq!(visible, "fixture-cli-REMOVEcredential");
    assert!(!visible.contains(CLI_KEY), "delivery exposed a credential in prompt text");
    let handover = crate::engine_switch::safe_text(&context, &cfg, "before fixture-cli-REMOVEcredential after", 100);
    assert_eq!(handover.as_deref(), Some("before fixture-cli-【已隐藏】credential after"));
}

#[test]
fn ticket219_core_route_list_keeps_endpoint_keys_out_of_settings_history() {
    let (root, context) = settings_fixture("route list endpoint credential");
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    let routes = json!([{"id": "first", "model": format!("model-{CLI_KEY}")}]);
    write_config(&root, |config| config["harness"]["routes"] = routes.clone());
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    let view = setting_view(&context, "harness.routes");
    assert_eq!(view["value"][0]["model"], "model-[REDACTED]");
    assert_eq!(view["redacted"], true);
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "harness.routes", "value": routes}));
    assert_eq!(status, 400);
    assert!(!reply.to_string().contains(CLI_KEY));
    assert!(history_page(&context, "?setting=harness.routes").0.is_empty());
    assert!(!json!(audit_rows(&root)).to_string().contains(CLI_KEY));
}

#[test]
fn ticket219_core_search_reports_an_unreadable_credential_store() {
    let (root, context) = settings_fixture("search credential store error");
    std::fs::write(root.0.join("data/settings.db"), b"unreadable fixture").unwrap();
    let config = json!({"webSearch": {
        "provider": "deepseek",
        "baseURL": "https://search.example.test/v1",
        "apiKey": "fixture-legacy-fallback"
    }});
    let net = crate::aux_search::Network {
        resolver: &|_| panic!("store failure must precede DNS"),
        transfer: &|_, _, _| panic!("store failure must precede transfer"),
    };
    let result = crate::aux_search::web_search("store error fixture", &config, &context, &net);
    assert_eq!(result.unwrap_err(), "主搜索凭据存储不可用", "search misreported the settings store error");
}

#[test]
fn ticket219_core_search_sends_the_credential_only_to_its_endpoint() {
    use crate::aux_fetch::Head;
    let (root, context) = settings_fixture("search credential binding");
    let web = |endpoint| json!({"webSearch": {"provider": "deepseek", "baseURL": endpoint, "apiKey": "fixture-search-literal"}});
    let selected = web("https://search.example.test/v1");
    let changed = web("https://replacement.example.test/v1");
    write_config(&root, |cfg| cfg["webSearch"] = selected["webSearch"].clone());
    assert_eq!(put(&context, SEARCH, KEY).0, 200);
    let seen = std::cell::RefCell::new(Vec::new());
    let transfer: &crate::aux_fetch::TransferFn<'_> = &|request, head, body| {
        seen.borrow_mut().push(request.headers.iter().find(|(name, _)| name == "x-api-key").unwrap().1.clone());
        let response = Head { status: 401, location: None };
        let _ = head(&response);
        let _ = body(br#"{"error":"unused fixture"}"#);
        Ok((response, true))
    };
    let net = crate::aux_search::Network {
        resolver: &|_| Ok(vec!["93.184.216.34".parse().unwrap()]),
        transfer,
    };
    for (i, cfg) in [&selected, &changed].into_iter().enumerate() {
        assert_eq!(
            crate::aux_search::web_search(&format!("fixture query {i}"), cfg, &context, &net).unwrap_err(),
            "主检索服务状态码 401",
        );
    }
    assert_eq!(seen.into_inner(), [KEY, "fixture-search-literal"], "credential did not follow its endpoint binding");
}

#[test]
fn ticket219_core_interrupted_credential_write_keeps_previous_value() {
    let (root, context) = settings_fixture("interrupted credential write");
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    assert_eq!(put(&context, CLI, "fixture-pending-rotation").0, 503, "interrupted write was accepted");
    let reopened = RuntimeContext::from_root(&root.0).unwrap();
    assert_eq!(crate::settings::secret_value(&reopened, &crate::settings::CLI_MODEL_API_KEY, RELAY).unwrap().as_deref(), Some(CLI_KEY));
    assert_eq!(audit_rows(&root).len(), 1);
}

#[test]
fn ticket219_core_search_cache_reloads_on_credential_rotation() {
    use crate::aux_fetch::Head;
    let (root, context) = settings_fixture("search credential cache");
    let cfg = json!({"webSearch": {"provider": "deepseek", "baseURL": "https://search.example.test/v1"}});
    write_config(&root, |file| file["webSearch"] = cfg["webSearch"].clone());
    let observed = std::cell::RefCell::new(Vec::new());
    let transfer: &crate::aux_fetch::TransferFn<'_> = &|request, head, body| {
        let key = request.headers.iter().find(|(name, _)| name == "x-api-key").unwrap().1.clone();
        let response = Head { status: 200, location: None };
        let payload = json!({"content": [{"type": "web_search_tool_result", "content": [
            {"type": "web_search_result", "title": key, "url": "https://result.example.test"}
        ]}]}).to_string();
        observed.borrow_mut().push(key);
        let _ = head(&response);
        let _ = body(payload.as_bytes());
        Ok((response, true))
    };
    let net = crate::aux_search::Network {
        resolver: &|_| Ok(vec!["93.184.216.34".parse().unwrap()]),
        transfer,
    };
    let search = || crate::aux_search::web_search("unchanged fixture query", &cfg, &context, &net).unwrap();
    assert_eq!(put(&context, SEARCH, KEY).0, 200);
    assert_eq!(search()["results"][0]["title"], "[REDACTED]");
    assert_eq!(search()["results"][0]["title"], "[REDACTED]");
    assert_eq!(put(&context, SEARCH, "fixture-search-rotation").0, 200);
    assert_eq!(search()["results"][0]["title"], "[REDACTED]");
    assert_eq!(observed.into_inner(), [KEY, "fixture-search-rotation"], "rotation did not invalidate the cached answer");
}

#[test]
fn ticket443_harness_api_key_binding_fallback_expected_endpoint_and_redaction() {
    const HARNESS_KEY_SETTING: &str = "harness.apiKey";
    const BASE_URL_SETTING: &str = "harness.apiBaseUrl";
    let (root, context) = settings_fixture("ticket443 harness api key");
    let disk_key = "fixture-disk-harness-key";
    std::fs::write(root.0.join("config/harness-api-key"), disk_key).unwrap();
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"model": "fixture-model", "apiKey": "fixture-ignored-config-literal"});
    });
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();

    let view = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view["isSet"], false, "disk fallback must keep isSet=false: {view}");
    assert_eq!(view["source"], json!("file"), "disk fallback must report source=file: {view}");
    assert!(view.get("boundElsewhere").is_none_or(|v| v == false));

    let ep_b = "https://override-b.example.test/compatible-mode/v1";
    assert_eq!(put(&context, BASE_URL_SETTING, ep_b).0, 200);
    let view_b = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view_b["isSet"], false);
    assert_eq!(view_b["source"], Value::Null, "disk key must not apply to override endpoint: {view_b}");

    let err = crate::harness::ModelRoute::for_model(&context, "fixture-model").err().unwrap_or_default();
    assert_eq!(err, "Set harness.apiKey in Settings for the current model endpoint.");

    // expectedEndpoint validation: 400 on wrong settings / non-string; 409 on stale endpoint; 200 on matching canonical endpoint.
    let (bad_setting_status, _) = post(&context, "/api/settings/set", json!({
        "setting": QUIET_KEY, "value": 4500, "expectedEndpoint": ep_b
    }));
    assert_eq!(bad_setting_status, 400);
    let (bad_search_status, _) = post(&context, "/api/settings/set", json!({
        "setting": SEARCH, "value": KEY, "expectedEndpoint": ep_b
    }));
    assert_eq!(bad_search_status, 400);
    let (bad_type_status, _) = post(&context, "/api/settings/set", json!({
        "setting": HARNESS_KEY_SETTING, "value": "fixture-b-key", "expectedEndpoint": 42
    }));
    assert_eq!(bad_type_status, 400);
    let (stale_status, stale_reply) = post(&context, "/api/settings/set", json!({
        "setting": HARNESS_KEY_SETTING, "value": "fixture-b-key", "expectedEndpoint": default_ep
    }));
    assert_eq!(stale_status, 409, "{stale_reply}");

    let key_b = "fixture-harness-b-key";
    let (ok_status, ok_reply) = post(&context, "/api/settings/set", json!({
        "setting": HARNESS_KEY_SETTING,
        "value": key_b,
        "expectedEndpoint": format!("{ep_b}/"),
    }));
    assert_eq!(ok_status, 200, "{ok_reply}");
    assert_eq!(ok_reply["isSet"], true);

    let view_b_set = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view_b_set["isSet"], true);
    assert_eq!(view_b_set["source"], json!("settings"));

    // Move B -> C -> B: B's bound row is preserved.
    let ep_c = "https://override-c.example.test/v1";
    assert_eq!(put(&context, BASE_URL_SETTING, ep_c).0, 200);
    let view_c = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view_c["isSet"], false);
    assert_eq!(view_c["boundElsewhere"], true);
    assert_eq!(view_c["source"], Value::Null);
    assert!(!view_c.to_string().contains(ep_b));
    assert!(!view_c.to_string().contains(key_b));

    assert_eq!(put(&context, BASE_URL_SETTING, ep_b).0, 200);
    let view_b_again = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view_b_again["isSet"], true);
    assert_eq!(view_b_again["source"], json!("settings"));

    // Resetting back to D restores disk fallback while marking boundElsewhere=true.
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting": BASE_URL_SETTING})).0, 200);
    let view_d = setting_view(&context, HARNESS_KEY_SETTING);
    assert_eq!(view_d["isSet"], false);
    assert_eq!(view_d["boundElsewhere"], true);
    assert_eq!(view_d["source"], json!("file"));

    // reject_credential_text blocks credentials inside harness.apiBaseUrl.
    for leaked in [key_b, disk_key] {
        let (leak_status, leak_reply) = put(&context, BASE_URL_SETTING, &format!("https://api.example.com/{leaked}"));
        assert_eq!(leak_status, 400, "{leak_reply}");
        assert_eq!(leak_reply["error"], "setting text contains a credential");
    }

    // Confirm config.json /harness/apiKey literal is ignored and never mirrored.
    let loaded = context.load_config_document().unwrap();
    assert_eq!(loaded.pointer("/harness/apiKey"), Some(&json!("fixture-ignored-config-literal")));
    assert_eq!(put(&context, BASE_URL_SETTING, ep_c).0, 200);
    assert!(
        crate::harness::ModelRoute::for_model(&context, "fixture-model").is_err(),
        "/harness/apiKey in config.json must never authenticate"
    );

    // cliModel.apiKey also enforces expectedEndpoint 409 conflict guard and binding-aware view.
    let (cli_stale, _) = post(&context, "/api/settings/set", json!({
        "setting": CLI, "value": CLI_KEY, "expectedEndpoint": ep_b
    }));
    assert_eq!(cli_stale, 409);
    let (cli_ok, cli_reply) = post(&context, "/api/settings/set", json!({
        "setting": CLI, "value": CLI_KEY, "expectedEndpoint": format!("{ep_c}/")
    }));
    assert_eq!((cli_ok, &cli_reply["isSet"]), (200, &json!(true)));
    assert_eq!(setting_view(&context, CLI)["isSet"], true);
    assert_eq!(put(&context, BASE_URL_SETTING, ep_b).0, 200);
    let cli_moved = setting_view(&context, CLI);
    assert_eq!(cli_moved["isSet"], false);
    assert_eq!(cli_moved["boundElsewhere"], true);

    // Seed never imports a config.json /harness/apiBaseUrl containing a credential.
    let (seed_root, seed_ctx) = settings_fixture("ticket443 seed url credential");
    let secret_in_url = "fixture-secret-seed-url";
    assert_eq!(put(&seed_ctx, CLI, secret_in_url).0, 200);
    write_config(&seed_root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{secret_in_url}")});
    });
    assert_eq!(set_quiet(&seed_ctx, json!(4600)).0, 200);
    assert!(
        history_page(&seed_ctx, "?setting=harness.apiBaseUrl").0.is_empty(),
        "seed must skip harness.apiBaseUrl containing a credential"
    );

    // Redaction across settings::stored_secrets, harness::routes, and /api/settings/models covers DB harness.apiKey.
    assert_eq!(put(&context, HARNESS_KEY_SETTING, key_b).0, 200);
    let known_secrets = crate::settings::stored_secrets(&context).unwrap();
    assert!(known_secrets.contains(&key_b.to_string()));
    assert!(
        crate::harness::ModelRoute::for_model(&context, &format!("m-{key_b}")).is_err(),
        "for_model must refuse a model containing a credential"
    );
    let route = crate::harness::ModelRoute::for_model(&context, "fixture-model").unwrap();
    assert_eq!(route.redact(&format!("before {key_b} {disk_key} after")), "before [REDACTED] [REDACTED] after");
    write_config(&root, |cfg| {
        cfg["harness"]["routes"] = json!([{"id": "r1", "model": format!("m-{key_b}")}]);
    });
    let (m_status, models_reply) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(m_status, 200);
    assert!(!models_reply.to_string().contains(key_b), "models list leaked harness.apiKey: {models_reply}");
}

#[test]
fn ticket443_r2_credential_url_hidden_cli_source_and_blank_expected_endpoint() {
    const HARNESS_KEY_SETTING: &str = "harness.apiKey";
    let (root, context) = settings_fixture("ticket443 r2 regressions");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();

    // 1. cliModel.apiKey view exposes source: null when unset, "settings" when bound, null when bound elsewhere.
    let cli_unset = setting_view(&context, CLI);
    assert_eq!(cli_unset.get("source"), Some(&Value::Null), "unset cliModel.apiKey must include source=null: {cli_unset}");
    assert_eq!(put(&context, CLI, CLI_KEY).0, 200);
    let cli_set = setting_view(&context, CLI);
    assert_eq!(cli_set["isSet"], true);
    assert_eq!(cli_set.get("source"), Some(&json!("settings")), "bound cliModel.apiKey must report source=settings: {cli_set}");
    assert_eq!(put(&context, "harness.apiBaseUrl", "https://r2-other.example.test/v1").0, 200);
    let cli_moved = setting_view(&context, CLI);
    assert_eq!(cli_moved["isSet"], false);
    assert_eq!(cli_moved["boundElsewhere"], true);
    assert_eq!(cli_moved.get("source"), Some(&Value::Null), "moved cliModel.apiKey must report source=null: {cli_moved}");

    // 2. Blank string expectedEndpoint reaches transactional store check and returns 409 (not 400).
    for (setting, exp) in [(HARNESS_KEY_SETTING, ""), (CLI, "   ")] {
        let (status, reply) = post(
            &context,
            "/api/settings/set",
            json!({"setting": setting, "value": "fixture-r2-key-alpha", "expectedEndpoint": exp}),
        );
        assert_eq!(status, 409, "blank expectedEndpoint for {setting} must return 409: {reply}");
    }
    let (other_blank_status, _) = post(
        &context,
        "/api/settings/set",
        json!({"setting": SEARCH, "value": KEY, "expectedEndpoint": ""}),
    );
    assert_eq!(other_blank_status, 400);

    // 3. Credential-bearing config.json /harness/apiBaseUrl must never leak via resolver, GET /api/settings, or GET /api/settings/models.
    let (leak_root, leak_ctx) = settings_fixture("ticket443 r2 config credential url");
    let leak_default = leak_ctx.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let stored_val = "fixture-r2-alpha-val";
    assert_eq!(put(&leak_ctx, CLI, stored_val).0, 200);
    let disk_val = "fixture-r2-beta-val";
    std::fs::write(leak_root.0.join("config/harness-api-key"), disk_val).unwrap();

    for (label, leaked) in [("db-val", stored_val), ("disk-val", disk_val)] {
        write_config(&leak_root, |cfg| {
            cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{leaked}")});
        });
        let snap = crate::settings::resolve_model_endpoint_and_key(&leak_ctx).unwrap();
        assert_eq!(
            snap.effective_endpoint.as_deref(),
            Some(leak_default.as_str()),
            "{label}: resolver must ignore credential-bearing config.json url"
        );
        let view_entry = setting_view(&leak_ctx, "harness.apiBaseUrl");
        assert_eq!(
            view_entry["value"],
            Value::Null,
            "{label}: GET /api/settings must hide credential-bearing config.json url: {view_entry}"
        );
        let (m_status, models_reply) = admin_request(&leak_ctx, "GET", "/api/settings/models", None);
        assert_eq!(m_status, 200);
        assert_eq!(
            models_reply["effectiveModelEndpoint"],
            json!(leak_default),
            "{label}: GET /api/settings/models must ignore credential-bearing config.json url: {models_reply}"
        );
        assert_eq!(models_reply["effectiveModelEndpointSource"], json!("dependencies.json"));
    }
    let _ = (root, default_ep);
}

#[test]
fn ticket443_r3_stored_url_and_routes_credential_suppression() {
    // 1. Stored harness.apiBaseUrl containing a post-saved DB credential is suppressed across resolver,
    // GET /api/settings (value + stored), GET /api/settings/models, and GET /api/settings/history.
    let (_root, context) = settings_fixture("ticket443 r3 stored url suppression");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let post_tok = "fixture-r3-post-tok";
    let leaked_url = format!("https://api.example.com/{post_tok}");
    assert_eq!(put(&context, "harness.apiBaseUrl", &leaked_url).0, 200);
    assert_eq!(put(&context, CLI, post_tok).0, 200);

    let snap = crate::settings::resolve_model_endpoint_and_key(&context).unwrap();
    assert_eq!(
        snap.effective_endpoint.as_deref(),
        Some(default_ep.as_str()),
        "resolver must suppress stored harness.apiBaseUrl containing a post-saved credential"
    );
    let url_view = setting_view(&context, "harness.apiBaseUrl");
    assert_eq!(url_view["value"], Value::Null, "GET /api/settings value must hide leaked stored url: {url_view}");
    assert!(
        !url_view["stored"].to_string().contains(post_tok),
        "GET /api/settings stored must not expose leaked stored url: {url_view}"
    );
    let (m_status, models_reply) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(m_status, 200);
    assert_eq!(
        models_reply["effectiveModelEndpoint"],
        json!(default_ep),
        "GET /api/settings/models must suppress leaked stored url: {models_reply}"
    );
    assert_eq!(models_reply["effectiveModelEndpointSource"], json!("dependencies.json"));
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert!(!entries.is_empty());
    for entry in &entries {
        assert!(
            !entry.to_string().contains(post_tok),
            "history must mask leaked harness.apiBaseUrl entry: {entry}"
        );
    }
    assert_eq!(entries[0]["new"], json!("(hidden)"));

    // Replacing stored URL with a clean one leaves the leaked history entry masked and blocks undo back to it.
    let clean_url = "https://clean-r3.example.test/v1";
    assert_eq!(put(&context, "harness.apiBaseUrl", clean_url).0, 200);
    let (entries_after, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert_eq!(entries_after[0]["old"], json!("(hidden)"));
    assert_eq!(entries_after[0]["new"], json!(clean_url));
    let (undo_status, _) = post(&context, "/api/settings/undo", json!({"entry": entries_after[0]["entry"]}));
    assert_eq!(undo_status, 400);

    // 2. Stored harness.apiBaseUrl containing a post-added config/harness-api-key is also suppressed,
    // while harness.routes rejects stored DB secrets in set/seed (preserving existing disk-key masking).
    let (disk_root, disk_ctx) = settings_fixture("ticket443 r3 stored url disk credential");
    let disk_default = disk_ctx.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let disk_tok = "fixture-r3-later-disk-tok";
    assert_eq!(put(&disk_ctx, "harness.apiBaseUrl", &format!("https://api.example.com/{disk_tok}")).0, 200);
    std::fs::write(disk_root.0.join("config/harness-api-key"), disk_tok).unwrap();
    let disk_snap = crate::settings::resolve_model_endpoint_and_key(&disk_ctx).unwrap();
    assert_eq!(disk_snap.effective_endpoint.as_deref(), Some(disk_default.as_str()));
    assert_eq!(setting_view(&disk_ctx, "harness.apiBaseUrl")["value"], Value::Null);
    assert_eq!(history_page(&disk_ctx, "?setting=harness.apiBaseUrl").0[0]["new"], json!("(hidden)"));

    let (route_root, route_ctx) = settings_fixture("ticket443 r3 routes db credential");
    let db_val = "fixture-r3-route-db-alpha";
    assert_eq!(put(&route_ctx, "harness.apiKey", db_val).0, 200);
    for bad_routes in [
        json!([{"id": "r1", "model": db_val}]),
        json!([{"id": db_val, "model": "m1"}]),
    ] {
        let (st, reply) = post(
            &route_ctx,
            "/api/settings/set",
            json!({"setting": "harness.routes", "value": bad_routes}),
        );
        assert_eq!(st, 400, "harness.routes write containing DB key must fail: {reply}");
        assert_eq!(reply["error"], "setting text contains a credential");
    }
    let _ = route_root;
}

#[test]
fn ticket443_r4_raw_trailing_slash_secret_and_single_tx_view_consistency() {
    // 1. Raw URL ending with '/' matching a secret ending with '/' is blocked on set, seed, and stored-row read.
    let (_root, context) = settings_fixture("ticket443 r4 raw trailing slash set");
    let slash_tok = "fixture-r4-slash/";
    assert_eq!(put(&context, CLI, slash_tok).0, 200);
    let (st, reply) = put(&context, "harness.apiBaseUrl", &format!("https://api.example.com/{slash_tok}"));
    assert_eq!(st, 400, "raw URL ending in slash matching secret must fail: {reply}");
    assert_eq!(reply["error"], "setting text contains a credential");

    let (seed_root, seed_ctx) = settings_fixture("ticket443 r4 raw trailing slash seed");
    let seed_default = seed_ctx.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    std::fs::write(seed_root.0.join("config/harness-api-key"), slash_tok).unwrap();
    write_config(&seed_root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{slash_tok}")});
    });
    assert_eq!(set_quiet(&seed_ctx, json!(4600)).0, 200);
    assert!(history_page(&seed_ctx, "?setting=harness.apiBaseUrl").0.is_empty());
    assert_eq!(
        crate::settings::resolve_model_endpoint_and_key(&seed_ctx).unwrap().effective_endpoint.as_deref(),
        Some(seed_default.as_str())
    );

    let (stored_root, stored_ctx) = settings_fixture("ticket443 r4 stored trailing slash");
    let stored_default = stored_ctx.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    assert_eq!(set_quiet(&stored_ctx, json!(4700)).0, 200);
    StoreProbe::at(&stored_root.0).insert_value(
        "harness.apiBaseUrl",
        &json!(format!("https://api.example.com/{slash_tok}")).to_string(),
    );
    assert_eq!(put(&stored_ctx, CLI, slash_tok).0, 200);
    assert_eq!(
        crate::settings::resolve_model_endpoint_and_key(&stored_ctx).unwrap().effective_endpoint.as_deref(),
        Some(stored_default.as_str()),
        "resolver must suppress stored raw URL ending in trailing-slash secret"
    );
    let slash_view = setting_view(&stored_ctx, "harness.apiBaseUrl");
    assert_eq!(slash_view["value"], Value::Null);
    assert_eq!(slash_view["stored"], json!("(hidden)"));

    // 2. settings::view() redacts harness.routes and projects ownerQQ from the single snapshot directly.
    let (view_root, view_ctx) = settings_fixture("ticket443 r4 single tx view");
    let route_tok = "fixture-r4-route-tok";
    std::fs::write(view_root.0.join("config/harness-api-key"), route_tok).unwrap();
    write_config(&view_root, |cfg| {
        cfg["harness"] = json!({"routes": [{"id": "r1", "model": format!("m-{route_tok}")}]});
    });
    std::fs::create_dir_all(view_root.0.join("data")).unwrap();
    std::fs::write(view_root.0.join("data/mode.json"), json!({"mode": "closed-agent"}).to_string()).unwrap();
    std::fs::write(view_root.0.join("data/closed-agent-owner.json"), json!({"ownerQQ": 9102}).to_string()).unwrap();
    let direct_view = crate::settings::view(&view_ctx).unwrap();
    let entries = direct_view["settings"].as_array().unwrap();
    let routes_entry = entries.iter().find(|e| e["key"] == "harness.routes").unwrap();
    assert_eq!(routes_entry["value"][0]["model"], json!("m-[REDACTED]"), "settings::view() must mask routes directly: {routes_entry}");
    assert_eq!(routes_entry["redacted"], json!(true));
    let owner_entry = entries.iter().find(|e| e["key"] == "ownerQQ").unwrap();
    assert_eq!(owner_entry["overriddenBy"]["value"], json!(9102));
}

#[test]
fn ticket443_r5_scrub_tombstones_leaked_url_after_secret_reset_or_rotation() {
    let (root, context) = settings_fixture("ticket443 r5 permanent scrub");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let leaked_tok = "fixture-r5-tombstone-tok";
    let leaked_url = format!("https://api.example.com/{leaked_tok}");
    assert_eq!(put(&context, "harness.apiBaseUrl", &leaked_url).0, 200);
    assert_eq!(put(&context, CLI, leaked_tok).0, 200);

    // Rotate and then reset the secret: the leaked URL must remain tombstoned everywhere.
    let (rot_st, rot_reply) = put(&context, CLI, "fixture-r5-rotated-tok");
    assert_eq!(rot_st, 200, "rotate failed: {rot_reply}");
    let (rst_st, rst_reply) = post(&context, "/api/settings/reset", json!({"setting": CLI}));
    assert_eq!(rst_st, 200, "reset failed: {rst_reply}");

    let snap = crate::settings::resolve_model_endpoint_and_key(&context).unwrap();
    assert_eq!(
        snap.effective_endpoint.as_deref(),
        Some(default_ep.as_str()),
        "rotating/resetting secret must never revive tombstoned url"
    );
    let url_view = setting_view(&context, "harness.apiBaseUrl");
    assert_eq!(url_view["value"], Value::Null, "value must stay null after secret reset: {url_view}");
    assert_eq!(url_view["stored"], json!("(hidden)"), "stored must stay hidden tombstone: {url_view}");
    assert!(
        !stored_config(&root).to_string().contains(leaked_tok),
        "config.json mirror must omit scrubbed /harness/apiBaseUrl"
    );
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert!(!entries.is_empty());
    assert_eq!(entries[0]["new"], json!("(hidden)"));
    assert!(!json!(entries).to_string().contains(leaked_tok));
    let (undo_st, undo_reply) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(undo_st, 400, "undoing scrubbed leaked url must return 400: {undo_reply}");
}

#[test]
fn ticket443_r6_malformed_config_scrub_never_writes_hidden_and_consecutive_url_undo_is_400() {
    // 1. Malformed secret-bearing /harness/apiBaseUrl in config.json is scrubbed on write-back,
    // and write_back never persists "(hidden)" or secret-bearing harness.apiBaseUrl into config.json.
    let (root, context) = settings_fixture("ticket443 r6 malformed config scrub");
    let tok = "fixture-r6-malformed-tok";
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": format!("not-a-valid-url-{tok}")});
    });
    assert_eq!(put(&context, CLI, tok).0, 200);
    let after_secret = stored_config(&root);
    assert!(
        after_secret.pointer("/harness/apiBaseUrl").is_none(),
        "write-back must scrub malformed secret-bearing /harness/apiBaseUrl: {after_secret}"
    );
    assert!(
        !after_secret.to_string().contains(tok) && !after_secret.to_string().contains("(hidden)"),
        "config.json must never retain secret or (hidden): {after_secret}"
    );

    // Even if a stored row somehow holds "(hidden)" (or a leaked URL), write_back removes /harness/apiBaseUrl.
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": "https://stale.example.test/v1"});
    });
    StoreProbe::at(&root.0).insert_value("harness.apiBaseUrl", &json!("(hidden)").to_string());
    crate::settings::write_back(&context).unwrap();
    let after_wb = stored_config(&root);
    assert!(
        after_wb.pointer("/harness/apiBaseUrl").is_none() && !after_wb.to_string().contains("(hidden)"),
        "write_back must never write (hidden) into config.json: {after_wb}"
    );

    // 2. Two consecutive credential-bearing URLs followed by saving the secret: undo of latest entry returns 400, not 409.
    let (_seq_root, seq_ctx) = settings_fixture("ticket443 r6 consecutive leaked urls");
    let seq_tok = "fixture-r6-seq-tok";
    assert_eq!(put(&seq_ctx, "harness.apiBaseUrl", &format!("https://a.example.test/{seq_tok}")).0, 200);
    assert_eq!(put(&seq_ctx, "harness.apiBaseUrl", &format!("https://b.example.test/{seq_tok}")).0, 200);
    assert_eq!(put(&seq_ctx, CLI, seq_tok).0, 200);
    let (seq_entries, _) = history_page(&seq_ctx, "?setting=harness.apiBaseUrl");
    assert_eq!(seq_entries.len(), 2);
    let (undo_st, undo_reply) = post(&seq_ctx, "/api/settings/undo", json!({"entry": seq_entries[0]["entry"]}));
    assert_eq!(undo_st, 400, "undoing tombstoned consecutive leaked url must return 400, not 409: {undo_reply}");
    let (older_undo_st, older_undo_reply) = post(&seq_ctx, "/api/settings/undo", json!({"entry": seq_entries[1]["entry"]}));
    assert_eq!(
        older_undo_st, 400,
        "undoing older tombstoned leaked url (later > 0) must return 400, not 409: {older_undo_reply}"
    );
}

#[test]
fn ticket443_r7_mirror_into_and_write_back_scrub_trailing_slash_secret_raw_url() {
    let (root, context) = settings_fixture("ticket443 r7 raw trailing slash mirror scrub");
    let slash_key = "fixture-r7-slash/";
    let raw_leaked = format!("https://api.example.com/{slash_key}");
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    std::fs::write(root.0.join("config/harness-api-key"), slash_key).unwrap();
    StoreProbe::at(&root.0).insert_value("harness.apiBaseUrl", &json!(raw_leaked).to_string());
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": raw_leaked});
    });

    crate::settings::write_back(&context).unwrap();
    let after_wb = stored_config(&root);
    assert!(
        after_wb.pointer("/harness/apiBaseUrl").is_none() && !after_wb.to_string().contains(slash_key),
        "write_back must not mirror or retain raw trailing-slash secret url: {after_wb}"
    );

    let mut candidate_doc = stored_config(&root);
    candidate_doc["harness"] = json!({"apiBaseUrl": raw_leaked});
    crate::settings::mirror_into(&context, &mut candidate_doc).unwrap();
    assert!(
        candidate_doc.pointer("/harness/apiBaseUrl").is_none()
            && !candidate_doc.to_string().contains(slash_key),
        "mirror_into must scrub raw trailing-slash secret url from document: {candidate_doc}"
    );

    let mut clean_doc = stored_config(&root);
    crate::settings::mirror_into(&context, &mut clean_doc).unwrap();
    assert!(
        clean_doc.pointer("/harness/apiBaseUrl").is_none()
            && !clean_doc.to_string().contains(slash_key),
        "mirror_into must not re-insert suppressed raw trailing-slash url: {clean_doc}"
    );
}

#[test]
fn ticket443_r7_undo_older_tombstoned_url_returns_400() {
    let (_root, context) = settings_fixture("ticket443 r7 undo older tombstoned url");
    let tok = "fixture-r7-older-undo-tok";
    assert_eq!(put(&context, "harness.apiBaseUrl", &format!("https://a.example.test/{tok}")).0, 200);
    assert_eq!(put(&context, "harness.apiBaseUrl", "https://clean-r7.example.test/v1").0, 200);
    assert_eq!(put(&context, CLI, tok).0, 200);
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert_eq!(entries.len(), 2);
    // Older entry (`entries[1]`) has later > 0, yet contains a credential -> must return 400, not 409.
    let (st, reply) = post(&context, "/api/settings/undo", json!({"entry": entries[1]["entry"]}));
    assert_eq!(st, 400, "undoing older credential-bearing url (later > 0) must return 400: {reply}");
}

#[test]
fn ticket443_r8_seed_trailing_slash_secret_scrubs_row_and_migration() {
    let (root, context) = settings_fixture("ticket443 r8 seed trailing slash secret");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let slash_tok = "fixture-r8-slash/";
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{slash_tok}")});
    });
    // Trigger seed() before the trailing-slash secret is stored: canonicalized URL omits trailing '/'.
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    // Saving the trailing-slash secret must still match and tombstone the canonicalized row & migration entry.
    assert_eq!(put(&context, CLI, slash_tok).0, 200);
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting": CLI})).0, 200);

    let snap = crate::settings::resolve_model_endpoint_and_key(&context).unwrap();
    assert_eq!(
        snap.effective_endpoint.as_deref(),
        Some(default_ep.as_str()),
        "canonicalized seeded url matching trailing-slash secret must be tombstoned across secret reset"
    );
    let url_view = setting_view(&context, "harness.apiBaseUrl");
    assert_eq!(url_view["value"], Value::Null, "{url_view}");
    assert_eq!(url_view["stored"], json!("(hidden)"), "{url_view}");
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert!(!entries.is_empty());
    assert_eq!(entries[0]["source"], json!("migration"));
    assert_eq!(entries[0]["new"], json!("(hidden)"));
    assert!(!json!(entries).to_string().contains("fixture-r8-slash"));
}

#[test]
fn ticket443_r8_mirror_into_fails_closed_on_malformed_stored_endpoint() {
    let (root, context) = settings_fixture("ticket443 r8 mirror_into fail closed");
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    assert_eq!(put(&context, CLI, "fixture-r8-db-secret").0, 200);
    // Corrupt stored harness.apiBaseUrl row to non-string JSON so EndpointWriteGuard errors.
    StoreProbe::at(&root.0).insert_value("harness.apiBaseUrl", "12345");
    let mut doc = stored_config(&root);
    doc["notes"] = json!("benign-edit");
    assert!(
        crate::settings::mirror_into(&context, &mut doc).is_err(),
        "mirror_into must fail closed instead of returning Ok(false) on errored EndpointWriteGuard"
    );
    assert!(
        crate::config_tx::write_selected_config_text(&context, &doc.to_string()).is_err(),
        "config_tx must refuse to publish config.json when EndpointWriteGuard errors"
    );
}

#[test]
fn ticket443_r8_undo_scrubbed_migration_entry_returns_400() {
    let (root, context) = settings_fixture("ticket443 r8 undo scrubbed migration");
    let tok = "fixture-r8-mig-tok";
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{tok}")});
    });
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    assert_eq!(put(&context, CLI, tok).0, 200);
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["source"], json!("migration"));
    let (st, reply) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(
        st, 400,
        "undoing scrubbed migration harness.apiBaseUrl entry must return 400, not 409: {reply}"
    );
    assert_eq!(reply["error"], "setting text contains a credential");
}

#[test]
fn ticket443_r9_scrub_overwrites_audit_plaintext_and_blocks_repost_after_rotation() {
    let (root, context) = settings_fixture("ticket443 r9 scrub audit plaintext");
    let tok = "fixture-r9-leaked-tok";
    let leaked_url = format!("https://api.example.com/{tok}/path");
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": leaked_url})).0,
        200
    );
    assert_eq!(put(&context, CLI, tok).0, 200);
    // Rotate key so `tok` is no longer in active secrets.
    assert_eq!(put(&context, CLI, "fixture-r9-rotated-key").0, 200);
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting": "harness.apiBaseUrl"})).0, 200);

    // Inspect settings.db directly: neither settings nor settings_audit may retain `tok` in plaintext,
    // and the append-only update trigger must still be installed.
    let db = rusqlite::Connection::open(root.0.join("data/settings.db")).unwrap();
    let audit_rows: Vec<(Option<String>, Option<String>)> = {
        let mut stmt = db
            .prepare("SELECT old_value, new_value FROM settings_audit WHERE scope='global' AND key='harness.apiBaseUrl'")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(!audit_rows.is_empty());
    for (old_val, new_val) in &audit_rows {
        assert!(
            !old_val.as_deref().unwrap_or("").contains(tok)
                && !new_val.as_deref().unwrap_or("").contains(tok),
            "settings_audit must scrub plaintext leaked url on disk: {audit_rows:?}"
        );
    }
    assert!(
        db.execute("UPDATE settings_audit SET new_value='\"tamper\"' WHERE key='harness.apiBaseUrl'", []).is_err(),
        "settings_audit_no_update trigger must stay active after scrub"
    );
    drop(db);

    // Re-posting the tombstoned URL after key rotation must be rejected with 400 (via scrubbed_hashes).
    let (st, reply) = post(
        &context,
        "/api/settings/set",
        json!({"setting": "harness.apiBaseUrl", "value": leaked_url}),
    );
    assert_eq!(st, 400, "reposting rotated scrubbed endpoint must fail closed with 400: {reply}");
    assert_eq!(reply["error"], "setting text contains a credential");
}

#[test]
fn ticket443_r9_trailing_slash_secret_matches_embedded_path_segment() {
    let (root, context) = settings_fixture("ticket443 r9 trailing slash embedded");
    let slash_tok = "fixture-r9-mid//";
    assert_eq!(put(&context, CLI, slash_tok).0, 200);
    let url_with_suffix = "https://api.example.com/fixture-r9-mid/v1";
    let (st, reply) = post(
        &context,
        "/api/settings/set",
        json!({"setting": "harness.apiBaseUrl", "value": url_with_suffix}),
    );
    assert_eq!(
        st, 400,
        "endpoint containing stripped trailing-slash secret before suffix must return 400: {reply}"
    );
    assert_eq!(reply["error"], "setting text contains a credential");

    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": url_with_suffix});
    });
    assert_eq!(set_quiet(&context, json!(4600)).0, 200);
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "write_back must scrub embedded stripped trailing-slash secret url from config.json"
    );
}

#[test]
fn ticket443_r9_non_string_malformed_config_endpoint_scrubbed_by_write_back_and_mirror_into() {
    let (root, context) = settings_fixture("ticket443 r9 non-string config endpoint");
    let tok = "fixture-r9-arr-tok";
    assert_eq!(put(&context, CLI, tok).0, 200);
    write_config(&root, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": [tok]});
    });
    assert_eq!(set_quiet(&context, json!(4700)).0, 200);
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "write_back must scrub non-string credential-bearing /harness/apiBaseUrl"
    );

    let mut doc = stored_config(&root);
    doc["harness"] = json!({"apiBaseUrl": {"nested": tok}});
    assert!(crate::settings::mirror_into(&context, &mut doc).unwrap());
    assert!(
        doc.pointer("/harness/apiBaseUrl").is_none(),
        "mirror_into must scrub non-string object credential-bearing /harness/apiBaseUrl"
    );
}

#[test]
fn ticket443_r10_raw_sqlite_bytes_scrubbed_after_secret_save() {
    let (root, context) = settings_fixture("ticket443 r10 raw sqlite bytes");
    let tok = "fixture-r10-raw-bytes-secret-99";
    let leaked_host = "secret-r10-freed-page.example.test";
    let leaked_url = format!("https://{leaked_host}/{tok}/v1");
    assert_eq!(
        post(
            &context,
            "/api/settings/set",
            json!({"setting": "harness.apiBaseUrl", "value": leaked_url}),
        )
        .0,
        200
    );
    // Move active endpoint to a clean URL so `leaked_host` lives only in settings_audit before scrub.
    assert_eq!(
        post(
            &context,
            "/api/settings/set",
            json!({"setting": "harness.apiBaseUrl", "value": "https://clean-r10.example.test/v1"}),
        )
        .0,
        200
    );
    assert_eq!(put(&context, CLI, tok).0, 200);
    let raw_db = std::fs::read(root.0.join("data/settings.db")).unwrap();
    assert!(
        !raw_db
            .windows(leaked_host.len())
            .any(|w| w == leaked_host.as_bytes()),
        "raw settings.db bytes must contain zero freed-page or WAL-remnant leaked url bytes"
    );
}

#[test]
fn ticket443_r10_legacy_trailing_slash_scrub_records_canonical_hash() {
    let (root, context) = settings_fixture("ticket443 r10 legacy trailing slash");
    let tok = "fixture-r10-legacy-slash";
    let raw_with_slash = format!("https://api.example.com/{tok}/");
    let canonical = format!("https://api.example.com/{tok}");
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    StoreProbe::at(&root.0).insert_value("harness.apiBaseUrl", &json!(raw_with_slash).to_string());
    assert_eq!(put(&context, CLI, tok).0, 200);
    // Rotate key so `tok` is no longer in active secrets.
    assert_eq!(put(&context, CLI, "fixture-r10-rotated-key").0, 200);
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting": "harness.apiBaseUrl"})).0, 200);

    let (st, reply) = post(
        &context,
        "/api/settings/set",
        json!({"setting": "harness.apiBaseUrl", "value": canonical}),
    );
    assert_eq!(
        st, 400,
        "reposting canonicalized form of scrubbed legacy trailing-slash url must return 400: {reply}"
    );
    assert_eq!(reply["error"], "setting text contains a credential");
}

#[test]
fn ticket443_r10_json_escape_bypass_and_undo_scrubs_config_when_write_back_blocked() {
    let (root, context) = settings_fixture("ticket443 r10 json escape and undo");
    let esc_tok = "fixture-r10-esc\"tok";
    assert_eq!(put(&context, CLI, esc_tok).0, 200);
    // (a) JSON escape inside non-string object/array must still be scrubbed.
    let mut doc = stored_config(&root);
    doc["harness"] = json!({"apiBaseUrl": {"nested": [esc_tok]}});
    assert!(crate::settings::mirror_into(&context, &mut doc).unwrap());
    assert!(
        doc.pointer("/harness/apiBaseUrl").is_none(),
        "mirror_into must scrub non-string object/array containing escaped secret"
    );

    // (b) When write_back_values fails (blocked parent), undo() restoring None on another setting
    // still scrubs leaked /harness/apiBaseUrl from config.json via remove_from_config.
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.maxSteps", "value": 12})).0,
        200
    );
    let (entries, _) = history_page(&context, "?setting=harness.maxSteps");
    let max_steps_entry = entries[0]["entry"].clone();
    write_config(&root, |cfg| {
        cfg["socialV2"] = json!({"reply": "not-an-object"});
        cfg["harness"] = json!({"apiBaseUrl": format!("https://api.example.com/{esc_tok}")});
    });
    let (st, reply) = post(&context, "/api/settings/undo", json!({"entry": max_steps_entry}));
    assert_eq!(st, 200, "undo must succeed with mirrorError warning: {reply}");
    assert!(reply["mirrorError"].as_str().is_some());
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "remove_from_config inside undo must scrub leaked /harness/apiBaseUrl even when post-commit write_back is blocked"
    );
}

#[test]
fn ticket443_r10_models_api_redacts_price_label() {
    let (root, context) = settings_fixture("ticket443 r10 models price label");
    let tok = "fixture-r10-price-tok";
    std::fs::write(root.0.join("config/harness-api-key"), tok).unwrap();
    let route_model = format!("custom-{tok}-model");
    write_config(&root, |cfg| {
        cfg["pricing"] = json!({
            "models": {
                route_model.clone(): {}
            }
        });
    });
    let (st, reply) = post(
        &context,
        "/api/settings/set",
        json!({
            "setting": "harness.routes",
            "value": [{"id": "primary", "model": route_model}]
        }),
    );
    assert_eq!(st, 200, "{reply}");
    let (m_st, models_reply) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(m_st, 200, "{models_reply}");
    let route = &models_reply["routes"][0];
    assert!(
        !route["model"].as_str().unwrap_or("").contains(tok),
        "route.model must be redacted: {route}"
    );
    assert!(
        !route["price"]["label"].as_str().unwrap_or("").contains(tok),
        "route.price.label must be redacted: {route}"
    );
}

#[test]
fn ticket443_r11_secret_save_scrubs_before_resolving_endpoint_and_rejects_leaked_expected() {
    // (a) Leaked stored harness.apiBaseUrl in settings.db is scrubbed BEFORE binding harness.apiKey.
    let (root, context) = settings_fixture("ticket443 r11 secret save scrub before resolve");
    let default_ep = context.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let tok = "fixture-r11-prebind-tok";
    let leaked_host = "leaked-r11-db.example.test";
    let leaked_url = format!("https://{leaked_host}/{tok}");
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": leaked_url})).0,
        200
    );
    assert_eq!(put(&context, "harness.apiKey", tok).0, 200);

    let snap = crate::settings::resolve_model_endpoint_and_key(&context).unwrap();
    assert_eq!(snap.effective_endpoint.as_deref(), Some(default_ep.as_str()));
    assert_eq!(snap.harness_db_key.as_deref(), Some(tok));
    let raw_key_row = StoreProbe::at(&root.0).value("harness.apiKey").unwrap();
    assert!(
        !raw_key_row.contains(leaked_host),
        "stored harness.apiKey must bind to baseline D, not leaked URL: {raw_key_row}"
    );
    let raw_db = std::fs::read(root.0.join("data/settings.db")).unwrap();
    assert!(
        !raw_db.windows(leaked_host.len()).any(|w| w == leaked_host.as_bytes()),
        "raw settings.db must contain zero bytes of leaked DB URL: {leaked_host}"
    );

    // (b) Unseeded config.json /harness/apiBaseUrl containing tok is suppressed BEFORE resolution,
    // rejects expectedEndpoint=leaked_url with 409, binds to D without expectedEndpoint, and blocks repost.
    let (root2, context2) = settings_fixture("ticket443 r11 unseeded leaked config before bind");
    let default_ep2 = context2.dependency_url("model").unwrap().trim_end_matches('/').to_string();
    let tok2 = "fixture-r11-cfgbind-tok";
    let cfg_host = "leaked-r11-cfg.example.test";
    let cfg_leaked_url = format!("https://{cfg_host}/{tok2}");
    write_config(&root2, |cfg| {
        cfg["harness"] = json!({"apiBaseUrl": cfg_leaked_url});
    });
    let (st_conflict, conflict_reply) = post(
        &context2,
        "/api/settings/set",
        json!({"setting": "harness.apiKey", "value": tok2, "expectedEndpoint": cfg_leaked_url}),
    );
    assert_eq!(
        st_conflict, 409,
        "saving secret with leaked config endpoint as expectedEndpoint must return 409: {conflict_reply}"
    );
    assert_eq!(put(&context2, "harness.apiKey", tok2).0, 200);
    let snap2 = crate::settings::resolve_model_endpoint_and_key(&context2).unwrap();
    assert_eq!(snap2.effective_endpoint.as_deref(), Some(default_ep2.as_str()));
    assert_eq!(snap2.harness_db_key.as_deref(), Some(tok2));
    assert!(stored_config(&root2).pointer("/harness/apiBaseUrl").is_none());
    let (st_repost, _) = post(
        &context2,
        "/api/settings/set",
        json!({"setting": "harness.apiBaseUrl", "value": cfg_leaked_url}),
    );
    assert_eq!(st_repost, 400);
}

#[test]
fn ticket443_r11_wal_checkpoint_truncate_and_pinned_ro_reader() {
    let (root, context) = settings_fixture("ticket443 r11 wal checkpoint truncate");
    let tok = "fixture-r11-wal-tok";
    let wal_host = "wal-r11-pinned.example.test";
    let leaked_url = format!("https://{wal_host}/{tok}");
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": leaked_url})).0,
        200
    );
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": "https://clean-r11-wal.example.test/v1"})).0,
        200
    );

    let db_path = root.0.join("data/settings.db");
    let ro = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    ro.execute_batch("BEGIN; SELECT count(*) FROM settings;").unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(ro);
    });
    let res = put(&context, CLI, tok);
    releaser.join().unwrap();
    assert_eq!(res.0, 200);

    let wal_path = root.0.join("data/settings.db-wal");
    if wal_path.exists() {
        let wal_bytes = std::fs::read(&wal_path).unwrap();
        assert!(
            !wal_bytes.windows(wal_host.len()).any(|w| w == wal_host.as_bytes()),
            "settings.db-wal must be truncated with zero leaked url bytes"
        );
    }
    let db_bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !db_bytes.windows(wal_host.len()).any(|w| w == wal_host.as_bytes()),
        "settings.db must have zero leaked url bytes after VACUUM + checkpoint"
    );

    // Also unit-test checkpoint_wal_truncate directly with a short deadline against a pinned RO connection.
    let ro_pin = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    ro_pin.execute_batch("BEGIN; SELECT count(*) FROM settings;").unwrap();
    assert_eq!(set_quiet(&context, json!(4650)).0, 200);
    let rw_conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(25);
    assert!(
        crate::settings::checkpoint_wal_truncate(&rw_conn, deadline).is_err(),
        "checkpoint_wal_truncate must return Err when WAL is pinned past deadline"
    );
    drop(rw_conn);
    drop(ro_pin);
}

#[test]
fn ticket443_r11_malformed_json_audit_scrubbed_in_history_undo_and_db() {
    let (root, context) = settings_fixture("ticket443 r11 malformed json audit");
    let tok = "sk-r11-malformed-tok";
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    let malformed = json!({"nested": [format!("https://x/{tok}")]}).to_string();
    StoreProbe::at(&root.0).insert_audit(&[crate::settings::AuditRow {
        at_ms: 1000,
        actor: "fixture",
        source: "migration",
        scope: "global",
        key: "harness.apiBaseUrl",
        old: None,
        new: Some(malformed.clone()),
        request: "r11-malformed".to_string(),
    }]);
    assert_eq!(put(&context, CLI, tok).0, 200);

    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["new"], json!("(hidden)"), "{entries:?}");
    let (st, reply) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(st, 400, "undoing malformed scrubbed audit entry must return 400: {reply}");

    let db = rusqlite::Connection::open(root.0.join("data/settings.db")).unwrap();
    let raw_new: Option<String> = db
        .query_row(
            "SELECT new_value FROM settings_audit WHERE scope='global' AND key='harness.apiBaseUrl'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw_new.as_deref(), Some("\"(hidden)\""));
}

#[test]
fn ticket443_r11_models_api_redacts_price_currency_and_symbol() {
    let (root, context) = settings_fixture("ticket443 r11 models currency symbol");
    let tok = "fixture-r11-price-tok";
    std::fs::write(root.0.join("config/harness-api-key"), tok).unwrap();
    write_config(&root, |cfg| {
        cfg["pricing"] = json!({
            "currency": format!("USD-{tok}"),
            "symbol": format!("SYM-{tok}"),
            "models": { "m1": {} }
        });
    });
    let (st, reply) = post(
        &context,
        "/api/settings/set",
        json!({
            "setting": "harness.routes",
            "value": [{"id": "primary", "model": "m1"}]
        }),
    );
    assert_eq!(st, 200, "{reply}");
    let (m_st, models_reply) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(m_st, 200, "{models_reply}");
    let price = &models_reply["routes"][0]["price"];
    assert!(
        !price["currency"].as_str().unwrap_or("").contains(tok),
        "price.currency must be redacted: {price}"
    );
    assert!(
        !price["symbol"].as_str().unwrap_or("").contains(tok),
        "price.symbol must be redacted: {price}"
    );
}

#[test]
fn ticket443_r12_rotating_or_resetting_secret_scrubs_outgoing_leaked_endpoint() {
    // Part A: rotating a stored secret scrubs audit and config endpoint leaks carrying the outgoing secret.
    let (root, context) = settings_fixture("ticket443 r12 outgoing secret rotate");
    let old_tok = "sk-old-r12-tok";
    let new_tok = "sk-new-r12-tok";
    let leaked_old_url = format!("https://rotate-r12.example.test/{old_tok}/v1");
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    assert_eq!(put(&context, CLI, old_tok).0, 200);
    write_config(&root, |cfg| {
        cfg["harness"]["apiBaseUrl"] = json!(leaked_old_url);
    });
    StoreProbe::at(&root.0).insert_audit(&[crate::settings::AuditRow {
        at_ms: 1000,
        actor: "fixture",
        source: "console",
        scope: "global",
        key: "harness.apiBaseUrl",
        old: None,
        new: Some(json!(leaked_old_url).to_string()),
        request: "r12-rotate-audit".to_string(),
    }]);

    assert_eq!(put(&context, CLI, new_tok).0, 200);
    let (entries, _) = history_page(&context, "?setting=harness.apiBaseUrl");
    assert!(
        !entries.iter().any(|e| e.to_string().contains(old_tok)),
        "rotation must scrub outgoing secret from audit history: {entries:?}"
    );
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "rotation must scrub leaked config/harness.apiBaseUrl carrying outgoing secret"
    );

    // Part B: resetting a stored secret scrubs audit and config endpoint leaks carrying the reset secret.
    let (root2, context2) = settings_fixture("ticket443 r12 outgoing secret reset");
    let reset_tok = "sk-reset-r12-tok";
    let leaked_reset_url = format!("https://reset-r12.example.test/{reset_tok}/v1");
    assert_eq!(set_quiet(&context2, json!(4500)).0, 200);
    assert_eq!(put(&context2, CLI, reset_tok).0, 200);
    write_config(&root2, |cfg| {
        cfg["harness"]["apiBaseUrl"] = json!(leaked_reset_url);
    });
    StoreProbe::at(&root2.0).insert_audit(&[crate::settings::AuditRow {
        at_ms: 1000,
        actor: "fixture",
        source: "console",
        scope: "global",
        key: "harness.apiBaseUrl",
        old: None,
        new: Some(json!(leaked_reset_url).to_string()),
        request: "r12-reset-audit".to_string(),
    }]);

    let (st, reply) = post(&context2, "/api/settings/reset", json!({"setting": CLI}));
    assert_eq!(st, 200, "{reply}");
    let (entries2, _) = history_page(&context2, "?setting=harness.apiBaseUrl");
    assert!(
        !entries2.iter().any(|e| e.to_string().contains(reset_tok)),
        "reset must scrub outgoing secret from audit history: {entries2:?}"
    );
    assert!(
        stored_config(&root2).pointer("/harness/apiBaseUrl").is_none(),
        "reset must scrub leaked config/harness.apiBaseUrl carrying reset secret"
    );
}

#[test]
fn ticket443_r12_pinned_ro_503_mirrors_config_and_retry_completes_physical_cleanup() {
    let (root, context) = settings_fixture("ticket443 r12 pinned ro 503");
    let tok = "fixture-r12-pin-tok";
    let wal_host = "ro-503-r12.example.test";
    let leaked_url = format!("https://{wal_host}/{tok}");
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": leaked_url})).0,
        200
    );
    assert_eq!(stored_config(&root).pointer("/harness/apiBaseUrl"), Some(&json!(leaked_url)));

    let db_path = root.0.join("data/settings.db");
    let ro = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let ro_tx = ro.unchecked_transaction().unwrap();
    let _: i64 = ro_tx.query_row("SELECT count(*) FROM settings", [], |r| r.get(0)).unwrap();

    // While RO transaction pins WAL, put must return 503 AFTER mirroring scrubbed config.json.
    let (st1, _) = put(&context, CLI, tok);
    assert_eq!(st1, 503, "pinned RO snapshot must cause 503 when physical cleanup cannot truncate WAL");
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "503 from failed physical cleanup must still mirror scrubbed config.json"
    );

    // Release RO snapshot and retry exact same save: durable pending marker completes VACUUM + TRUNCATE.
    drop(ro_tx);
    drop(ro);
    let (st2, _) = put(&context, CLI, tok);
    assert_eq!(st2, 200);

    let wal_path = root.0.join("data/settings.db-wal");
    if wal_path.exists() {
        let wal_bytes = std::fs::read(&wal_path).unwrap();
        assert!(
            !wal_bytes.windows(wal_host.len()).any(|w| w == wal_host.as_bytes()),
            "settings.db-wal must have zero leaked url bytes after retry cleanup"
        );
    }
    let db_bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !db_bytes.windows(wal_host.len()).any(|w| w == wal_host.as_bytes()),
        "settings.db must have zero leaked url bytes after retry cleanup"
    );
}

#[test]
fn ticket443_r12_remove_from_config_always_scrubs_leaked_endpoint() {
    let (root, context) = settings_fixture("ticket443 r12 remove_from_config");
    let tok = "sk-r12-rm-tok";
    assert_eq!(
        post(&context, "/api/settings/set", json!({"setting": "harness.apiBaseUrl", "value": "https://clean-r12-rm.example.test/v1"})).0,
        200
    );
    assert_eq!(put(&context, CLI, tok).0, 200);
    assert_eq!(set_quiet(&context, json!(4500)).0, 200);
    let (entries, _) = history_page(&context, "?setting=reply.wakeQuietMs");
    let entry = entries[0]["entry"].as_i64().unwrap();

    // Inject a leaked /harness/apiBaseUrl into config.json while store holds a clean endpoint.
    let leaked_cfg = format!("https://leaked-r12-rm.example.test/{tok}/v1");
    write_config(&root, |cfg| {
        cfg["harness"]["apiBaseUrl"] = json!(leaked_cfg);
    });

    // Abort right after remove_from_config runs (before commit/mirror_after_commit) to verify remove_from_config itself scrubbed config.json.
    crate::settings::PANIC_BEFORE_COMMIT.with(|p| p.set(true));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::settings::undo(&context, entry, "fixture-admin", &|_| Ok(()))
    }));
    crate::settings::PANIC_BEFORE_COMMIT.with(|p| p.set(false));
    assert!(res.is_err(), "expected PANIC_BEFORE_COMMIT after remove_from_config");
    assert!(
        stored_config(&root).pointer("/harness/apiBaseUrl").is_none(),
        "remove_from_config must scrub leaked config/harness.apiBaseUrl even when store holds a clean endpoint"
    );
}
