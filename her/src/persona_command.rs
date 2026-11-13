//! The administrator's private `/persona` command; groups and other users get a fixed refusal.
//! Intercepted before participation-mode dispatch like `/update` (`docs/agents/prompts-modes.md`).

use crate::runtime::RuntimeContext;
use serde_json::json;

pub const REFUSAL: &str = "人设只能由管理员在私聊或控制台切换。";
pub const ROLE_RETIRED: &str = "角色卡已停用：请用 /persona 或控制台的人设页切换人设。";

/// Whole-message forms: `/persona`, `/persona <name>`, `/persona <name|off> <conversation>`.
pub fn is_persona_command(plain: &str) -> bool {
    let words: Vec<&str> = plain.split_whitespace().collect();
    words.first().is_some_and(|w| w.eq_ignore_ascii_case("/persona")) && words.len() <= 3
}

pub fn handle(context: &RuntimeContext, key: &str, sender: &str, plain: &str) -> Result<(), String> {
    let cfg = context.load_config().map_err(str::to_string)?;
    let administrator = crate::chat::is_owner_key(&cfg, key) && crate::chat::owner_qq(&cfg) == sender;
    // A change commits only while the sender is still the administrator, checked again under the
    // configuration gate (the reply guard below protects only the reply).
    let authority = || -> Result<Option<std::fs::File>, (u16, String)> {
        let gate = crate::console::acquire_social_state_gate(context)?;
        let latest = context.load_config().map_err(|_| (503, "configuration unavailable".to_string()))?;
        if !crate::chat::is_owner_key(&latest, key) || crate::chat::owner_qq(&latest) != sender {
            return Err((403, "administrator changed".to_string()));
        }
        Ok(Some(gate))
    };
    let reply = if administrator { execute(context, plain, &authority) } else { REFUSAL.to_string() };
    let ob = crate::chat::onebot_of(context, &cfg)?;
    let guard = || -> Result<(), String> {
        let latest = context.load_config().map_err(str::to_string)?;
        if administrator {
            if !crate::chat::is_owner_key(&latest, key) || crate::chat::owner_qq(&latest) != sender {
                return Err("administrator changed".into());
            }
        } else if !crate::chat::allowed_key(&latest, key) {
            return Err("conversation is not allowed".into());
        }
        Ok(())
    };
    let report = crate::chat::send_recorded(context, &ob, key, "chat", "persona-command", &reply, 0, &guard);
    if report.error.is_some() || report.parts == 0 || report.confirmed != report.parts {
        return Err("persona reply was not confirmed".into());
    }
    Ok(())
}

/// Runs an administrator's command and returns the reply text.
pub(crate) fn execute(context: &RuntimeContext, plain: &str, authority: crate::persona::Authority<'_>) -> String {
    let words: Vec<&str> = plain.split_whitespace().collect();
    let listing = match crate::persona::list(context) {
        Ok(listing) => listing,
        Err((_, error)) => return format!("人设库不可用：{error}"),
    };
    let all = listing["personas"].as_array().cloned().unwrap_or_default();
    let id_of = |name: &str| all.iter().find(|p| p["name"] == name).and_then(|p| p["id"].as_str()).map(str::to_string);
    let body = match words.as_slice() {
        [_] => {
            let default = all.iter().find(|p| p["id"] == listing["default"]).and_then(|p| p["name"].as_str()).unwrap_or("无");
            let names: Vec<&str> = all.iter().filter_map(|p| p["name"].as_str()).collect();
            return format!("默认人设：{default}；可用人设：{}", if names.is_empty() { "无".to_string() } else { names.join("、") });
        }
        [_, name] => match id_of(name) {
            Some(id) => json!({"default": id}),
            None => return format!("人设「{name}」不存在。"),
        },
        [_, "off", key] => json!({"conversation": key, "persona": null}),
        [_, name, key] => match id_of(name) {
            Some(id) => json!({"conversation": key, "persona": id}),
            None => return format!("人设「{name}」不存在。"),
        },
        _ => return "用法：/persona、/persona <人设名>、/persona <人设名|off> <group:群号|private:QQ号>".to_string(),
    };
    match crate::persona::assign(context, &body, authority) {
        Ok(_) if body.get("default").is_some() => format!("默认人设已切换为「{}」。", words[1]),
        Ok(_) if body["persona"].is_null() => format!("已清除 {} 的人设覆盖。", words[2]),
        Ok(_) => format!("{} 的人设已切换为「{}」。", words[2], words[1]),
        Err((_, error)) => format!("人设切换失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn persona_commands_are_the_whole_message_forms_only() {
        for command in ["/persona", "/PERSONA", " /persona 小鲸鱼", "/persona 会计 group:1", "/persona off group:1"] {
            assert!(super::is_persona_command(command), "{command:?}");
        }
        for ordinary in ["/personas", "persona 小鲸鱼", "/persona a b c", "请 /persona", ""] {
            assert!(!super::is_persona_command(ordinary), "{ordinary:?}");
        }
    }

    #[test]
    fn an_administrator_command_lists_and_switches_the_default_persona() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../backups/ticket 19 persona fixtures")
            .join(format!("command-{}-{}", std::process::id(), crate::parity_clock::now_ms()));
        let root = crate::install_root::development_root(root);
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","ownerQQ":1,"socialV2":{"wake":{}}}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        let open: crate::persona::Authority<'static> = &crate::persona::open_authority;
        let created = crate::persona::create(&context, &serde_json::json!({"name": "会计", "core": "你是会计助手。"}), open).unwrap();
        crate::persona::approve(&context, &serde_json::json!({"id": created["id"], "version": 1}), open).unwrap();
        assert_eq!(super::execute(&context, "/persona", open), "默认人设：无；可用人设：会计");
        assert_eq!(super::execute(&context, "/persona 会计", open), "默认人设已切换为「会计」。");
        assert_eq!(crate::persona::load(&context).unwrap()["default"], "p1");
        assert_eq!(super::execute(&context, "/persona 不存在", open), "人设「不存在」不存在。");
        // The owner is re-checked before the change, not only before the reply.
        let moved = || -> Result<Option<std::fs::File>, (u16, String)> { Err((403, "administrator changed".to_string())) };
        let before = std::fs::read(root.join("data/personas.json")).unwrap();
        assert_eq!(super::execute(&context, "/persona off group:1", &moved), "人设切换失败：administrator changed");
        assert_eq!(std::fs::read(root.join("data/personas.json")).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }
}
