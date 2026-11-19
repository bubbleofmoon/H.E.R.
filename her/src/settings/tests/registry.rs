// The registry: its entries, each checked through its kind, and the settings page that lists them.
use super::*;

#[test]
fn settings_registry_is_complete_and_agrees_with_its_reader() {
    let registry: Vec<_> = crate::settings::registry().collect();
    assert!(!registry.is_empty(), "the registry has settings");
    let mut keys: Vec<&str> = registry.iter().map(|setting| setting.key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), registry.len(), "registry keys are unique");
    let mut pointers: Vec<&str> = registry.iter().map(|setting| setting.pointer).collect();
    pointers.sort();
    pointers.dedup();
    assert_eq!(pointers.len(), registry.len(), "registry pointers are unique");
    for setting in registry {
        assert!(!setting.label.is_empty() && !setting.hint.is_empty() && !setting.group.is_empty(), "{}", setting.key);
        assert!(matches!(setting.applies, "live" | "next wake" | "restart" | "not read yet"), "{}", setting.key);
        assert!(setting.pointer.starts_with('/') && !setting.pointer.contains('~'), "{}", setting.pointer);
        assert!(setting.pointer.split('/').skip(1).all(|name| !name.is_empty()), "{}", setting.pointer);
        assert!(!setting.secret || !setting.chat, "a credential cannot be chat-visible");
        // Each kind checks its own bounds, default and reader (`SettingKind::check_entry`).
        setting.kind.check_entry(setting);
    }
    // The registry owns the default and bounds the reader applies to the file (`wake_cfg_of`).
    let quiet = crate::settings::find(QUIET_KEY).expect("reply.wakeQuietMs is registered");
    let kind = quiet.kind.view();
    assert_eq!(kind["type"], "integer", "reply.wakeQuietMs is an integer setting");
    let (min, max) = (kind["min"].as_i64().unwrap(), kind["max"].as_i64().unwrap());
    let default = quiet.default_value().as_i64().unwrap();
    for ms in [min - 1, min, default, max, max + 1] {
        let config = json!({"socialV2": {"reply": {"wakeQuietMs": ms}}});
        let expected = quiet.normalize(&json!(ms)).and_then(|value| value.as_i64()).unwrap_or(default);
        assert_eq!(crate::wake_cfg_of(&config).reply_wake_quiet_ms, Some(expected), "{ms}");
    }
    assert_eq!(crate::wake_cfg_of(&json!({})).reply_wake_quiet_ms, Some(default));
}

#[test]
fn no_registry_setting_is_cached_on_the_config_stamp() {
    // `RuntimeContext::config_stamp` sees only the stored file. Values cached on it alone: `reserved.rs` (`social`),
    // `console_access.rs` (`allowAllWhenEmpty`; its allow, deny and owner view is keyed on the store's mark too, #154)
    // and `history_log.rs` (`history`).
    let registry: Vec<_> = crate::settings::registry().collect();
    assert!(!registry.is_empty(), "the registry has settings");
    for setting in registry {
        for cached in ["/social", "/allowAllWhenEmpty", "/history"] {
            assert!(
                setting.pointer != cached && !setting.pointer.starts_with(&format!("{cached}/")),
                "{} is cached on the config stamp ({cached}); key that cache on the store too",
                setting.key
            );
        }
    }
}

#[test]
fn settings_page_lists_the_registry_and_sets_through_the_route() {
    let (root, context) = settings_fixture("settings page");
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let (status, content_type, page) =
        crate::console_settings::route(&context, "GET", "/settings", &HashMap::new(), "", false, &granted);
    assert_eq!(status, 200, "{page}");
    assert_eq!(content_type, "text/html; charset=utf-8");
    let (status, _) = admin_request(&context, "GET", "/settings", None);
    assert_eq!(status, 200, "the page is served behind the console front");
    let (status, body) = agent_request(&context, "GET", "/settings", None, "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    // The page renders what the list route returns and sends what the set route reads.
    let view = quiet_view(&context);
    for field in ["key", "label", "hint", "value", "default", "applies"] {
        assert!(page.contains(&format!("s.{field}")), "the page shows {field}");
        assert!(!view[field].is_null(), "the list route returns {field}");
    }
    assert!(page.contains("s.kind.min") && page.contains("s.kind.max"), "the input takes the bounds");
    assert_eq!(view["kind"]["min"], 1);
    assert!(page.contains("const type=s.kind.type") && page.contains("type==='integer'"), "the page picks the input by the kind");
    assert!(page.contains("input.type='checkbox'") && page.contains("type==='id list'"), "a switch and an id list have inputs");
    assert!(!page.contains("maxLength"), "a model id input sets no UTF-16 length: the set route counts characters");
    assert!(page.contains("input.value=s.value==null?'':s.value"), "a model id that is not configured shows empty");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    let model = body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == "harness.model").unwrap();
    assert_eq!(model["kind"], json!({"type": "model id", "maxChars": 200}));
    assert!(page.contains("api('/api/settings')"), "the page lists through the route");
    assert!(
        page.contains("send('/api/settings/set',{setting:s.key,value:value()})")
            && page.contains("type==='integer'?Number(input.value)"),
        "the page sets through the route: a number for an integer setting, the text for a model id"
    );
    assert!(page.contains("send('/api/settings/reset',{setting:s.key})"), "the page resets through the route");
    assert!(
        page.contains("const body={entry:e.entry}") && page.contains("api('/api/settings/undo',body)"),
        "the page undoes through the route"
    );
    // #154: a security setting is changed only after the page asked, and the request says so.
    assert!(page.contains("if(s.confirm){if(!ask(s.key))return;body.confirm=true}"), "a set or reset asks first");
    assert!(page.contains("if(security.has(e.setting)){if(!ask(e.setting))return;body.confirm=true}"), "an undo asks first");
    assert!(page.contains("confirm('Change the security setting '"), "the page asks with window.confirm");
    assert!(page.contains("s.overriddenBy"), "the page shows the owner the admin agent mode puts in force");
    assert!(
        page.contains("'/api/settings/history'") && page.contains("'?before='+next"),
        "the page lists the history one page at a time"
    );
    for field in ["entry", "atMs", "setting", "old", "new", "source", "actor"] {
        assert!(page.contains(&format!("e.{field}")), "the history shows {field}");
    }
    assert!(page.contains("'x-console-token'"), "the page sends the console credential");
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
    let request = json!({"setting": QUIET_KEY, "value": 4500});
    let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&request));
    assert_eq!(status, 200, "{body}");
    assert_eq!(quiet_view(&context)["value"], 4500, "the page's next list shows the new value");
}
