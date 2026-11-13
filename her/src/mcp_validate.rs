//! Input validation of MCP tool arguments against a tool's JSON schema, with the issue texts of the JS
//! server (MCP SDK 1.30 + zod 4). The schema vocabulary is the one the 38 `qq_*` tools use: `type`
//! (string / number / integer / boolean / array / object), `enum`, `anyOf`, `minimum`, `maximum`,
//! `minLength`, `minItems`, `maxItems`, `items`, `properties`, `required`.
//!
//! Like zod's object parsing, keys the schema does not declare are dropped from the validated value.

use serde_json::{Map, Value};

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

fn received(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_name(schema: &Value) -> &'static str {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string",
        Some("number") | Some("integer") => "number",
        Some("boolean") => "boolean",
        Some("array") => "array",
        Some("object") => "object",
        _ => "unknown",
    }
}

fn type_matches(schema: &Value, value: &Value) -> bool {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => value.is_string(),
        Some("number") | Some("integer") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        _ => false,
    }
}

fn issue(path: &[String], message: String) -> String {
    if path.is_empty() {
        message
    } else {
        format!("{message} at {}", path.join("."))
    }
}

fn num_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// Returns the validated (stripped) value or the list of issue lines.
pub fn validate(schema: &Value, value: Option<&Value>) -> Result<Value, Vec<String>> {
    let mut issues = Vec::new();
    let out = check(schema, value, &mut Vec::new(), &mut issues);
    if issues.is_empty() {
        Ok(out.unwrap_or(Value::Null))
    } else {
        Err(issues)
    }
}

fn check(schema: &Value, value: Option<&Value>, path: &mut Vec<String>, issues: &mut Vec<String>) -> Option<Value> {
    let Some(value) = value else {
        let message = if schema.get("anyOf").is_some() { "Invalid input".to_string() } else { format!("Invalid input: expected {}, received undefined", type_name(schema)) };
        issues.push(issue(path, message));
        return None;
    };
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        let mut accepted: Option<Value> = None;
        for option in options {
            let mut sub = Vec::new();
            let mut p = path.clone();
            if let Some(v) = check(option, Some(value), &mut p, &mut sub) {
                if sub.is_empty() {
                    accepted = Some(v);
                    break;
                }
            }
        }
        if accepted.is_some() {
            return accepted;
        }
        // zod reports the issues of the branch when exactly one branch has the value's type; otherwise only "Invalid input".
        let same_type: Vec<&Value> = options.iter().filter(|o| type_matches(o, value)).collect();
        if same_type.len() == 1 {
            check(same_type[0], Some(value), path, issues);
        } else {
            issues.push(issue(path, "Invalid input".to_string()));
        }
        return None;
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if options.contains(value) {
            return Some(value.clone());
        }
        let list: Vec<String> = options.iter().map(|o| o.to_string()).collect();
        issues.push(issue(path, format!("Invalid option: expected one of {}", list.join("|"))));
        return None;
    }
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");
    let type_ok = match kind {
        "string" => value.is_string(),
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    };
    if !type_ok {
        issues.push(issue(path, format!("Invalid input: expected {}, received {}", type_name(schema), received(value))));
        return None;
    }
    match kind {
        "string" => {
            let s = value.as_str().unwrap_or("");
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if (s.encode_utf16().count() as u64) < min {
                    issues.push(issue(path, format!("Too small: expected string to have >={min} characters")));
                    return None;
                }
            }
            Some(value.clone())
        }
        "number" | "integer" => {
            let n = value.as_f64().unwrap_or(0.0);
            let mut bad = false;
            if kind == "integer" {
                if n.fract() != 0.0 && n.abs() <= MAX_SAFE {
                    issues.push(issue(path, "Invalid input: expected int, received number".to_string()));
                    return None;
                }
                if n.fract() == 0.0 && n.abs() > MAX_SAFE {
                    let word = if n > 0.0 { ("Too big", "<=", MAX_SAFE) } else { ("Too small", ">=", -MAX_SAFE) };
                    issues.push(issue(path, format!("{}: expected int to be {}{}", word.0, word.1, num_text(word.2))));
                    bad = true;
                }
            }
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if n < min {
                    issues.push(issue(path, format!("Too small: expected number to be >={}", num_text(min))));
                    bad = true;
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if n > max {
                    issues.push(issue(path, format!("Too big: expected number to be <={}", num_text(max))));
                    bad = true;
                }
            }
            if bad { None } else { Some(value.clone()) }
        }
        "array" => {
            let items = value.as_array().unwrap();
            let before = issues.len();
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    issues.push(issue(path, format!("Too small: expected array to have >={min} items")));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if (items.len() as u64) > max {
                    issues.push(issue(path, format!("Too big: expected array to have <={max} items")));
                }
            }
            let mut out = Vec::new();
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    if let Some(v) = check(item_schema, Some(item), path, issues) {
                        out.push(v);
                    }
                    path.pop();
                }
            } else {
                out = items.clone();
            }
            if issues.len() > before { None } else { Some(Value::Array(out)) }
        }
        "object" => {
            let obj = value.as_object().unwrap();
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut out = Map::new();
            let before = issues.len();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (name, prop_schema) in props {
                    path.push(name.clone());
                    match obj.get(name) {
                        None if !required.contains(&name.as_str()) => {}
                        found => {
                            if let Some(v) = check(prop_schema, found, path, issues) {
                                out.insert(name.clone(), v);
                            }
                        }
                    }
                    path.pop();
                }
            }
            if issues.len() > before { None } else { Some(Value::Object(out)) }
        }
        _ => Some(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type": "object", "properties": {
            "key": {"type": "string"},
            "afterSeq": {"type": "integer", "minimum": 0, "maximum": 9007199254740991u64},
            "messages": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}, "minItems": 1}]},
            "mode": {"type": "string", "enum": ["a", "b"]},
        }, "required": ["key"]})
    }

    #[test]
    fn strips_unknown_keys_and_accepts_valid_input() {
        let ok = validate(&schema(), Some(&json!({"key": "k", "extra": 1, "afterSeq": 0}))).unwrap();
        assert_eq!(ok, json!({"key": "k", "afterSeq": 0}));
    }

    #[test]
    fn issue_texts_follow_the_js_server() {
        let err = validate(&schema(), Some(&json!({"afterSeq": -1, "mode": "z", "messages": []}))).unwrap_err();
        assert_eq!(
            err,
            vec![
                "Invalid input: expected string, received undefined at key",
                "Too small: expected number to be >=0 at afterSeq",
                "Too small: expected array to have >=1 items at messages",
                "Invalid option: expected one of \"a\"|\"b\" at mode",
            ]
        );
        assert_eq!(
            validate(&schema(), Some(&json!({"key": "k", "afterSeq": 1.5}))).unwrap_err(),
            vec!["Invalid input: expected int, received number at afterSeq"]
        );
    }
}
