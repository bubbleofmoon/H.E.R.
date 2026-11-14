//! Persona wake words (GLOSSARY "Persona"): in reserved2 the names the conversation's active persona
//! declares wake her like configured keywords, switch with the persona (read from the library at every
//! ingress) and can be turned off one by one (`triggers.dropNames`). Every change of a conversation's
//! `keywords` or `dropNames` through the wake-config route is logged in its `wakeWordLog` as the words
//! added and removed (newest `LOG_MAX`) and can be undone from the console. Contract:
//! `docs/agents/prompts-modes.md` (Persona wake words).

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

/// Entries kept in a conversation's `wakeWordLog`.
pub(crate) const LOG_MAX: usize = 20;
/// `dropNames` keeps at most this many names of at most `NAME_MAX` characters (Unicode scalar values;
/// persona names are limited to 40 UTF-16 units, so every persona name fits).
const NAMES_MAX: usize = 10;
const NAME_MAX: usize = 40;
const LISTS: [&str; 2] = ["keywords", "dropNames"];

/// Trimmed non-empty strings of a JSON list; anything else is an empty list.
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// `word` is in `list`, compared case-insensitively.
fn listed(list: &[String], word: &str) -> bool {
    let word = word.to_lowercase();
    list.iter().any(|item| item.to_lowercase() == word)
}

/// The words that wake her in `key`: the persona's names she has not dropped, then her own keywords,
/// without duplicates (compared case-insensitively, the first spelling kept).
pub(crate) fn effective(context: &RuntimeContext, key: &str, keywords: &[String], dropped: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in crate::persona::wake_names(context, key) {
        if !listed(dropped, &name) && !listed(&out, &name) {
            out.push(name);
        }
    }
    for word in keywords {
        if !word.trim().is_empty() && !listed(&out, word) {
            out.push(word.clone());
        }
    }
    out
}

/// `triggers.dropNames` for a wake-config write: the input list when one is given (a non-list clears
/// it), else the current list; `None` when neither has one. Names are trimmed, empty ones dropped, at
/// most `NAMES_MAX` of at most `NAME_MAX` characters.
pub(crate) fn drop_names_input(input: Option<&Value>, current: Option<&Value>) -> Option<Vec<Value>> {
    let source = input.or(current)?;
    Some(strings(source).into_iter().take(NAMES_MAX).map(|name| Value::String(name.chars().take(NAME_MAX).collect())).collect())
}

/// `{keywords, dropNames}` of a wake configuration, as trimmed string lists.
pub(crate) fn words_of(wake_config: &Value) -> Value {
    let triggers = &wake_config["triggers"];
    json!({"keywords": strings(&triggers["keywords"]), "dropNames": strings(&triggers["dropNames"])})
}

/// The words of `from` that `other` lacks (exact match), in `from`'s order.
fn missing(from: &[String], other: &[String]) -> Vec<String> {
    from.iter().filter(|word| !other.contains(*word)).cloned().collect()
}

/// What changed between two wake configurations: (`{keywords, dropNames}` added, the same removed).
fn change(before: &Value, after: &Value) -> (Value, Value) {
    let (old, new) = (words_of(before), words_of(after));
    let (mut added, mut removed) = (json!({}), json!({}));
    for list in LISTS {
        let (was, now) = (strings(&old[list]), strings(&new[list]));
        added[list] = json!(missing(&now, &was));
        removed[list] = json!(missing(&was, &now));
    }
    (added, removed)
}

fn nothing(added: &Value, removed: &Value) -> bool {
    [added, removed].iter().all(|side| LISTS.iter().all(|list| side[*list].as_array().map_or(true, Vec::is_empty)))
}

/// Appends `entry` with the next id to `conversation.wakeWordLog`, keeping the newest `LOG_MAX`.
fn append(conversation: &mut Value, mut entry: Value) -> Option<u64> {
    if !conversation.is_object() {
        return None;
    }
    if !conversation["wakeWordLog"].is_array() {
        conversation["wakeWordLog"] = json!([]);
    }
    let list = conversation["wakeWordLog"].as_array_mut()?;
    let id = list.iter().filter_map(|e| e["id"].as_u64()).max().unwrap_or(0) + 1;
    entry["id"] = json!(id);
    list.push(entry);
    let excess = list.len().saturating_sub(LOG_MAX);
    list.drain(..excess);
    Some(id)
}

