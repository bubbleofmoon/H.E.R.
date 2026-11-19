//! The global media settings.

use crate::settings::{kinds::{FileRule, Integer}, Setting};

macro_rules! setting {
    ($name:ident, $key:literal, $path:literal, $kind:expr, $label:literal, $hint:literal, $chat:literal) => {
        pub(crate) const $name: Setting = Setting {
            key: $key,
            pointer: $path,
            kind: &$kind,
            applies: "live",
            group: "media",
            label: $label,
            hint: $hint,
            chat: $chat,
            secret: false,
        };
    };
}

setting!(
    MEDIA_BYTES, "socialV2.media.maxBytes", "/socialV2/media/maxBytes",
    Integer { min: 262_144, max: 20_971_520, default: Some(8_388_608), file: FileRule::Media },
    "Media size", "Maximum source image bytes before normalization.", false
);
setting!(
    MEDIA_PIXELS, "socialV2.media.maxPixels", "/socialV2/media/maxPixels",
    Integer { min: 1_000_000, max: 64_000_000, default: Some(64_000_000), file: FileRule::Media },
    "Media pixels", "Maximum source image pixels before normalization.", false
);
setting!(
    MEDIA_COUNT, "socialV2.media.maxCount", "/socialV2/media/maxCount",
    Integer { min: 1, max: 8, default: Some(5), file: FileRule::Media },
    "Media count", "Maximum source images considered in a batch.", false
);
pub(super) const SETTINGS: &[Setting] = &[
    MEDIA_BYTES, MEDIA_PIXELS, MEDIA_COUNT,
];
