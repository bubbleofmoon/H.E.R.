//! The one QQ conversation -> DSH session mapping of a selected runtime (ticket 10).
//!
//! File: `<data>/sessions.json`:
//! `{"sessions": {"<key>": "<sessionId>"}, "sessionPolicies": {"<key>": "<policy>"}}`.
//! Rust adds one top-level `workspaceId`; every other top-level field is preserved (semantically, not byte-for-byte: the file is re-serialized), so a
//! admin console (`/api/sessions`,
//! `/api/session/reset`) sees exactly the mapping the turn runner uses.
//!
//! Every read is fresh and every change is a locked read-modify-write, because the console
//! process resets mappings while the bridge process runs turns. A damaged file is an error, never
//! an empty mapping: starting a fresh session for every conversation would silently drop context.

use serde_json::{json, Map, Value};

use crate::runtime::{RuntimeArea, RuntimeContext};

pub const FILE: &str = "sessions.json";
const LOCK: &str = "locks/sessions.lock";
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn lock(context: &RuntimeContext) -> Result<std::fs::File, String> {
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, LOCK) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" => {
                if std::time::Instant::now() >= deadline {
                    return Err("会话映射正忙，稍后重试".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(format!("会话映射锁不可用：{error}")),
        }
    }
}

fn parse(text: Option<String>) -> Result<Map<String, Value>, String> {
    let Some(text) = text else { return Ok(Map::new()) };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("候选会话映射 JSON 损坏，拒绝新建会话：{error}"))?;
    let Value::Object(doc) = value else {
        return Err("会话映射不是 JSON 对象，拒绝新建会话".to_string());
    };
    if doc.get("sessions").is_some_and(|s| !s.is_object()) {
        return Err("会话映射的 sessions 不是对象，拒绝新建会话".to_string());
    }
    Ok(doc)
}

/// Read and validate the whole document (fresh from disk).
pub fn read(context: &RuntimeContext) -> Result<Map<String, Value>, String> {
    let text = context
        .read_optional_text(RuntimeArea::Data, FILE)
        .map_err(|error| format!("读取会话映射失败：{error}"))?;
    parse(text)
}

fn session_of(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("sessionId").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The DSH session currently mapped to `key`.
pub fn lookup(context: &RuntimeContext, key: &str) -> Result<Option<String>, String> {
    let doc = read(context)?;
    Ok(doc.get("sessions").and_then(|s| s.get(key)).and_then(session_of))
}

/// Every `(key, sessionId)` pair.
pub fn all(context: &RuntimeContext) -> Result<Vec<(String, String)>, String> {
    let doc = read(context)?;
    Ok(doc
        .get("sessions")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| session_of(v).map(|s| (k.clone(), s))).collect())
        .unwrap_or_default())
}

