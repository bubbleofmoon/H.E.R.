//! Runtime configuration normalization and public participation mode aliases.

use serde_json::{json, Value};

const REASONING_DEFAULT: &str = "max";
const REASONING_OPTIONS: [&str; 4] = ["low", "medium", "high", "max"];

#[cfg(test)]
mod backup_settings_tests {
    use super::*;

    #[test]
    fn backup_old_count_migrates_to_seven_days_in_effective_config() {
        let (config, _) =
            canonical_config(&json!({"backup":{"keep":42,"includeState":false}})).unwrap();
        assert_eq!(config["backup"]["keepDays"], 7);
        assert!(config["backup"].get("keep").is_none());
        assert_eq!(config["backup"]["time"], "04:00");
        assert_eq!(config["backup"]["includeState"], false);
    }
}

/// Public mode spellings mapped to the internal spelling the runtime compares against.
/// `simulation` is the canonical public name of `reserved2`, for participation mode selection.
pub fn normalize_mode(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "chat" => Some("chat"),
        "reserved" => Some("reserved"),
        "reserved2" | "simulation" => Some("reserved2"),
        "closed-agent" => Some("closed-agent"),
        _ => None,
    }
}

/// The effective runtime configuration. Stored keys stay in place and are read through their
/// aliases; the returned notes name every place the canonical view differs from the raw file.
pub fn canonical_config(raw: &Value) -> Result<(Value, Vec<String>), String> {
    let mut config = raw.clone();
    let root = config
        .as_object_mut()
        .ok_or("config is not a JSON object")?;
    let mut notes = Vec::new();

    match root.get("ownerQQ").cloned() {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                root.remove("ownerQQ");
                notes.push("ownerQQ: empty string -> unset".to_string());
            } else {
                let qq = parse_qq(&text)?;
                root.insert("ownerQQ".into(), json!(qq));
                notes.push("ownerQQ: string -> number".to_string());
            }
        }
        Some(Value::Number(number)) => {
            parse_qq(&number.to_string())?;
        }
        Some(_) => return Err("ownerQQ must be a QQ number".to_string()),
    }

    for section in ["allow", "deny"] {
        let Some(object) = root.get(section).cloned() else {
            continue;
        };
        let object = object
            .as_object()
            .ok_or_else(|| format!("{section} must be a JSON object"))?;
        let private = resolve_list(object, "private", "privates");
        let groups = resolve_list(object, "groups", "group");
        let mut updated = object.clone();
        if !object.contains_key("private") && object.contains_key("privates") {
            notes.push(format!("{section}.privates -> {section}.private (alias)"));
        }
        if !object.contains_key("groups") && object.contains_key("group") {
            notes.push(format!("{section}.group -> {section}.groups (alias)"));
        }
        updated.insert("private".into(), json!(private));
        updated.insert("groups".into(), json!(groups));
        root.insert(section.into(), Value::Object(updated));
    }

    let dsh = match root.get("dsh").cloned() {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(object)) => object,
        Some(_) => return Err("dsh must be a JSON object".to_string()),
    };
    let mut dsh = dsh;
    let effort = match dsh.get("reasoningEffort").cloned() {
        None | Some(Value::Null) => {
            notes.push(format!(
                "dsh.reasoningEffort: missing -> {REASONING_DEFAULT}"
            ));
            REASONING_DEFAULT.to_string()
        }
        Some(value) => {
            let text = match &value {
                Value::String(s) => s.trim().to_lowercase(),
                other => other.to_string(),
            };
            if REASONING_OPTIONS.contains(&text.as_str()) {
                text
            } else {
                notes.push(format!(
                    "dsh.reasoningEffort: invalid -> {REASONING_DEFAULT}"
                ));
                REASONING_DEFAULT.to_string()
            }
        }
    };
    dsh.insert("reasoningEffort".into(), json!(effort));
    root.insert("dsh".into(), Value::Object(dsh));

    if raw["backup"].get("keep").is_some() {
        notes.push("backup.keep: retired; default backup.keepDays is 7".to_string());
    }
    root.insert("backup".into(), backup_config(raw)?);
    if raw.get("update").is_some() { root.insert("update".into(), crate::release::settings(raw)?); }

    Ok((config, notes))
}

/// Backup migration is shared by effective reads, saves and fresh per-run raw reads.
pub fn backup_config(raw: &Value) -> Result<Value, String> {
    if !raw.is_object() {
        return Err("config is not a JSON object".into());
    }
    let mut backup = match raw.get("backup") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(fields)) => fields.clone(),
        _ => return Err("backup must be a JSON object".into()),
    };
    backup.remove("keep");
    for (key, default) in [
        ("keepDays", json!(7)),
        ("time", json!("04:00")),
        ("includeState", json!(true)),
    ] {
        if backup.get(key).is_none_or(Value::is_null) {
            backup.insert(key.into(), default);
        }
    }
    if backup["keepDays"].as_u64().is_none_or(|days| days < 1) {
        return Err("backup.keepDays must be an integer of at least 1".into());
    }
    if !backup["includeState"].is_boolean() {
        return Err("backup.includeState must be a boolean".into());
    }
    let time = backup["time"]
        .as_str()
        .ok_or("backup.time must be HH:MM (00:00-23:59)")?;
    let bytes = time.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![bytes[0], bytes[1], bytes[3], bytes[4]]
            .iter()
            .all(u8::is_ascii_digit)
        || (bytes[0] - b'0') * 10 + (bytes[1] - b'0') > 23
        || (bytes[3] - b'0') * 10 + (bytes[4] - b'0') > 59
    {
        return Err("backup.time must be HH:MM (00:00-23:59)".into());
    }
    match backup.get("dir") {
        None | Some(Value::Null) => {}
        Some(Value::String(value)) => {
            if value.is_empty() {
                backup.insert("dir".into(), Value::Null);
            }
        }
        _ => return Err("backup.dir must be a path string or null".into()),
    }
    Ok(Value::Object(backup))
}

type Map = serde_json::Map<String, Value>;

/// ID-list normalization with nullish alias precedence: a present non-array value gives an empty list.
fn resolve_list(section: &Map, preferred: &str, alias: &str) -> Vec<String> {
    let value = match section.get(preferred) {
        Some(value) if !value.is_null() => Some(value),
        _ => section.get(alias).filter(|value| !value.is_null()),
    };
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.trim().to_string(),
            other => other.to_string(),
        })
        .filter(|text| !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

fn parse_qq(text: &str) -> Result<u64, String> {
    let message = "ownerQQ must be a positive QQ number";
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return Err(message.to_string());
    }
    let qq: u64 = text.parse().map_err(|_| message.to_string())?;
    if qq == 0 || qq > 9_007_199_254_740_991 {
        return Err(message.to_string());
    }
    Ok(qq)
}