/// Logs the words added to and removed from `keywords` and `dropNames` between two wake configurations
/// as `{id, at, by, added, removed}` (ids count up per conversation). Returns the new id, or `None`
/// when no word was added or removed.
pub(crate) fn record(conversation: &mut Value, before: &Value, after: &Value, by: &str, at: i64) -> Option<u64> {
    let (added, removed) = change(before, after);
    if nothing(&added, &removed) {
        return None;
    }
    append(conversation, json!({"at": at, "by": by, "added": added, "removed": removed}))
}

/// Undoes log entry `id` of `key`: its added words are taken out and its removed words put back, only
/// while every added word is still there and every removed word still absent (otherwise a newer change
/// is in the way), and only when she can still be woken afterwards. A refusal changes nothing and never
/// creates a record. The undo is logged itself (`by: "undo"`, `undoes: id`).
pub(crate) fn undo(state: &mut Value, key: &str, id: u64, at: i64) -> Result<Value, (u16, String)> {
    undo_in(None, state, key, id, at)
}

/// `undo` that also counts the persona names of `context` as wake triggers. Stub until the fix.
pub(crate) fn undo_in(_context: Option<&RuntimeContext>, state: &mut Value, key: &str, id: u64, at: i64) -> Result<Value, (u16, String)> {
    undo_entry(state, key, id, at)
}

fn undo_entry(state: &mut Value, key: &str, id: u64, at: i64) -> Result<Value, (u16, String)> {
    let conversation = state
        .get_mut("conversations")
        .and_then(|all| all.get_mut(key))
        .filter(|c| c.is_object())
        .ok_or_else(|| (404, "conversation not found".to_string()))?;
    let entry = conversation["wakeWordLog"]
        .as_array()
        .and_then(|log| log.iter().find(|e| e["id"].as_u64() == Some(id)))
        .cloned()
        .ok_or_else(|| (404, "log entry not found".to_string()))?;
    let before = conversation["wakeConfig"].clone();
    if !before["triggers"].is_object() {
        return Err((409, "the conversation has no wake configuration".to_string()));
    }
    let mut next = before.clone();
    for list in LISTS {
        let added = strings(&entry["added"][list]);
        let removed = strings(&entry["removed"][list]);
        let mut words = strings(&before["triggers"][list]);
        if added.iter().any(|w| !words.contains(w)) || removed.iter().any(|w| words.contains(w)) {
            return Err((409, "the wake words changed after this entry; undo the newer entries first".to_string()));
        }
        words.retain(|w| !added.contains(w));
        words.extend(removed);
        next["triggers"][list] = json!(words);
    }
    if !crate::wake::is_wakeable(&crate::session::wake_config_from_json(Some(&next)), at) {
        return Err((409, "undoing this entry would leave no way to wake her".to_string()));
    }
    let (added, removed) = change(&before, &next);
    let words = words_of(&next);
    conversation["wakeConfig"] = next;
    let new_id = append(conversation, json!({"at": at, "by": "undo", "undoes": id, "added": added, "removed": removed}));
    Ok(json!({"ok": true, "conversation": key, "id": new_id, "undid": id, "wakeWords": words}))
}

/// `GET /api/personas/wake-words?conversation=`: the persona's names, her keywords and dropped names,
/// the words in effect and the change log, oldest first.
pub(crate) fn view(context: &RuntimeContext, conversation: &str) -> Result<Value, (u16, String)> {
    let key = crate::initial_prompts::canonical_key(conversation)
        .ok_or_else(|| (400, "conversation must be group:<id> or private:<id>".to_string()))?;
    let state = context.read_social_state().map_err(|_| (503, "social state is unavailable".to_string()))?;
    let conv = &state["conversations"][key.as_str()];
    let words = words_of(&conv["wakeConfig"]);
    let keywords = strings(&words["keywords"]);
    let dropped = strings(&words["dropNames"]);
    Ok(json!({"ok": true, "conversation": key, "personaNames": crate::persona::wake_names(context, &key),
        "keywords": keywords, "dropNames": dropped, "effective": effective(context, &key, &keywords, &dropped),
        "log": conv["wakeWordLog"].as_array().cloned().unwrap_or_default()}))
}