pub fn workspace_id(context: &RuntimeContext) -> Result<Option<String>, String> {
    let doc = read(context)?;
    Ok(doc.get("workspaceId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
}

fn modify(
    context: &RuntimeContext,
    change: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<bool, String> {
    let _gate = lock(context)?;
    let mut doc = read(context)?;
    if !change(&mut doc) {
        return Ok(false);
    }
    let bytes = serde_json::to_vec_pretty(&Value::Object(doc)).map_err(|e| e.to_string())?;
    context.write_atomic(RuntimeArea::Data, FILE, &bytes)?;
    Ok(true)
}

fn object<'a>(doc: &'a mut Map<String, Value>, field: &str) -> &'a mut Map<String, Value> {
    let entry = doc.entry(field.to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().expect("object ensured above")
}

pub fn set_workspace_id(context: &RuntimeContext, id: &str) -> Result<(), String> {
    modify(context, |doc| {
        doc.insert("workspaceId".to_string(), json!(id));
        true
    })
    .map(|_| ())
}

/// Map `key` to `session_id` (JS-compatible plain string) and store the JS session policy.
pub fn bind(context: &RuntimeContext, key: &str, session_id: &str, policy: Option<&str>) -> Result<(), String> {
    modify(context, |doc| {
        object(doc, "sessions").insert(key.to_string(), json!(session_id));
        if let Some(policy) = policy {
            object(doc, "sessionPolicies").insert(key.to_string(), json!(policy));
        }
        true
    })
    .map(|_| ())
}

/// Remove the mapping (and its policy) only if it still points at `expected`; returns whether
/// it did. A newer mapping created by someone else is never removed by a stale caller.
pub fn retire_if(context: &RuntimeContext, key: &str, expected: &str) -> Result<bool, String> {
    let retired = modify(context, |doc| {
        let current = doc.get("sessions").and_then(|s| s.get(key)).and_then(session_of);
        if current.as_deref() != Some(expected) {
            return false;
        }
        object(doc, "sessions").remove(key);
        if let Some(policies) = doc.get_mut("sessionPolicies").and_then(Value::as_object_mut) {
            policies.remove(key);
        }
        true
    })?;
    if retired {
        crate::pipeline::forget_failover_session(context, expected);
    }
    Ok(retired)
}

/// Clear session bindings and their policies while preserving the rest of the document.
/// The caller must hold the stopped-runtime owner guard; this function also holds the mapping gate.
pub fn reset_all(context: &RuntimeContext) -> Result<Option<String>, String> {
    let _gate = lock(context)?;
    let Some(original) = context.read_optional_text(RuntimeArea::Data, FILE).map_err(str::to_string)? else {
        return Ok(None);
    };
    let mut document = parse(Some(original.clone()))?;
    if document.get("sessionPolicies").is_some_and(|value| !value.is_object()) {
        return Err("session policies are not an object".into());
    }
    for field in ["sessions", "sessionPolicies"] {
        if document.contains_key(field) { document.insert(field.to_string(), json!({})); }
    }
    let changed = serde_json::to_vec_pretty(&Value::Object(document)).map_err(|error| error.to_string())?;
    static BACKUP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = BACKUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = crate::parity_clock::now_ms();
    let backup = format!("lifecycle/backups/sessions-{now}-{}-{seq}.json", std::process::id());
    let audit = |result: &str| -> Result<(), String> {
        let mut record = serde_json::to_vec(&json!({"v":1,"event":"sessions-reset","result":result,"at":now}))
            .map_err(|error| error.to_string())?;
        record.push(b'\n');
        context.append_bytes_durable(RuntimeArea::Data, "lifecycle/audit.jsonl", &record)
    };
    audit("intent")?;
    context.create_new_file(RuntimeArea::Data, &backup, original.as_bytes())?;
    if context.read_optional_text(RuntimeArea::Data, &backup).map_err(str::to_string)?.as_deref() != Some(original.as_str()) {
        return Err("session backup verification failed".into());
    }
    context.write_atomic(RuntimeArea::Data, FILE, &changed)?;
    audit("applied").map_err(|error| format!("session reset was applied but audit failed: {error}"))?;
    Ok(Some(backup))
}

fn legacy_fence_paths(root: &std::path::Path) -> Result<[std::path::PathBuf; 2], String> {
    let logs = root.parent().ok_or("legacy installation root is unavailable")?.join("logs");
    match std::fs::create_dir(&logs) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("legacy Rust gate directory is unavailable".into()),
    }
    let metadata = std::fs::symlink_metadata(&logs).map_err(|_| "legacy Rust gate directory is unavailable")?;
    if !metadata.is_dir() || crate::guarded_file::is_link(&metadata) { return Err("legacy Rust gate directory is redirected".into()); }
    Ok([root.join("state/bridge.lock"), logs.join("rbridge-live.lock")])
}

fn verify_legacy_fence(path: &std::path::Path, record: &[u8]) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "legacy launch fence is unavailable")?;
    if !metadata.is_dir() || crate::guarded_file::is_link(&metadata) || read_legacy(&path.join("retired.json"))? != record {
        return Err("legacy launch fence is occupied or belongs to another runtime".into());
    }
    Ok(())
}

