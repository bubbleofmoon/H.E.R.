//! Persona library: `<data>/personas.json` is owned here and nowhere else (storage, numbered
//! versions, approval and assignment). Contract: `docs/agents/prompts-modes.md` (Personas).

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::sync::Mutex;

pub const FILE: &str = "personas.json";
const GATE_FILE: &str = "locks/personas.lock";
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// UTF-16 units per text field (core, notes, compact).
const TEXT_MAX: usize = 20_000;
const NAME_MAX: usize = 40;
const NAMES_MAX: usize = 10;
/// Versions kept per persona.
const VERSIONS_MAX: usize = 50;
/// The fields a save compares with the latest version.
const CONTENT: [&str; 8] = ["name", "core", "notes", "compact", "names", "traits", "editMode", "autoRules"];
/// Parts auto edits may change; the name and the rule set are never auto-editable.
const PARTS: [&str; 5] = ["core", "notes", "compact", "names", "traits"];
static GATE: Mutex<()> = Mutex::new(());

type Failure = (u16, String);

/// Checked by every write before the library lock is taken; the returned guard is held until the
/// write commits (the console: configuration gate + fresh credential; `/persona`: gate + fresh owner).
pub type Authority<'a> = &'a dyn Fn() -> Result<Option<std::fs::File>, (u16, String)>;

#[cfg(test)]
pub(crate) fn open_authority() -> Result<Option<std::fs::File>, (u16, String)> {
    Ok(None)
}

fn fail(status: u16, message: &str) -> Failure {
    (status, message.to_string())
}

fn now() -> i64 {
    crate::parity_clock::now_ms() as i64
}

fn empty() -> Value {
    json!({"format": 1, "nextId": 1, "default": null, "overrides": {}, "importedRolesAt": null, "personas": {}})
}

/// The stored library; a missing file is an empty library. A damaged file is refused, never repaired.
pub fn load(context: &RuntimeContext) -> Result<Value, Failure> {
    let text = context
        .read_optional_text(RuntimeArea::Data, FILE)
        .map_err(|_| fail(503, "persona library is unavailable"))?;
    let Some(text) = text else { return Ok(empty()) };
    let doc: Value = serde_json::from_str(text.strip_prefix('\u{FEFF}').unwrap_or(&text))
        .map_err(|_| fail(503, "persona library is damaged"))?;
    if doc["format"] != 1 || !doc["personas"].is_object() || !doc["overrides"].is_object() || !doc["nextId"].is_u64() {
        return Err(fail(503, "persona library is damaged"));
    }
    Ok(doc)
}

/// One read-modify-write under the process mutex and `locks/personas.lock`.
fn mutate<T>(context: &RuntimeContext, authority: Authority<'_>, change: impl FnOnce(&mut Value) -> Result<T, Failure>) -> Result<T, Failure> {
    // Order: authority (the social gate) → process mutex → library file lock. The re-brief, which
    // takes the social gate itself, always runs after this guard is dropped.
    let _authority = authority()?;
    let _process = GATE.lock().unwrap_or_else(|p| p.into_inner());
    let deadline = std::time::Instant::now() + GATE_WAIT;
    let _file = loop {
        match context.open_locked_file(RuntimeArea::Data, GATE_FILE) {
            Ok(file) => break file,
            Err(error) if error == "runtime file is already locked" => {
                if std::time::Instant::now() >= deadline {
                    return Err(fail(503, "persona library is busy"));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return Err(fail(503, "persona library is unavailable")),
        }
    };
    let mut doc = load(context)?;
    let out = change(&mut doc)?;
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|_| fail(500, "persona library could not be encoded"))?;
    context
        .write_atomic(RuntimeArea::Data, FILE, &bytes)
        .map_err(|_| fail(500, "persona library could not be written"))?;
    Ok(out)
}

fn persona<'a>(doc: &'a Value, id: &str) -> Result<&'a Value, Failure> {
    doc["personas"].get(id).filter(|p| p.is_object()).ok_or_else(|| fail(404, "persona not found"))
}

