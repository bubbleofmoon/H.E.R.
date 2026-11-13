//! Canonical QQ face notation shared by tool sends, the outbox and history verification.
use serde_json::{json, Value};

pub const GUIDANCE: &str = "[表情N] 会发送受支持的 QQ 原生表情；未知/格式错误的编号保留为文本，[表情] 和 [表情:摘要] 会移除。";

// QSid compatibility snapshot: NapNeko/NapCatQQ 98d781d19815d1625b24d717ce280ecf0a4f6d0c,
// packages/napcat-core/external/face_config.json. Explicit IDs, never a numeric range.
const FACE_IDS: &[u16] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 41, 42, 43, 46, 49, 53, 55, 56, 59, 60, 63,
    64, 66, 67, 74, 75, 76, 77, 78, 79, 85, 86, 89, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105,
    106, 107, 108, 109, 110, 111, 112, 114, 116, 118, 119, 120, 121, 122, 123, 124, 125, 129, 137,
    144, 146, 147, 148, 169, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 185,
    187, 193, 194, 198, 200, 201, 202, 203, 204, 206, 210, 211, 212, 214, 215, 216, 217, 218, 219,
    221, 222, 223, 224, 225, 226, 227, 229, 230, 231, 232, 233, 235, 237, 238, 239, 240, 241, 243,
    244, 262, 263, 264, 265, 266, 267, 268, 269, 270, 271, 272, 273, 277, 278, 281, 282, 283, 284,
    285, 286, 287, 288, 289, 290, 292, 293, 294, 295, 297, 298, 299, 300, 301, 302, 303, 305, 306,
    307, 311, 312, 314, 317, 318, 319, 320, 322, 323, 324, 325, 326, 332, 333, 334, 336, 337, 338,
    339, 341, 342, 343, 344, 345, 346, 347, 348, 349, 350, 351, 352, 353, 354, 355, 356, 357, 358,
    359, 360, 361, 362, 363, 364, 365, 366, 367, 368, 369, 370, 371, 372, 373, 374, 375, 376, 377,
    378, 379, 380, 381, 382, 383, 384, 385, 386, 387, 388, 389, 390, 391, 392, 393, 394, 395, 396,
    397, 398, 399, 400, 401, 402, 403, 404, 405, 406, 407, 408, 409, 410, 411, 412, 413, 415, 416,
    417, 419, 420, 421, 422, 423, 424, 425, 426, 427, 428, 429, 430, 431, 432, 450, 451, 452, 453,
    454, 455, 456, 457, 458, 459, 460, 461, 462, 463, 464, 465, 466, 467, 468, 469, 470, 472, 474,
    475, 476, 477, 478, 479, 480, 481, 482, 483, 484,
];

fn face_id(digits: &str) -> Option<u16> {
    let id = digits.parse::<u16>().ok()?;
    FACE_IDS.binary_search(&id).ok().map(|_| id)
}

fn tokens() -> &'static regex::Regex {
    static TOKENS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    TOKENS.get_or_init(|| regex::Regex::new(r"\[表情([0-9]+)\]").unwrap())
}

/// Splitters may cut between these atoms: one supported face token or one Unicode scalar.
pub(crate) fn atoms(text: &str) -> impl Iterator<Item = &str> {
    let mut faces = tokens().captures_iter(text).filter_map(|c| {
        face_id(&c[1])?;
        c.get(0)
    }).peekable();
    let mut cursor = 0;
    std::iter::from_fn(move || {
        if cursor == text.len() {
            return None;
        }
        let end = if faces.peek().is_some_and(|face| face.start() == cursor) {
            faces.next().unwrap().end()
        } else {
            cursor + text[cursor..].chars().next()?.len_utf8()
        };
        let atom = &text[cursor..end];
        cursor = end;
        Some(atom)
    })
}

pub fn normalize(text: &str) -> String {
    static NOTATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let notation =
        NOTATION.get_or_init(|| regex::Regex::new(r"\[表情([0-9]+)?\]|\[表情:[^\]]*\]").unwrap());
    let mut normalized = text.trim().to_owned();
    loop {
        let next = notation
            .replace_all(&normalized, |c: &regex::Captures<'_>| match c.get(1) {
                Some(digits) => face_id(digits.as_str())
                    .map(|id| format!("[表情{id}]"))
                    .unwrap_or_else(|| c[0].to_string()),
                None => String::new(),
            })
            .trim()
            .to_owned();
        if next == normalized {
            return normalized;
        }
        // Each changed pass removes bytes. Nested notation therefore terminates at one stable
        // representation, shared by auditing, admission, outbox/history and segment rendering.
        normalized = next;
    }
}

pub fn segments(text: &str) -> Vec<Value> {
    let normalized = normalize(text);
    let text = normalized.as_str();
    let mut out = Vec::new();
    let mut cursor = 0;
    for token in tokens().captures_iter(text) {
        let Some(id) = face_id(&token[1]) else {
            continue;
        };
        let whole = token.get(0).unwrap();
        if cursor < whole.start() {
            out.push(json!({"type":"text","data":{"text":crate::onebot::escape_cq(&text[cursor..whole.start()])}}));
        }
        out.push(json!({"type":"face","data":{"id":id.to_string()}}));
        cursor = whole.end();
    }
    if cursor < text.len() {
        out.push(json!({"type":"text","data":{"text":crate::onebot::escape_cq(&text[cursor..])}}));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf124_fix1_normalization_is_idempotent_for_adversarial_notation() {
        let samples = [
            "",
            "a",
            " \n\t",
            "[",
            "]",
            "[表",
            "情]",
            "[表情",
            "[表情]",
            "[表情:摘要]",
            "[表情28]",
            "[表情028]",
            "[表情999999]",
            "[表情-1]",
            "[表情２８]",
            "[表情x]",
            "[表情包:图]",
            "a[表情[表情]]b",
            "前[表情0[表情]28]后",
            "a[表[表情]情]b",
            "[表情:[表情]]",
            "\0",
        ];
        for left in samples {
            for right in samples {
                let input = format!("{left}{right}");
                let canonical = normalize(&input);
                assert_eq!(
                    normalize(&canonical),
                    canonical,
                    "normalization must be stable for {input:?}"
                );
            }
        }
        let mut nested = String::new();
        for _ in 0..64 {
            nested = format!("[表情{nested}]");
            assert_eq!(
                normalize(&nested),
                "",
                "every nested bare token is stripped: {nested:?}"
            );
        }
    }
}