/// Startup verification retained because selected production startup still calls it.
/// Publishing and import/export are retired; removal of this active check is an open scope item.
pub fn verify_cutover(context: &RuntimeContext) -> Result<(), String> {
    let required = context.read_optional_text(RuntimeArea::Data, "lifecycle/session-handover-required.json").map_err(str::to_string)?.is_some();
    let imported = if let Some(root) = context.runtime_root() {
        let path = root.join("import-manifest.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let manifest: Value = serde_json::from_slice(&read_legacy(&path)?).map_err(|_| "runtime import manifest is invalid")?;
                manifest["sourceLayout"] == "legacy-deployment"
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err("runtime import manifest is unavailable".into()),
        }
    } else { false };
    let Some(text) = context.read_optional_text(RuntimeArea::Data, "lifecycle/session-handover.json").map_err(str::to_string)? else {
        return if required || imported { Err("required cutover is incomplete; a guarded import is required".into()) } else { Ok(()) };
    };
    let manifest: Value = serde_json::from_str(&text).map_err(|_| "session handover record is invalid")?;
    if manifest["rustOwns"] != true { return Err("legacy owns the handover; a guarded import is required".into()); }
    let root = std::path::Path::new(manifest["legacyRoot"].as_str().ok_or("legacy handover source is missing")?);
    let record = serde_json::to_vec(&json!({"v":1,"runtimeRoot":context.runtime_root(),"legacyRoot":root})).map_err(|error| error.to_string())?;
    for fence in legacy_fence_paths(root)? { verify_legacy_fence(&fence, &record)?; }
    Ok(())
}

