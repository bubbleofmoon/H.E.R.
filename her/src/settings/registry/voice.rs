//! Live settings for voice admission, processing limits and readback.

use crate::settings::{
    kinds::{FileRule, Integer, SettingKind, Switch, VoiceNumber},
    Setting,
};

const fn voice(key: &'static str, pointer: &'static str, kind: &'static dyn SettingKind, label: &'static str, hint: &'static str) -> Setting {
    Setting { key, pointer, kind, applies: "live", group: "voice", label, hint, chat: false, secret: false }
}

pub(crate) const VOICE_ENABLED: Setting = voice(
    "socialV2.voice.enabled", "/socialV2/voice/enabled", &Switch::new(false),
    "AI voice", "Allows voice tools. Operator sends are separately authorized.",
);
pub(crate) const VOICE_MAX_SECONDS: Setting = voice(
    "socialV2.voice.maxSeconds", "/socialV2/voice/maxSeconds",
    &VoiceNumber { max: crate::voice::QQ_MAX_SECONDS, default: crate::voice::QQ_MAX_SECONDS, duration: true },
    "Voice duration", "Positive seconds, up to QQ's fixed 60-second ceiling.",
);
pub(crate) const VOICE_MAX_BYTES: Setting = voice(
    "socialV2.voice.maxBytes", "/socialV2/voice/maxBytes",
    &Integer { min: 1, max: 100 * 1024 * 1024, default: Some(crate::voice::DEFAULT_MAX_BYTES as i64), file: FileRule::VoiceBytes },
    "Voice size", "Maximum source file size in bytes.",
);
pub(crate) const VOICE_MAX_MINUTE: Setting = voice(
    "socialV2.voice.maxPerMinute", "/socialV2/voice/maxPerMinute",
    &VoiceNumber { max: 60.0, default: 2.0, duration: false },
    "Voice sends per minute", "The minute budget for confirmed voice sends. Zero disables this budget.",
);
pub(crate) const VOICE_MAX_HOUR: Setting = voice(
    "socialV2.voice.maxPerHour", "/socialV2/voice/maxPerHour",
    &VoiceNumber { max: 600.0, default: 10.0, duration: false },
    "Voice sends per hour", "The hour budget for confirmed voice sends. Zero disables this budget.",
);
pub(crate) const VOICE_AUTO_READBACK: Setting = voice(
    "socialV2.voice.autoReadback", "/socialV2/voice/autoReadback", &Switch::new(true),
    "Voice readback", "Reads the delivered recording back for confirmation.",
);

pub(super) const SETTINGS: &[Setting] = &[
    VOICE_ENABLED, VOICE_MAX_SECONDS, VOICE_MAX_BYTES, VOICE_MAX_MINUTE, VOICE_MAX_HOUR, VOICE_AUTO_READBACK,
];
