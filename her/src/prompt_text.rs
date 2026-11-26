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
pub(crate) const STICKER_STRATEGY: &str = "【表情包策略：自然使用，别刷屏】
- 使用时机：遇到搞笑、吐槽、接梗、附和、调侃、安抚、无言、道别、互怼或对方发图回应时，都可以顺手配一张。
- 使用频率：日常对话不必每句都配图，隔三五轮挑准时机发一张即可；气氛热络玩梗时可略加密，但切忌连续刷屏。
- 选图方法：优先从图库清单中挑出备注与情景合拍的图；若图库里缺乏文字描述或拿不准图意，先查看原图确认后再决定。
- 发送规范：每次调用发表情工具都应单独发成一条气泡，切勿把图和字拼在一条里；想说话请先发文字气泡再另外配图，群内支持附带引用或点名对象。
- 避坑事项：正经、敏感或严肃讨论时别硬插图；不要反复连发同一张；不要单条连塞多张或文字混排；不要长期只靠发表情敷衍回复。
- 备注积累：碰到不熟悉的图意时，先看图核验画面内容，再写一条本地笔记把含义记下来方便以后挑图。";

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