/// The briefing's `【名字唤醒】` rule section: which persona names wake her in `key`, which she turned
/// off and how to change them; empty when her persona declares no names.
pub(crate) fn briefing_line(context: &RuntimeContext, key: &str, conversation: &Value) -> String {
    let names = crate::persona::wake_names(context, key);
    if names.is_empty() {
        return String::new();
    }
    let dropped = strings(&conversation["wakeConfig"]["triggers"]["dropNames"]);
    let (off, on): (Vec<String>, Vec<String>) = names.into_iter().partition(|name| listed(&dropped, name));
    let on = if on.is_empty() { "（都已关闭）".to_string() } else { on.join("、") };
    let off = if off.is_empty() { String::new() } else { format!("你已关闭：{}。", off.join("、")) };
    format!("【名字唤醒】群里有人说到这些名字会唤醒你（来自当前人设）：{on}。{off}想关掉某个名字，就用 qq_set_wake_config 把要关掉的名字完整写进 triggers.dropNames（空列表全部恢复）；triggers.keywords 是你自己另加的唤醒词。每次改动都会记下来，管理员可以撤销。\n\n")
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

    #[test]
    fn undo_counts_the_persona_names_as_wake_triggers() {
        let (root, context) = fixture("undo-names", &library());
        let state = |dropped: &[&str]| {
            let mut state = json!({"conversations": {"group:1": {"wakeConfig": {"mode": "diving", "infinite": true,
                "triggers": {"atMention": false, "keywords": ["海"], "dropNames": dropped}}}}});
            super::record(&mut state["conversations"]["group:1"], &json!({"triggers": {"dropNames": dropped}}), &json!({"triggers": {"keywords": ["海"], "dropNames": dropped}}), "her", 1);
            state
        };
        let mut plain = state(&[]);
        assert_eq!(super::undo(&mut plain, "group:1", 1, 2).unwrap_err().0, 409, "without the persona's names her only trigger is the keyword");
        let mut named = state(&[]);
        super::undo_in(Some(&context), &mut named, "group:1", 1, 2).unwrap();
        assert_eq!(super::words_of(&named["conversations"]["group:1"]["wakeConfig"]), json!({"keywords": [], "dropNames": []}));
        let mut dropped = state(&["小鲸鱼", "鲸鱼"]);
        assert_eq!(super::undo_in(Some(&context), &mut dropped, "group:1", 1, 2).unwrap_err().0, 409, "every persona name is turned off: nothing would wake her");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn undo_refuses_to_restore_words_past_the_list_limits() {
        let at_limit = |list: &str, count: usize| {
            let words: Vec<String> = (0..count).map(|i| format!("词{i}")).collect();
            let mut triggers = json!({"atMention": true, "keywords": [], "dropNames": []});
            triggers[list] = json!(words);
            let mut removed = json!({"keywords": [], "dropNames": []});
            removed[list] = json!(["旧"]);
            json!({"conversations": {"group:1": {"wakeConfig": {"mode": "diving", "infinite": true, "triggers": triggers},
                "wakeWordLog": [{"id": 1, "at": 1, "by": "her", "added": {"keywords": [], "dropNames": []}, "removed": removed}]}}})
        };
        for (list, max) in [("dropNames", 10), ("keywords", 50)] {
            let mut full = at_limit(list, max);
            let kept = full.clone();
            assert_eq!(super::undo(&mut full, "group:1", 1, 2).unwrap_err().0, 409, "{list}: {max} words plus the restored one is over the limit");
            assert_eq!(full, kept, "a refusal changes nothing");
            let mut room = at_limit(list, max - 1);
            super::undo(&mut room, "group:1", 1, 2).unwrap();
            assert_eq!(room["conversations"]["group:1"]["wakeConfig"]["triggers"][list].as_array().map(Vec::len), Some(max), "{list}: exactly the limit fits");
        }
    }
}