fn version(persona: &Value, number: u64) -> Option<&Value> {
    persona["versions"].as_array()?.iter().find(|v| v["version"].as_u64() == Some(number))
}

fn latest(persona: &Value) -> Option<&Value> {
    persona["versions"].as_array()?.last()
}

/// A stored version with what rendering needs: a number, a name and a non-empty core.
fn complete(v: &Value) -> bool {
    v["version"].is_u64()
        && v["name"].as_str().is_some_and(|n| !n.trim().is_empty())
        && v["core"].as_str().is_some_and(|c| !c.trim().is_empty())
}

/// The version in effect: `activeVersion`, only when that record is approved and complete. Every
/// reader (prompt, assignment, auto-approval, names) resolves through here, so a bad stored record
/// is skipped and never rendered; the file itself is never rewritten to repair it.
fn active(persona: &Value) -> Option<&Value> {
    version(persona, persona["activeVersion"].as_u64()?).filter(|v| v["approved"] == true && complete(v))
}

/// The active version's name, else the latest version's.
fn current_name(persona: &Value) -> &str {
    active(persona).or_else(|| latest(persona)).and_then(|v| v["name"].as_str()).unwrap_or("")
}

fn name_taken(doc: &Value, name: &str, except: Option<&str>) -> bool {
    doc["personas"]
        .as_object()
        .is_some_and(|all| all.iter().any(|(id, p)| Some(id.as_str()) != except && current_name(p) == name))
}

fn next_id(doc: &mut Value) -> String {
    let n = doc["nextId"].as_u64().unwrap_or(1);
    doc["nextId"] = json!(n + 1);
    format!("p{n}")
}