fn read_legacy(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "legacy file is unavailable")?;
    if !metadata.is_file() || crate::guarded_file::is_link(&metadata) || metadata.len() > 8 * 1024 * 1024 {
        return Err("legacy file is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path).map_err(|_| "legacy file cannot be opened")?.take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes).map_err(|_| "legacy file cannot be read")?;
    if bytes.len() > 8 * 1024 * 1024 { return Err("legacy file is too large".into()); }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-session-map-{tag}-{}-{}",
            std::process::id(),
            crate::turn_ledger::now_ms()
        )));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-console-token-0001"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:9","model":"http://127.0.0.1:9","onebot":"http://127.0.0.1:9"}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn js_written_mapping_is_read_and_extra_fields_survive_a_rust_change() {
        let root = root("js");
        std::fs::write(
            root.join("data/sessions.json"),
            r#"{"sessions":{"group:1":"js-session-1"},"sessionPolicies":{"group:1":"[\"reserved2\",\"qq-chat-v2\",null]"},"jsOnly":{"keep":true}}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        assert_eq!(lookup(&context, "group:1").unwrap().as_deref(), Some("js-session-1"));
        bind(&context, "group:2", "rust-session-2", Some("[\"reserved2\",\"qq-chat-v2\",null]")).unwrap();
        let doc: Value = serde_json::from_slice(&std::fs::read(root.join("data/sessions.json")).unwrap()).unwrap();
        assert_eq!(doc["sessions"]["group:1"], json!("js-session-1"));
        assert_eq!(doc["sessions"]["group:2"], json!("rust-session-2"));
        assert_eq!(doc["jsOnly"], json!({"keep": true}));
        assert_eq!(doc["sessionPolicies"]["group:2"], json!("[\"reserved2\",\"qq-chat-v2\",null]"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retire_only_removes_the_mapping_it_was_asked_about() {
        let root = root("retire");
        let context = RuntimeContext::from_root(&root).unwrap();
        bind(&context, "group:1", "old", Some("p")).unwrap();
        bind(&context, "group:1", "new", Some("p")).unwrap();
        assert!(!retire_if(&context, "group:1", "old").unwrap(), "a stale caller must not drop the new mapping");
        assert_eq!(lookup(&context, "group:1").unwrap().as_deref(), Some("new"));
        assert!(retire_if(&context, "group:1", "new").unwrap());
        assert_eq!(lookup(&context, "group:1").unwrap(), None);
        let doc = read(&context).unwrap();
        assert!(doc["sessionPolicies"].as_object().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_damaged_mapping_is_an_error_not_an_empty_start() {
        let root = root("bad");
        std::fs::write(root.join("data/sessions.json"), b"{not json").unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        assert!(lookup(&context, "group:1").is_err());
        assert!(bind(&context, "group:1", "x", None).is_err());
        assert_eq!(std::fs::read(root.join("data/sessions.json")).unwrap(), b"{not json");
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Briefing evidence shares the mapping gate. The social gate pins the epoch while writing;
/// a late collector cannot acknowledge a replacement session or a rotated token.
pub fn update_context(
    context: &RuntimeContext, key: &str, sid: &str, epoch: &str,
    change: impl FnOnce(&mut Value),
) -> Result<bool, String> {
    let _social = crate::console::acquire_social_state_gate(context).map_err(|(_, e)| e)?;
    if crate::console::conversation_token(context, key)?.as_deref() != Some(epoch) {
        return Ok(false);
    }
    modify(context, |doc| {
        if doc.get("sessions").and_then(|s| s.get(key)).and_then(session_of).as_deref() != Some(sid) {
            return false;
        }
        let record = object(doc, "briefings").entry(key.to_string()).or_insert_with(|| json!({}));
        if record["sid"] != sid || record["epoch"] != epoch || record["format"] != 1 {
            *record = json!({"sid":sid,"epoch":epoch,"format":1,"generation":0});
        }
        let before = record.clone();
        change(record);
        *record != before
    })
}

pub fn context_record(context: &RuntimeContext, key: &str, sid: &str, epoch: &str) -> Result<Value, String> {
    let doc = read(context)?;
    let record = doc.get("briefings").and_then(|r| r.get(key)).cloned().unwrap_or(Value::Null);
    Ok(if record["sid"] == sid && record["epoch"] == epoch && record["format"] == 1 {
        record
    } else { json!({"generation":0}) })
}

/// A rotation may have removed its mapping before the replacement token write failed.
/// Its retained checkpoint owns retirement until that exact epoch is durably revoked.
pub(crate) fn unrevoked_rotation(
    context: &RuntimeContext, key: &str, epoch: &str,
) -> Result<Option<String>, String> {
    let doc = read(context)?;
    let r = doc.get("briefings").and_then(|v| v.get(key)).unwrap_or(&Value::Null);
    Ok(if !epoch.is_empty() && r["epoch"] == epoch && r["format"] == 1
        && r["rotation"]["phase"] == "rotate"
        && r["rotation"]["turnSeq"].as_i64().is_some_and(|seq| seq > 0)
        && r["rotation"]["turnSeq"] == r["lastTurnSeq"] {
        r["sid"].as_str().filter(|sid| !sid.is_empty()).map(str::to_string)
    } else { None })
}

/// Last failure remains available after briefing replacement and session retirement.
pub(crate) fn record_rotation_failure(
    context: &RuntimeContext,
    key: &str,
    value: Value,
) -> Result<(), String> {
    modify(context, |doc| {
        object(doc, "rotationFailures").insert(key.into(), value);
        true
    })
    .map(|_| ())
}

/// Failure alerts survive restarts and session retirement, at most once per local day.
pub(crate) fn mark_rotation_alert(
    context: &RuntimeContext,
    key: &str,
    _sid: &str,
) -> Result<bool, String> {
    mark_rotation_alert_day(context, key, rotation_alert_day())
}

pub(crate) fn rotation_alert_day() -> i64 {
    (crate::parity_clock::now_ms() + crate::tasks::local_offset_ms()).div_euclid(86_400_000)
}

pub(crate) fn mark_rotation_alert_day(
    context: &RuntimeContext,
    key: &str,
    day: i64,
) -> Result<bool, String> {
    // The independent claim survives an unreadable sessions document. Serialize
    // both files under the same gate, before any possibly uncertain notification.
    const ALERT_FILE: &str = "rotation-alerts.json";
    let _gate = lock(context)?;
    let mut sessions = read(context);
    let mut independent = context
        .read_optional_text(RuntimeArea::Data, ALERT_FILE)
        .map_err(str::to_string)
        .and_then(parse);
    let markers: Vec<_> = [&independent, &sessions]
        .into_iter()
        .filter_map(|store| store.as_ref().ok()?.get("rotationAlerts")?.get(key))
        .collect();
    let last = markers.iter().filter_map(|v| v["day"].as_i64()).max();
    // Adopt legacy string markers conservatively for today, as before.
    let claimed = !last.is_some_and(|last| last >= day) && !markers.iter().any(|v| v.is_string());
    let stamp = last.unwrap_or(day).max(day);
    let mut durable = false;
    for (file, store) in [(ALERT_FILE, &mut independent), (FILE, &mut sessions)] {
        if let Ok(doc) = store {
            let alerts = object(doc, "rotationAlerts");
            if alerts.get(key).and_then(|v| v["day"].as_i64()) == Some(stamp) {
                durable = true;
            } else {
                alerts.insert(key.into(), json!({"day":stamp}));
                let bytes = serde_json::to_vec_pretty(doc).map_err(|e| e.to_string())?;
                durable |= context
                    .write_atomic(RuntimeArea::Data, file, &bytes)
                    .is_ok();
            }
        }
    }
    if durable || !claimed {
        Ok(claimed)
    } else {
        Err("rotation notice claim storage unavailable".into())
    }
}
