//! #140: model-visible prompt text as live data. Each text is a text setting of the registry (`settings::registry()`,
//! group "prompt text"), stored in `data/settings.db` and edited on the settings page; the text here is its compiled
//! default and the fallback. Readers take the configuration document the loader overlaid, so an edit is in effect at
//! the next wake (briefing and compatibility render) or the next harness turn (mode prompts). They check the value again
//! with the registry's own check, so a value it refuses (one in `config.json` with a denied term, say) renders the
//! default.

use serde_json::Value;

pub(crate) const NOT_A_CLERK_KEY: &str = "prompt.notAClerk";
pub(crate) const PROACTIVE_KEY: &str = "prompt.proactive";
pub(crate) const GROUP_CONDUCT_KEY: &str = "prompt.groupConduct";
pub(crate) const REPEAT_KEY: &str = "prompt.repeat";
pub(crate) const STICKER_STRATEGY_KEY: &str = "prompt.stickerStrategy";
pub(crate) const STICKER_SOFTEN_KEY: &str = "prompt.stickerSoften";
/// Mode prompts: the start of a harness turn's system message, one per mode profile (`harness::profile::Profile::prompt_key`).
pub(crate) const MODE_CHAT_KEY: &str = "prompt.mode.chat";
pub(crate) const MODE_OWNER_PRIVATE_KEY: &str = "prompt.mode.ownerPrivate";
pub(crate) const MODE_RESERVED2_KEY: &str = "prompt.mode.reserved2";
pub(crate) const MODE_RESERVED2_OWNER_KEY: &str = "prompt.mode.reserved2Owner";
pub(crate) const MODE_CLOSED_AGENT_KEY: &str = "prompt.mode.closedAgent";

/// The longest briefing rule or sticker text, in Unicode scalar values. Rule sections are never cut or counted by the
/// memory layers, so this bounds what one saved text adds to every briefing.
pub(crate) const RULE_MAX_CHARS: usize = 2000;
/// The longest mode prompt, in Unicode scalar values; it is sent with every model request of a harness turn.
pub(crate) const MODE_MAX_CHARS: usize = 8000;

pub(crate) const NOT_A_CLERK: &str = "";

pub(crate) const PROACTIVE: &str = "";

/// Group conduct of the reserved2 mode prompt (behaviour only; who she is belongs to the persona).
pub(crate) const GROUP_CONDUCT: &str = "【在群里怎么说话】\n- 你是群里的一员，不是主持人：不总结大家在聊什么，不点评每个人，不劝架也不站队，除非你自己被卷进来。\n- 一条消息只说一个意思，多数时候一句就够；话多时先挑最想说的那句，别写成一大段，也别拆成一串连发。\n- 不用列表、标题、加粗这些格式，也不主动给别人没要的建议。\n- 别人引用的消息不一定是在找你：只有引用了你的话、@ 了你或叫了你的名字才算。\n- 留意这个群平常怎么聊（句子多长、怎么称呼人、常用的梗和表情），慢慢跟上这个节奏，但不照抄别人的原话。\n- 隔了一阵再回来时，不用装作一直在线；接住眼下的话题就好，已经过去的事别硬拉回来。";

/// Her own catchphrases and bits: the send-path repeat guard only catches near-identical bubbles.
pub(crate) const REPEAT: &str = "【别反复用同一个梗】你自己的口头禅、固定句式和玩过的梗，在同一个会话里 24 小时内最多用一两次；发之前想想最近是不是刚说过，说过就换个说法，或者干脆不说。有人提醒你在重复，就把那句收起来一段时间。";

