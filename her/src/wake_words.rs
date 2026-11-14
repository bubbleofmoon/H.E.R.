//! Persona wake words (GLOSSARY "Persona"). Stub until Run B2.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

/// Entries kept in a conversation's `wakeWordLog`.
pub(crate) const LOG_MAX: usize = 20;

pub(crate) fn effective(_context: &RuntimeContext, _key: &str, keywords: &[String], _dropped: &[String]) -> Vec<String> {
    keywords.to_vec()
}

pub(crate) fn drop_names_input(_input: Option<&Value>, _current: Option<&Value>) -> Option<Vec<Value>> {
    None
}

pub(crate) fn words_of(_wake_config: &Value) -> Value {
    json!({"keywords": [], "dropNames": []})
}

pub(crate) fn record(_conversation: &mut Value, _before: &Value, _after: &Value, _by: &str, _at: i64) -> Option<u64> {
    None
}

pub(crate) fn undo(_state: &mut Value, _key: &str, _id: u64, _at: i64) -> Result<Value, (u16, String)> {
    Err((501, "not implemented".to_string()))
}

pub(crate) fn view(_context: &RuntimeContext, _conversation: &str) -> Result<Value, (u16, String)> {
    Err((501, "not implemented".to_string()))
}

pub(crate) fn briefing_line(_context: &RuntimeContext, _key: &str, _conversation: &Value) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    fn fixture(label: &str, library: &Value) -> (std::path::PathBuf, crate::runtime::RuntimeContext) {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../backups/ticket 21 wake word fixtures")
            .join(format!("{label}-{}-{}", std::process::id(), crate::parity_clock::now_ms()));
        let root = crate::install_root::development_root(root);
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","ownerQQ":1,"socialV2":{"wake":{}}}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        std::fs::write(root.join("data/personas.json"), library.to_string()).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    /// p1 (default) 小鲸鱼 with names 小鲸鱼/鲸鱼; p2 会计 without a names list, assigned to group:2.
    fn library() -> Value {
        json!({"format": 1, "nextId": 3, "default": "p1", "overrides": {"group:2": "p2"}, "importedRolesAt": null, "personas": {
            "p1": {"id": "p1", "activeVersion": 1, "versions": [{"version": 1, "approved": true, "name": "小鲸鱼", "core": "核心。", "names": ["小鲸鱼", "鲸鱼"]}]},
            "p2": {"id": "p2", "activeVersion": 1, "versions": [{"version": 1, "approved": true, "name": "会计", "core": "核心。"}]}}})
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn persona_names_wake_her_and_switch_with_the_persona() {
        let (root, context) = fixture("names", &library());
        assert_eq!(super::effective(&context, "group:1", &[], &[]), words(&["小鲸鱼", "鲸鱼"]));
        assert_eq!(super::effective(&context, "group:2", &[], &[]), words(&["会计"]), "the override's names; its name when it lists none");
        assert_eq!(super::effective(&context, "group:1", &words(&["海", "小鲸鱼"]), &words(&["鲸鱼"])), words(&["小鲸鱼", "海"]), "dropped names go, her keywords follow, no duplicates");
        let mut switched = library();
        switched["overrides"] = json!({"group:2": "p1"});
        std::fs::write(root.join("data/personas.json"), switched.to_string()).unwrap();
        assert_eq!(super::effective(&context, "group:2", &words(&["海"]), &[]), words(&["小鲸鱼", "鲸鱼", "海"]), "a switch switches the names");
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        assert_eq!(super::effective(&context, "group:1", &words(&["海"]), &[]), words(&["海"]), "closed-agent adds no names");
        std::fs::remove_file(root.join("data/mode.json")).unwrap();
        std::fs::write(root.join("data/personas.json"), r#"{"format":99}"#).unwrap();
        assert_eq!(super::effective(&context, "group:1", &words(&["海"]), &[]), words(&["海"]), "a damaged library adds no names");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_change_of_her_words_is_logged_once_and_the_newest_twenty_stay() {
        let mut conversation = json!({});
        let before = json!({"triggers": {"keywords": ["海"]}});
        assert_eq!(super::record(&mut conversation, &before, &before.clone(), "her", 1), None, "no change, no entry");
        assert_eq!(super::record(&mut conversation, &before, &json!({"triggers": {"keywords": [" 海 "]}}), "her", 1), None, "the same words, no entry");
        assert!(conversation.get("wakeWordLog").is_none());
        let after = json!({"triggers": {"keywords": ["海"], "dropNames": ["小鲸鱼"]}});
        assert_eq!(super::record(&mut conversation, &before, &after, "her", 2), Some(1));
        assert_eq!(conversation["wakeWordLog"][0], json!({"id": 1, "at": 2, "by": "her",
            "added": {"keywords": [], "dropNames": ["小鲸鱼"]}, "removed": {"keywords": [], "dropNames": []}}));
        for n in 0..(super::LOG_MAX as i64 - 1) {
            super::record(&mut conversation, &json!({}), &json!({"triggers": {"keywords": [format!("词{n}")]}}), "console", 10 + n);
        }
        let ids = |c: &Value| c["wakeWordLog"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect::<Vec<_>>();
        assert_eq!((ids(&conversation).len(), ids(&conversation)[0]), (20, 1), "exactly twenty entries keep the first");
        super::record(&mut conversation, &json!({}), &json!({"triggers": {"keywords": ["再一个"]}}), "her", 100);
        assert_eq!((ids(&conversation).len(), ids(&conversation)[0], ids(&conversation)[19]), (20, 2, 21), "the twenty-first drops the oldest");
        assert_eq!(super::words_of(&json!({"triggers": {"keywords": [" 海 ", "", 3]}})), json!({"keywords": ["海"], "dropNames": []}));
    }

    #[test]
    fn undo_reverts_an_entry_only_while_its_change_is_still_in_place() {
        let mut state = json!({"conversations": {"group:1": {"wakeConfig": {"mode": "diving", "infinite": true,
            "triggers": {"atMention": true, "keywords": ["海"], "dropNames": []}}}}});
        let first = json!({"triggers": {}});
        let second = json!({"triggers": {"dropNames": ["小鲸鱼"]}});
        let third = json!({"triggers": {"keywords": ["海"], "dropNames": ["小鲸鱼"]}});
        let fourth = json!({"triggers": {"keywords": ["海"]}});
        super::record(&mut state["conversations"]["group:1"], &first, &second, "her", 1); // 1: added dropNames 小鲸鱼
        super::record(&mut state["conversations"]["group:1"], &second, &third, "her", 2); // 2: added keyword 海
        super::record(&mut state["conversations"]["group:1"], &third, &fourth, "her", 3); // 3: removed dropNames 小鲸鱼
        assert_eq!(super::undo(&mut state, "group:1", 1, 4).unwrap_err().0, 409, "entry 1's added name is already gone");
        assert_eq!(super::undo(&mut state, "group:1", 9, 4).unwrap_err().0, 404, "unknown entry");
        assert_eq!(super::undo(&mut state, "group:7", 1, 4).unwrap_err().0, 404, "unknown conversation");
        assert!(state["conversations"].get("group:7").is_none(), "a refused undo creates no conversation record");
        // With @ off, taking the only keyword away would leave an infinite sleep with no trigger: refused, nothing changes.
        state["conversations"]["group:1"]["wakeConfig"]["triggers"]["atMention"] = json!(false);
        let kept = state.clone();
        assert_eq!(super::undo(&mut state, "group:1", 2, 5).unwrap_err().0, 409);
        assert_eq!(state, kept);
        state["conversations"]["group:1"]["wakeConfig"]["triggers"]["atMention"] = json!(true);
        let undone = super::undo(&mut state, "group:1", 2, 6).unwrap();
        assert_eq!((undone["id"].clone(), undone["undid"].clone()), (json!(4), json!(2)));
        let conversation = &state["conversations"]["group:1"];
        assert_eq!(super::words_of(&conversation["wakeConfig"]), json!({"keywords": [], "dropNames": []}));
        assert_eq!(conversation["wakeConfig"]["triggers"]["atMention"], json!(true), "other settings stay");
        assert_eq!((conversation["wakeWordLog"][3]["by"].clone(), conversation["wakeWordLog"][3]["undoes"].clone()), (json!("undo"), json!(2)));
        assert_eq!(conversation["wakeWordLog"][3]["removed"], json!({"keywords": ["海"], "dropNames": []}));
        assert_eq!(super::undo(&mut state, "group:1", 2, 7).unwrap_err().0, 409, "undone already");
        super::undo(&mut state, "group:1", 3, 8).unwrap();
        assert_eq!(super::words_of(&state["conversations"]["group:1"]["wakeConfig"]), json!({"keywords": [], "dropNames": ["小鲸鱼"]}), "a removed name comes back");
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["triggers"]["keywords"], json!([]), "lists are written as lists");
    }

    #[test]
    fn drop_names_are_kept_by_wake_config_writes_like_keywords() {
        let input = |v: Value| super::drop_names_input(Some(&v), None);
        assert_eq!(input(json!([" 小鲸鱼 ", "", 3, "鲸鱼"])), Some(vec![json!("小鲸鱼"), json!("鲸鱼")]));
        assert_eq!(input(json!("小鲸鱼")), Some(Vec::new()), "a non-list clears the list");
        let names = |n: usize| (0..n).map(|i| format!("名{i}")).collect::<Vec<_>>();
        assert_eq!(input(json!(names(10))).map(|v| v.len()), Some(10), "ten names fit");
        assert_eq!(input(json!(names(11))).map(|v| v.len()), Some(10), "the eleventh is dropped");
        assert_eq!(input(json!(["长".repeat(40)])), Some(vec![json!("长".repeat(40))]), "forty characters fit");
        assert_eq!(input(json!(["长".repeat(41)])), Some(vec![json!("长".repeat(40))]), "the forty-first is cut");
        assert_eq!(super::drop_names_input(None, Some(&json!(["鲸鱼"]))), Some(vec![json!("鲸鱼")]), "absent keeps the current list");
        assert_eq!(super::drop_names_input(None, None), None);
        let cfg = json!({});
        let set = crate::wake::normalize_wake_config(&json!({"triggers": {"atMention": true}}), &json!({"triggers": {"dropNames": ["小鲸鱼"]}}), &cfg, "group:1", 0, 0.5).unwrap();
        assert_eq!(set["triggers"]["dropNames"], json!(["小鲸鱼"]));
        let kept = crate::wake::normalize_wake_config(&set, &json!({"triggers": {"keywords": ["海"]}}), &cfg, "group:1", 0, 0.5).unwrap();
        assert_eq!(kept["triggers"]["dropNames"], json!(["小鲸鱼"]), "a write that omits the list keeps it");
        let cleared = crate::wake::normalize_wake_config(&kept, &json!({"triggers": {"dropNames": []}}), &cfg, "group:1", 0, 0.5).unwrap();
        assert_eq!(cleared["triggers"]["dropNames"], json!([]));
        let untouched = crate::wake::normalize_wake_config(&json!({"triggers": {"atMention": true}}), &json!({}), &cfg, "group:1", 0, 0.5).unwrap();
        assert!(untouched["triggers"].get("dropNames").is_none(), "a configuration that never had the list gains none");
    }

    #[test]
    fn the_briefing_names_the_wake_words_and_the_ones_she_turned_off() {
        let (root, context) = fixture("briefing", &library());
        let on = super::briefing_line(&context, "group:1", &json!({}));
        assert!(on.starts_with("【名字唤醒】") && on.contains("（来自当前人设）：小鲸鱼、鲸鱼。想关掉") && on.contains("triggers.dropNames"), "{on}");
        let off = super::briefing_line(&context, "group:1", &json!({"wakeConfig": {"triggers": {"dropNames": ["鲸鱼"]}}}));
        assert!(off.contains("：小鲸鱼。你已关闭：鲸鱼。想关掉"), "{off}");
        let all = super::briefing_line(&context, "group:1", &json!({"wakeConfig": {"triggers": {"dropNames": ["小鲸鱼", "鲸鱼"]}}}));
        assert!(all.contains("：（都已关闭）。你已关闭：小鲸鱼、鲸鱼。"), "{all}");
        std::fs::write(root.join("data/personas.json"), r#"{"format":1,"nextId":1,"default":null,"overrides":{},"importedRolesAt":null,"personas":{}}"#).unwrap();
        assert_eq!(super::briefing_line(&context, "group:1", &json!({})), "", "no persona, no section");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resets_to_default_keep_dropped_names() {
        let (root, context) = fixture("resets", &library());
        let wake = json!({"mode": "diving", "infinite": true, "triggers": {"atMention": true, "keywords": ["海"], "dropNames": ["小鲸鱼"]},
            "noActionCount": 2, "confirmedAt": 5, "confirmedBy": "default"});
        std::fs::write(root.join("data/social-v2.json"), json!({"conversations": {"group:1": {"agentToken": "t1", "wakeConfig": wake, "lastActionAt": 0, "lastAiReplyAt": 0}}}).to_string()).unwrap();
        crate::console::refresh_default_wake_configs(&context).unwrap();
        let state = context.read_social_state().unwrap();
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["triggers"]["dropNames"], json!(["小鲸鱼"]), "the default refresh keeps them");
        // A third turn without any action resets the configuration to the defaults (noActionLimit 3).
        crate::console::complete_reserved2_turn(&context, &json!({}), "group:1", "t1", Some(1), &json!(5), 0).unwrap();
        let state = context.read_social_state().unwrap();
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["noActionCount"], json!(0), "the no-action reset ran");
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["triggers"]["dropNames"], json!(["小鲸鱼"]));
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["triggers"]["keywords"], json!(["海"]));
        // A turn that missed its closure after the reminders (miss 2 of 2) resets as well.
        let wake = json!({"mode": "active", "infinite": true, "triggers": {"atMention": true, "keywords": ["海"], "dropNames": ["小鲸鱼"]},
            "confirmedAt": 7, "confirmedBy": "set_wake_config"});
        std::fs::write(root.join("data/social-v2.json"), json!({"conversations": {"group:1": {"agentToken": "t1", "wakeConfig": wake}}}).to_string()).unwrap();
        crate::console::complete_reserved2_turn(&context, &json!({}), "group:1", "t1", None, &json!(7), 2).unwrap();
        let state = context.read_social_state().unwrap();
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["mode"], json!("diving"), "the missed-closure reset ran");
        assert_eq!(state["conversations"]["group:1"]["wakeConfig"]["triggers"]["dropNames"], json!(["小鲸鱼"]));
        let _ = std::fs::remove_dir_all(root);
    }
}