fn id_of(body: &Value) -> Result<String, Failure> {
    body["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string).ok_or_else(|| fail(400, "id is required"))
}

fn text(value: &Value, field: &str, required: bool) -> Result<String, Failure> {
    let raw = match value {
        Value::String(raw) => raw.as_str(),
        Value::Null if !required => "",
        Value::Null => return Err(fail(400, &format!("{field} is required"))),
        _ => return Err(fail(400, &format!("{field} must be text"))),
    };
    let normalized = crate::initial_prompts::normalize_text(raw);
    if required && normalized.is_empty() {
        return Err(fail(400, &format!("{field} is required")));
    }
    if crate::role::utf16_len(&normalized) > TEXT_MAX {
        return Err(fail(400, &format!("{field} is too long")));
    }
    Ok(normalized)
}

fn short_name(value: &Value, field: &str) -> Result<String, Failure> {
    let name = text(value, field, true)?;
    if name.contains('\n') || crate::role::utf16_len(&name) > NAME_MAX {
        return Err(fail(400, &format!("{field} must be one line of at most 40 characters")));
    }
    Ok(name)
}

fn default_rules() -> Value {
    json!({"parts": ["notes"], "perWeek": 3, "monthlyDriftPause": true, "notify": true})
}

fn rules(value: &Value) -> Result<Value, Failure> {
    let Value::Object(map) = value else {
        return if value.is_null() { Ok(default_rules()) } else { Err(fail(400, "autoRules must be an object")) };
    };
    let mut out = default_rules();
    for (key, item) in map {
        match key.as_str() {
            "parts" => {
                let parts = item
                    .as_array()
                    .filter(|parts| parts.iter().all(|p| p.as_str().is_some_and(|p| PARTS.contains(&p))))
                    .ok_or_else(|| fail(400, "autoRules.parts may name core, notes, compact, names or traits"))?;
                out["parts"] = Value::Array(parts.clone());
            }
            "perWeek" => {
                let n = item.as_u64().filter(|n| *n <= 20).ok_or_else(|| fail(400, "autoRules.perWeek must be 0-20"))?;
                out["perWeek"] = json!(n);
            }
            "monthlyDriftPause" | "notify" => {
                let on = item.as_bool().ok_or_else(|| fail(400, &format!("autoRules.{key} must be true or false")))?;
                out[key.as_str()] = json!(on);
            }
            _ => return Err(fail(400, &format!("autoRules.{key} is not a rule"))),
        }
    }
    Ok(out)
}

fn pick<'a>(body: &'a Value, base: &'a Value, field: &str) -> &'a Value {
    if body.get(field).is_some() { &body[field] } else { &base[field] }
}

/// A new, unapproved version: `base` (the latest version, or nothing) overlaid with `body`.
fn build(base: &Value, body: &Value, number: u64, source: &str) -> Result<Value, Failure> {
    let name = short_name(pick(body, base, "name"), "name")?;
    let core = text(pick(body, base, "core"), "core", true)?;
    let notes = text(pick(body, base, "notes"), "notes", false)?;
    let compact = text(pick(body, base, "compact"), "compact", false)?;
    let names = match pick(body, base, "names") {
        Value::Null => vec![name.clone()],
        Value::Array(items) if items.len() <= NAMES_MAX => {
            items.iter().map(|item| short_name(item, "names")).collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err(fail(400, "names must be a list of up to 10 names")),
    };
    let traits = match pick(body, base, "traits") {
        Value::Null => json!({"neediness": 1}),
        Value::Object(map)
            if map.len() == 1 && map.get("neediness").and_then(Value::as_u64).is_some_and(|n| n <= 3) =>
        {
            json!({"neediness": map["neediness"].clone()})
        }
        _ => return Err(fail(400, "traits.neediness must be 0-3")),
    };
    let edit_mode = match pick(body, base, "editMode") {
        Value::Null => "auto".to_string(),
        Value::String(mode) if mode == "auto" || mode == "manual" => mode.clone(),
        _ => return Err(fail(400, "editMode must be auto or manual")),
    };
    let auto_rules = rules(pick(body, base, "autoRules"))?;
    Ok(json!({
        "version": number, "createdAt": now(), "source": source, "approved": false, "approvedAt": null,
        "name": name, "core": core, "notes": notes, "compact": compact, "names": names,
        "traits": traits, "editMode": edit_mode, "autoRules": auto_rules
    }))
}

/// Appends `next`; it is in effect at once when its core and compact equal the active version's.
fn settle(entry: &mut Value, mut next: Value) -> u64 {
    let number = next["version"].as_u64().unwrap_or(0);
    let same = active(entry).is_some_and(|a| a["core"] == next["core"] && a["compact"] == next["compact"]);
    if same {
        next["approved"] = json!(true);
        next["approvedAt"] = json!(now());
        entry["activeVersion"] = json!(number);
    }
    match entry["versions"].as_array_mut() {
        Some(list) => list.push(next),
        None => entry["versions"] = json!([next]),
    }
    prune(entry);
    number
}

/// Keeps at most `VERSIONS_MAX` versions: the oldest unapproved go first, then the oldest approved;
/// the active and the newest version are never dropped.
fn prune(entry: &mut Value) {
    let active = entry["activeVersion"].as_u64();
    let Some(list) = entry["versions"].as_array_mut() else { return };
    while list.len() > VERSIONS_MAX {
        let newest = list.last().and_then(|v| v["version"].as_u64());
        let keep = |v: &Value| v["version"].as_u64().is_some_and(|n| Some(n) == active || Some(n) == newest);
        let Some(index) = list.iter().position(|v| !keep(v) && v["approved"] != true).or_else(|| list.iter().position(|v| !keep(v))) else { break };
        list.remove(index);
    }
}

pub fn list(context: &RuntimeContext) -> Result<Value, Failure> {
    let doc = load(context)?;
    let mut personas: Vec<Value> = doc["personas"]
        .as_object()
        .map(|all| {
            all.values()
                .filter(|p| p.is_object())
                .map(|p| {
                    let pending: Vec<Value> = p["versions"]
                        .as_array()
                        .map(|vs| vs.iter().filter(|v| v["approved"] != true).map(|v| v["version"].clone()).collect())
                        .unwrap_or_default();
                    json!({"id": p["id"], "name": current_name(p), "activeVersion": p["activeVersion"],
                        "latestVersion": latest(p).map(|v| v["version"].clone()).unwrap_or(Value::Null),
                        "pendingVersions": pending})
                })
                .collect()
        })
        .unwrap_or_default();
    personas.sort_by_key(|p| p["id"].as_str().and_then(|id| id.strip_prefix('p')).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0));
    Ok(json!({"ok": true, "default": doc["default"], "overrides": doc["overrides"],
        "importedRolesAt": doc["importedRolesAt"],
        "rebriefDue": doc["rebriefDue"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "personas": personas}))
}

pub fn item(context: &RuntimeContext, id: &str) -> Result<Value, Failure> {
    let doc = load(context)?;
    let found = persona(&doc, id)?.clone();
    Ok(json!({"ok": true, "persona": found}))
}

pub fn create(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    mutate(context, authority, |doc| {
        let next = build(&Value::Null, body, 1, "console")?;
        if name_taken(doc, next["name"].as_str().unwrap_or(""), None) {
            return Err(fail(409, "persona name already exists"));
        }
        let id = next_id(doc);
        doc["personas"][id.as_str()] = json!({"id": id.clone(), "activeVersion": null, "versions": [next]});
        Ok(json!({"ok": true, "id": id, "version": 1}))
    })
}

pub fn update(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    let id = id_of(body)?;
    mutate(context, authority, |doc| {
        let found = persona(doc, &id)?;
        let base = latest(found).cloned().unwrap_or(Value::Null);
        let next = build(&base, body, base["version"].as_u64().unwrap_or(0) + 1, "console")?;
        // Saving what the latest version already says adds no version.
        if !base.is_null() && CONTENT.iter().all(|field| next[*field] == base[*field]) {
            return Ok(json!({"ok": true, "id": id, "version": base["version"], "activeVersion": found["activeVersion"], "unchanged": true}));
        }
        if name_taken(doc, next["name"].as_str().unwrap_or(""), Some(&id)) {
            return Err(fail(409, "persona name already exists"));
        }
        let entry = doc["personas"].get_mut(id.as_str()).ok_or_else(|| fail(404, "persona not found"))?;
        let number = settle(entry, next);
        Ok(json!({"ok": true, "id": id, "version": number, "activeVersion": entry["activeVersion"]}))
    })
}

/// Approves a version and puts it in effect; approving an older version rolls back.
pub fn approve(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    let id = id_of(body)?;
    let number = body["version"].as_u64().ok_or_else(|| fail(400, "version is required"))?;
    mutate(context, authority, |doc| {
        let target = version(persona(doc, &id)?, number).ok_or_else(|| fail(404, "version not found"))?;
        if !complete(target) {
            return Err(fail(409, "version is damaged"));
        }
        // Approval and rollback put this version's name in effect, so it must stay unique.
        if name_taken(doc, target["name"].as_str().unwrap_or(""), Some(&id)) {
            return Err(fail(409, "persona name already exists"));
        }
        let entry = doc["personas"].get_mut(id.as_str()).ok_or_else(|| fail(404, "persona not found"))?;
        let at = now();
        let list = entry["versions"].as_array_mut().ok_or_else(|| fail(503, "persona library is damaged"))?;
        let found = list.iter_mut().find(|v| v["version"].as_u64() == Some(number)).ok_or_else(|| fail(404, "version not found"))?;
        if found["approved"] != true {
            found["approved"] = json!(true);
            found["approvedAt"] = json!(at);
        }
        entry["activeVersion"] = json!(number);
        Ok(json!({"ok": true, "id": id, "activeVersion": number}))
    })
}

pub fn duplicate(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    let id = id_of(body)?;
    mutate(context, authority, |doc| {
        let source = persona(doc, &id)?;
        let from = active(source).or_else(|| latest(source)).cloned().ok_or_else(|| fail(409, "persona has no versions"))?;
        let mut next = build(&from, &json!({"name": body["name"]}), 1, "duplicate")?;
        if name_taken(doc, next["name"].as_str().unwrap_or(""), None) {
            return Err(fail(409, "persona name already exists"));
        }
        let approved = from["approved"] == true;
        if approved {
            next["approved"] = json!(true);
            next["approvedAt"] = json!(now());
        }
        let new_id = next_id(doc);
        doc["personas"][new_id.as_str()] = json!({"id": new_id.clone(),
            "activeVersion": if approved { json!(1) } else { Value::Null }, "versions": [next]});
        Ok(json!({"ok": true, "id": new_id, "version": 1}))
    })
}

pub fn delete(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    let id = id_of(body)?;
    mutate(context, authority, |doc| {
        persona(doc, &id)?;
        let assigned = doc["default"].as_str() == Some(id.as_str())
            || doc["overrides"].as_object().is_some_and(|o| o.values().any(|v| v.as_str() == Some(id.as_str())));
        if assigned {
            return Err(fail(409, "persona is assigned; clear its assignments first"));
        }
        if let Some(all) = doc["personas"].as_object_mut() {
            all.remove(&id);
        }
        Ok(json!({"ok": true, "id": id}))
    })
}

pub fn assign(context: &RuntimeContext, body: &Value, authority: Authority<'_>) -> Result<Value, Failure> {
    let (target, wanted) = assignment(body)?;
    let keys: Vec<String> = crate::session_map::all(context)
        .map_err(|error| (503, error))?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    // Every conversation whose persona changed, plus any whose earlier re-brief failed, is due:
    // `rebriefDue` {key: stamp} survives a failed or interrupted re-brief, so any later assignment
    // (repeating the same one included) re-runs it.
    let (result, due, stamp) = mutate(context, authority, |doc| {
        let before: Vec<Option<String>> = keys.iter().map(|key| effective_id(doc, key)).collect();
        apply_assignment(doc, target.as_deref(), wanted.as_deref())?;
        let view: &Value = doc;
        let mut due: Vec<String> = keys
            .iter()
            .zip(before)
            .filter(|(key, old)| effective_id(view, key) != *old)
            .map(|(key, _)| key.clone())
            .collect();
        let waiting: Vec<String> = view["rebriefDue"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
        for key in waiting {
            if !due.contains(&key) {
                due.push(key);
            }
        }
        let stamp = view["rebriefSeq"].as_u64().unwrap_or(0) + 1;
        let result = json!({"default": view["default"], "overrides": view["overrides"]});
        doc["rebriefSeq"] = json!(stamp);
        if !doc["rebriefDue"].is_object() {
            doc["rebriefDue"] = json!({});
        }
        for key in &due {
            doc["rebriefDue"][key.as_str()] = json!(stamp);
        }
        Ok((result, due, stamp))
    })?;
    // The library lock and the authority guard are released before the social gate is taken.
    let mut rebriefed = Vec::new();
    let mut failed = Vec::new();
    let mut settled = Vec::new();
    for key in due {
        match rebrief(context, &key) {
            Ok(true) => {
                rebriefed.push(key.clone());
                settled.push(key);
            }
            // No mapped session or a changed epoch: the next wake starts a fresh briefing anyway.
            Ok(false) => settled.push(key),
            Err(error) => failed.push(json!({"conversation": key, "error": error})),
        }
    }
    if !settled.is_empty() {
        // Best effort: an entry left behind is re-briefed once more by the next assignment. Only
        // this assignment's stamp is removed, never a newer assignment's.
        let _ = mutate(context, authority, |doc| {
            if let Some(waiting) = doc["rebriefDue"].as_object_mut() {
                waiting.retain(|key, value| !(settled.contains(key) && value.as_u64() == Some(stamp)));
            }
            Ok(())
        });
    }
    Ok(json!({"ok": true, "default": result["default"], "overrides": result["overrides"],
        "rebriefed": rebriefed, "rebriefFailed": failed}))
}

/// A switch starts a new briefing generation in the conversation's current session (as a
/// compaction does): its next wake carries a full briefing; memory and history stay. Without a
/// mapped session the next wake starts a fresh one anyway.
pub(crate) fn rebrief(context: &RuntimeContext, key: &str) -> Result<bool, String> {
    let Some(sid) = crate::session_map::lookup(context, key)? else { return Ok(false) };
    let state = context.read_social_state()?;
    let Some(epoch) = state["conversations"][key]["agentToken"].as_str().map(str::to_string) else { return Ok(false) };
    crate::session_map::update_context(context, key, &sid, &epoch, |record| {
        record["generation"] = json!(record["generation"].as_u64().unwrap_or(0) + 1);
        record["pending"] = Value::Null;
        record["needsBriefing"] = json!(true);
    })
}

/// `{default: id|null}` or `{conversation: key, persona: id|null}`.
fn assignment(body: &Value) -> Result<(Option<String>, Option<String>), Failure> {
    let target = match (body.get("default"), body.get("conversation")) {
        (Some(_), None) => None,
        (None, Some(conversation)) => Some(
            conversation
                .as_str()
                .and_then(crate::initial_prompts::canonical_key)
                .ok_or_else(|| fail(400, "conversation must be group:<id> or private:<id>"))?,
        ),
        _ => return Err(fail(400, "give either default or conversation")),
    };
    let wanted = match if target.is_some() { &body["persona"] } else { &body["default"] } {
        Value::Null => None,
        Value::String(id) => Some(id.clone()),
        _ => return Err(fail(400, "persona must be an id or null")),
    };
    Ok((target, wanted))
}

fn apply_assignment(doc: &mut Value, target: Option<&str>, wanted: Option<&str>) -> Result<(), Failure> {
    if let Some(id) = wanted {
        if active(persona(doc, id)?).is_none() {
            return Err(fail(409, "persona has no approved version"));
        }
    }
    match (target, wanted) {
        (None, _) => doc["default"] = json!(wanted),
        (Some(key), Some(id)) => doc["overrides"][key] = json!(id),
        (Some(key), None) => {
            if let Some(overrides) = doc["overrides"].as_object_mut() {
                overrides.remove(key);
            }
        }
    }
    Ok(())
}

/// Imports `resources/roles/*.md` as approved personas, each core being the card's reserved2 view (as the old v2 hint, uncut). The one run counts only when at least one card was imported.
pub fn import_roles(context: &RuntimeContext, authority: Authority<'_>) -> Result<Value, Failure> {
    let cards = role_cards(context)?;
    mutate(context, authority, |doc| {
        if !doc["importedRolesAt"].is_null() {
            return Err(fail(409, "role cards were already imported"));
        }
        let mut imported = Vec::new();
        let mut skipped = Vec::new();
        for (name, content) in &cards {
            let core = crate::prompt::role_card_v2_text(content);
            let built = build(&Value::Null, &json!({"name": name, "core": core, "names": [name]}), 1, "import");
            let mut next = match built {
                Ok(next) if !name_taken(doc, name, None) => next,
                _ => {
                    skipped.push(name.clone());
                    continue;
                }
            };
            next["approved"] = json!(true);
            next["approvedAt"] = json!(now());
            let id = next_id(doc);
            doc["personas"][id.as_str()] = json!({"id": id.clone(), "activeVersion": 1, "versions": [next]});
            imported.push(json!({"id": id, "name": name}));
        }
        if !imported.is_empty() {
            doc["importedRolesAt"] = json!(now());
        }
        Ok(json!({"ok": true, "imported": imported, "skipped": skipped}))
    })
}

fn role_cards(context: &RuntimeContext) -> Result<Vec<(String, String)>, Failure> {
    if !context.resources_dir().join("roles").is_dir() {
        return Ok(Vec::new());
    }
    let unavailable = || fail(503, "role cards are unavailable");
    let dir = context.resolve_path(RuntimeArea::Resources, "roles", PathIntent::ReadDirectory).map_err(|_| unavailable())?;
    let mut cards = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|_| unavailable())? {
        let path = entry.map_err(|_| unavailable())?.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        if name.is_empty() || name == "README" || crate::console::sanitize_role_name(&name) != name {
            continue;
        }
        let content = context.read_text(RuntimeArea::Resources, &format!("roles/{name}.md")).map_err(|_| unavailable())?;
        cards.push((name, content));
    }
    cards.sort_by(|a, b| crate::role::compare_names(&a.0, &b.0));
    Ok(cards)
}

fn effective_id(doc: &Value, key: &str) -> Option<String> {
    doc["overrides"].get(key).and_then(Value::as_str).or_else(|| doc["default"].as_str()).map(str::to_string)
}

/// Companion modes (chat, reserved, reserved2) use personas; closed-agent keeps role cards.
pub(crate) fn applies(context: &RuntimeContext) -> bool {
    context.is_isolated() && crate::console::runtime_mode(context) != "closed-agent"
}

/// `【人设】<name>\n<core>\n\n` for the conversation's persona (override, else default), or empty.
/// The core is cut at `limit` UTF-16 units (`role.maxInjectChars`) until #18 owns the prompt budget.
pub(crate) fn core_block(context: &RuntimeContext, key: &str, limit: usize) -> String {
    if !applies(context) {
        return String::new();
    }
    let Ok(doc) = load(context) else { return String::new() };
    let Some(found) = effective_id(&doc, key).and_then(|id| doc["personas"].get(&id).and_then(active).cloned()) else {
        return String::new();
    };
    let core = crate::social_mem::truncate_text(found["core"].as_str().unwrap_or(""), limit as i64);
    format!("【人设】{}\n{core}\n\n", found["name"].as_str().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    fn fixture(label: &str) -> (std::path::PathBuf, crate::runtime::RuntimeContext) {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../backups/ticket 19 persona fixtures")
            .join(format!("library-{label}-{}-{}", std::process::id(), crate::parity_clock::now_ms()));
        let root = crate::install_root::development_root(root);
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","ownerQQ":1,"socialV2":{"wake":{}}}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:1":{"agentToken":"fixture-epoch"}}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    fn open() -> super::Authority<'static> {
        &super::open_authority
    }

    fn new_id(created: Value) -> String {
        created["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn persona_writes_commit_only_while_the_authority_holds() {
        let (root, context) = fixture("authority");
        let revoked = || -> Result<Option<std::fs::File>, (u16, String)> { Err((401, "console credential was revoked".to_string())) };
        assert_eq!(super::create(&context, &json!({"name": "甲", "core": "核心。"}), &revoked).unwrap_err().0, 401);
        let (status, _, body) = crate::console_persona::route(&context, "POST", "/api/personas/create",
            &std::collections::HashMap::new(), r#"{"name":"甲","core":"核心。"}"#, false, &revoked);
        assert_eq!(status, 401, "{body}");
        assert!(!root.join("data/personas.json").exists(), "nothing was written");
        let _ = new_id(super::create(&context, &json!({"name": "甲", "core": "核心。"}), open()).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_an_approved_complete_version_can_be_assigned_or_approved() {
        let (root, context) = fixture("validation");
        std::fs::write(root.join("data/personas.json"), r#"{"format":1,"nextId":2,"default":null,"overrides":{},"importedRolesAt":null,"personas":{"p1":{"id":"p1","activeVersion":2,"versions":[{"version":1,"approved":true,"name":"甲","core":"甲的核心。"},{"version":2,"approved":false,"name":"甲","core":"草稿。"},{"version":3,"approved":false,"name":"甲"}]}}}"#).unwrap();
        assert_eq!(super::assign(&context, &json!({"default": "p1"}), open()).unwrap_err().0, 409, "an unapproved active version is not in effect");
        assert_eq!(super::approve(&context, &json!({"id": "p1", "version": 3}), open()).unwrap_err().0, 409, "a version without a core is never put in effect");
        super::approve(&context, &json!({"id": "p1", "version": 1}), open()).unwrap();
        assert_eq!(super::assign(&context, &json!({"default": "p1"}), open()).unwrap()["default"], "p1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_and_rollback_refuse_a_name_another_persona_uses() {
        let (root, context) = fixture("unique");
        let a = new_id(super::create(&context, &json!({"name": "甲", "core": "甲的核心。"}), open()).unwrap());
        super::approve(&context, &json!({"id": a, "version": 1}), open()).unwrap();
        // A pending rename to 乙: the core changes too, so it waits for approval.
        assert_eq!(super::update(&context, &json!({"id": a, "name": "乙", "core": "改名后的核心。"}), open()).unwrap()["activeVersion"], 1);
        let b = new_id(super::create(&context, &json!({"name": "乙", "core": "乙的核心。"}), open()).unwrap());
        super::approve(&context, &json!({"id": b, "version": 1}), open()).unwrap();
        assert_eq!(super::approve(&context, &json!({"id": a, "version": 2}), open()).unwrap_err().0, 409);
        let names: Vec<Value> = super::list(&context).unwrap()["personas"].as_array().unwrap().iter().map(|p| p["name"].clone()).collect();
        assert_eq!(names, vec![json!("甲"), json!("乙")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_identical_save_adds_no_version_and_at_most_fifty_versions_are_kept() {
        let (root, context) = fixture("versions");
        let id = new_id(super::create(&context, &json!({"name": "甲", "core": "核心。"}), open()).unwrap());
        super::approve(&context, &json!({"id": id, "version": 1}), open()).unwrap();
        assert_eq!(super::update(&context, &json!({"id": id, "notes": "笔记。"}), open()).unwrap()["version"], 2);
        let again = super::update(&context, &json!({"id": id, "notes": "笔记。"}), open()).unwrap();
        assert_eq!((again["version"].clone(), again["unchanged"].clone()), (json!(2), json!(true)));
        // v2 is active (a notes-only edit is in effect at once); 60 core drafts wait for approval.
        for n in 0..60 {
            super::update(&context, &json!({"id": id, "core": format!("草稿{n}。")}), open()).unwrap();
        }
        let item = super::item(&context, &id).unwrap();
        let numbers: Vec<u64> = item["persona"]["versions"].as_array().unwrap().iter().filter_map(|v| v["version"].as_u64()).collect();
        assert_eq!(numbers.len(), 50);
        assert_eq!(item["persona"]["activeVersion"], 2);
        assert_eq!((numbers[0], numbers[1], numbers[2], numbers[49]), (1, 2, 15, 62), "the oldest drafts went first: {numbers:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_role_import_that_imports_nothing_can_run_again() {
        let (root, context) = fixture("import");
        assert_eq!(super::import_roles(&context, open()).unwrap()["imported"], json!([]));
        assert!(super::load(&context).unwrap()["importedRolesAt"].is_null(), "nothing imported, so the one run is not used");
        std::fs::create_dir_all(root.join("resources/roles")).unwrap();
        std::fs::write(root.join("resources/roles/会计.md"), "你是会计助手，只讲数字。\n").unwrap();
        assert_eq!(super::import_roles(&context, open()).unwrap()["imported"].as_array().map(Vec::len), Some(1));
        assert!(super::load(&context).unwrap()["importedRolesAt"].is_number());
        assert_eq!(super::import_roles(&context, open()).unwrap_err().0, 409);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_rebrief_stays_due_and_repeating_the_assignment_retries_it() {
        let (root, context) = fixture("rebrief");
        let id = new_id(super::create(&context, &json!({"name": "甲", "core": "核心。"}), open()).unwrap());
        super::approve(&context, &json!({"id": id, "version": 1}), open()).unwrap();
        crate::session_map::bind(&context, "group:1", "sid-1", None).unwrap();
        // The social gate stays busy longer than a re-brief waits: the assignment is saved, its re-brief fails.
        let held = crate::console::acquire_social_state_gate(&context).unwrap();
        let first = super::assign(&context, &json!({"default": id}), open()).unwrap();
        drop(held);
        assert_eq!(first["rebriefFailed"][0]["conversation"], "group:1", "{first}");
        assert_eq!(super::list(&context).unwrap()["rebriefDue"], json!(["group:1"]));
        let generation = || crate::session_map::context_record(&context, "group:1", "sid-1", "fixture-epoch").unwrap()["generation"].clone();
        assert_eq!(generation(), json!(0));
        // The same assignment changes nothing, yet re-runs the re-brief that is still due.
        let again = super::assign(&context, &json!({"default": id}), open()).unwrap();
        assert_eq!(again["rebriefed"], json!(["group:1"]), "{again}");
        assert_eq!(generation(), json!(1));
        assert_eq!(super::list(&context).unwrap()["rebriefDue"], json!([]));
        let _ = std::fs::remove_dir_all(root);
    }
}
