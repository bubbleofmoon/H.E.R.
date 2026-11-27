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
    let route = crate::harness::ModelRoute::for_model(&context, &format!("m-{key_b}")).unwrap();
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
