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
    assert_eq!(registered, [SEARCH, CLI], "only endpoint API keys are secret");
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
            "主搜索接口 HTTP 401",
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