/// 表情策略文案（和 prompt 里那段完全一致，结构化接口 `/api/socialV2/prompt` 也用它）。
pub(crate) const STICKER_STRATEGY: &str = "【表情包策略：像真人一样用，不刷屏】
- 合适时机：被戳中笑点/槽点、接梗、怼人、赞同、自嘲、安慰、无语、赢了/输了、告别/晚安、别人发了表情时回一张，都可以自然用。
- 频率：普通闲聊不用每条都配；大约每 3~5 轮来一张就够，热闹/玩梗时可以更密，但不要连续刷屏。
- 选择：优先用备注（desc）和你的记忆（localNote/tags）能准确对上语境的；没有备注/不确定的表情，先 qq_get_sticker_image 看图再决定，不要瞎发。
- 发送：用 qq_send_sticker；一条消息只能是一张表情，不能在同一气泡里附带文字；想说的话先用 qq_send_message / qq_reply 作为单独气泡发出，再单独发表情。需要引用/点名时传 replyToMessageId / atUserId（群聊）。
- 不要：在严肃/正式/敏感话题硬塞表情；不要每次都用同一个；不要一条消息里塞多个表情；不要把文字和表情混在同一个气泡里；不要把表情包当回复的唯一内容（偶尔可以，但别让群友觉得你在敷衍）。
- 学习：看到新表情不确定含义时，先用 qq_get_sticker_image 看图，再用 qq_sticker_note 记下你的理解，下次就能更准地选。";

/// When a sticker or a `[表情N]` face softens a reply (wake text only, printed only with a sticker inventory).
pub(crate) const STICKER_SOFTEN: &str = "【用表情缓和语气】文字单独看会显得冲、冷淡或敷衍时（拒绝别人、回怼、吐槽、只回一两个字），发完文字后补一张对得上语境的表情包（用【可用表情包】里冒号前的 stickerId 调用 qq_send_sticker，清单里没有合适的就用 qq_list_stickers 找），或者在句末带一个 [表情N] 小表情，让语气软一点。对方真的难过、生气或在说正事时，别用表情打岔。同一张表情包、同一个小表情不要连着用。";

/// No compiled mode prompt: until the owner saves one, a harness turn's system message is the tool session line alone.
pub(crate) const NO_MODE_PROMPT: &str = "";

/// The text of the text setting `key` that the next use reads: the configuration's value when the registry accepts
/// it, else the setting's default. A key outside the registry, or one that is not a text setting, reads as empty.
pub(crate) fn text(cfg: &Value, key: &str) -> String {
    let Some(setting) = crate::settings::find(key) else {
        return String::new();
    };
    let Some(default) = setting.kind.text_default() else {
        return String::new();
    };
    cfg.pointer(setting.pointer)
        .and_then(|value| setting.normalize(value))
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| default.to_string())
}

/// Deliver validated prompt settings with the normal audit/default fallback and credential check.
pub(crate) fn model_text(context: &crate::runtime::RuntimeContext, cfg: &Value, key: &str) -> String {
    let Some(setting) = crate::settings::find(key) else {
        return String::new();
    };
    let Some(default) = setting.kind.text_default() else {
        return String::new();
    };
    let Ok(mut credentials) = crate::settings::stored_secrets(context) else { return String::new() };
    credentials.extend(crate::settings::harness_credential(context));
    let visible = |text: &str| {
        credentials.iter().all(|key| !text.contains(key)).then(|| text.to_owned())
    };
    visible(&text(cfg, key))
        .or_else(|| {
            crate::settings::last_good_text(context, key, |candidate| visible(candidate).is_some())
                .and_then(|candidate| visible(&candidate))
        })
        .or_else(|| visible(default))
        .unwrap_or_default()
}

/// One rule as its own paragraph: the model-visible text without trailing whitespace, then a blank line; nothing when it
/// is empty.
fn paragraph(context: &crate::runtime::RuntimeContext, cfg: &Value, key: &str) -> String {
    let full = model_text(context, cfg, key);
    let trimmed = full.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n\n")
    }
}

/// The behaviour rules for the briefing and compatibility render, in prompt order.
pub(crate) fn behaviour(context: &crate::runtime::RuntimeContext, cfg: &Value) -> String {
    [NOT_A_CLERK_KEY, PROACTIVE_KEY, GROUP_CONDUCT_KEY, REPEAT_KEY]
        .into_iter()
        .map(|key| paragraph(context, cfg, key))
        .collect()
}

/// The mode prompt `key` names, without surrounding whitespace; empty when none is set. Delivery reads `model_text`.
#[cfg(test)]
pub(crate) fn mode_prompt(cfg: &Value, key: &str) -> String {
    text(cfg, key).trim().to_string()
}
