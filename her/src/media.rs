//! Frozen-JS media acquisition/fallbacks, shared by inline delivery and media tools.
use crate::media_normalize::{normalize, Normalized};
use crate::onebot::OneBot;
use crate::runtime::RuntimeContext;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

// Reserve the worst-case source size before any I/O. No queued request retains
// source bytes: four permits cap decoded source buffers at 4 * 20 MiB/process.
static SOURCE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub fn admit_source() -> Option<tokio::sync::SemaphorePermit<'static>> {
    SOURCE_GATE.try_acquire().ok()
}

pub fn read_image_path(path: &std::path::Path, max_bytes: usize) -> Result<Vec<u8>, ()> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| ())?;
    if file.metadata().map_err(|_| ())?.len() > max_bytes as u64 {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > max_bytes {
        return Err(());
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub bytes: usize,
    pub pixels: u64,
    pub count: usize,
}

impl Limits {
    pub fn from_config(cfg: &Value) -> Self {
        // JS Number + Math.round, followed by the frozen bridge's safety clamps.
        fn number(v: Option<&Value>, default: f64, low: f64, high: f64) -> u64 {
            let n = v
                .map(crate::console::social_v1_number)
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(default);
            n.round().clamp(low, high) as u64
        }
        let m = cfg.pointer("/socialV2/media");
        Self {
            bytes: number(
                m.and_then(|m| m.get("maxBytes")),
                8_388_608.0,
                262_144.0,
                20_971_520.0,
            ) as usize,
            pixels: number(
                m.and_then(|m| m.get("maxPixels")),
                64_000_000.0,
                1_000_000.0,
                64_000_000.0,
            ),
            count: number(m.and_then(|m| m.get("maxCount")), 5.0, 1.0, 8.0) as usize,
        }
    }
}

fn safe_file_ref(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 512
        && !file.contains("..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+=@-".contains(&b))
}

fn emoji_id(media: &Value) -> String {
    let explicit = media
        .get("emojiId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    let file = media.get("file").and_then(Value::as_str).unwrap_or("");
    let lower = file.to_ascii_lowercase();
    if ![".gif", ".png", ".jpg", ".jpeg", ".webp"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        return String::new();
    }
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
    let tail: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if tail.len() == 32 {
        tail
    } else {
        String::new()
    }
}

fn accepted(bytes: Vec<u8>, limits: Limits) -> Option<(Normalized, usize)> {
    if bytes.len() > limits.bytes {
        return None;
    }
    let n = normalize(&bytes, limits.pixels).ok()?;
    Some((n, bytes.len()))
}

fn from_url(url: &str, limits: Limits) -> Option<(Normalized, usize)> {
    accepted(
        crate::safe_fetch::fetch_bytes(url, limits.bytes)
            .ok()?
            .bytes,
        limits,
    )
}

fn from_gateway(
    context: &RuntimeContext,
    cfg: &Value,
    ob: &OneBot,
    media: &Value,
    limits: Limits,
) -> Option<(Normalized, usize)> {
    let file = media.get("file").and_then(Value::as_str)?.trim();
    if !safe_file_ref(file) {
        return None;
    }
    let reply = ob.request_bounded(
        "get_image",
        json!({"file": file}),
        limits.bytes.saturating_mul(4) / 3 + 4096,
    )?;
    if reply.get("retcode").and_then(Value::as_i64).unwrap_or(0) != 0 || reply["status"] == "failed"
    {
        return None;
    }
    let obj = reply.get("data")?;
    for name in ["data", "base64", "file"] {
        if let Some(s) = obj.get(name).and_then(Value::as_str) {
            // Bound encoded input before allocating the whitespace-free representation.
            if s.len() > limits.bytes.saturating_mul(4) / 3 + 4096 {
                continue;
            }
            let s = s.trim();
            let s = s
                .strip_prefix("base64://")
                .or_else(|| {
                    s.starts_with("data:image/")
                        .then(|| s.split_once(',').map(|(_, b)| b))
                        .flatten()
                })
                .unwrap_or(s);
            let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            if compact.len().saturating_mul(3) / 4 > limits.bytes {
                continue;
            }
            if let Ok(bytes) = STANDARD.decode(compact) {
                if let Some(img) = accepted(bytes, limits) {
                    return Some(img);
                }
            }
        }
    }
    if let Some(url) = obj.get("url").and_then(Value::as_str) {
        if let Some(img) = from_url(url, limits) {
            return Some(img);
        }
    }
    // A gateway-returned local file is trusted only inside the explicitly configured home;
    // selected roots additionally require it to be inside that root (no legacy fallback).
    let path = std::fs::canonicalize(obj.get("file").and_then(Value::as_str)?).ok()?;
    let home =
        std::fs::canonicalize(cfg.pointer("/snowluma/homeDir").and_then(Value::as_str)?).ok()?;
    if !path.starts_with(home) {
        return None;
    }
    if let Some(root) = context.runtime_root() {
        if !path.starts_with(std::fs::canonicalize(root).ok()?) {
            return None;
        }
    }
    accepted(read_image_path(&path, limits.bytes).ok()?, limits)
}

fn fetch_image(
    context: &RuntimeContext,
    cfg: &Value,
    ob: Option<&OneBot>,
    media: &Value,
    limits: Limits,
) -> Option<(Normalized, usize)> {
    let id = emoji_id(media);
    let sticker = media["kind"] == "sticker"
        || !id.is_empty()
        || media
            .get("subType")
            .map(|v| v == "1" || v == 1)
            .unwrap_or(false);
    let gateway = || ob.and_then(|ob| from_gateway(context, cfg, ob, media, limits));
    let urls = || {
        if let Some(url) = media.get("url").and_then(Value::as_str) {
            if let Some(img) = from_url(url, limits) {
                return Some(img);
            }
        }
        if (16..=64).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_hexdigit()) {
            let candidate = format!(
                "https://gxh.vip.qq.com/club/item/parcel/item/{}/{id}/raw300.gif",
                &id[..2]
            );
            if media.get("url").and_then(Value::as_str).map(str::trim) == Some(candidate.as_str()) {
                return None;
            }
            return from_url(&candidate, limits);
        }
        None
    };
    if sticker {
        urls().or_else(gateway)
    } else {
        gateway().or_else(urls)
    }
}

pub fn image_values(n: &Normalized) -> Vec<Value> {
    n.images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let mut v = json!({"mimeType": img.mime, "data": STANDARD.encode(&img.bytes)});
            if n.images.len() > 1 {
                v["frameIndex"] = json!(i + 1);
                if i == 0 {
                    v["text"] = json!(n.note);
                }
            }
            if img.mime != n.original_mime {
                v["originalMimeType"] = json!(n.original_mime);
            }
            v
        })
        .collect()
}

pub fn images(context: &RuntimeContext, cfg: &Value, media_list: &[Value]) -> Vec<Value> {
    let limits = Limits::from_config(cfg);
    let ob = crate::console::media_onebot(context, cfg).ok();
    let mut out = Vec::new();
    let mut total = 0;
    for (i, media) in media_list.iter().enumerate() {
        let kind = match media.get("kind").and_then(Value::as_str) {
            Some("face") => "face",
            Some("sticker") => "sticker",
            _ => "image",
        };
        let mut base = json!({"index": i + 1, "kind": kind});
        if i >= limits.count {
            base["text"] = json!(format!("（超过单条上限 {}，已跳过）", limits.count));
            out.push(base);
            continue;
        }
        if !media.is_object() {
            continue;
        }
        for field in ["file", "url", "emojiId", "summary", "subType"]
            .into_iter()
            .filter(|_| kind != "face")
        {
            if let Some(v) = media
                .get(field)
                .filter(|v| !v.is_null() && *v != "" && *v != 0 && *v != false)
            {
                base[field] = json!(v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()));
            }
        }
        let Some(_source) = admit_source() else {
            base["text"] = json!(if kind == "face" {
                let id = media
                    .get("faceId")
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_default();
                base["faceId"] = json!(id);
                let summary = media.get("summary").and_then(Value::as_str).unwrap_or("");
                if summary.is_empty() {
                    format!("[表情#{id}]")
                } else {
                    format!("[表情:{summary}]")
                }
            } else {
                format!(
                    "（{}获取失败）",
                    if kind == "sticker" {
                        "表情"
                    } else {
                        "图片"
                    }
                )
            });
            out.push(base);
            continue;
        };
        let (image, fallback, success_text) = if kind == "face" {
            let id = media
                .get("faceId")
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default();
            if !id.is_empty() {
                base["faceId"] = json!(id);
            }
            let summary = media.get("summary").and_then(Value::as_str).unwrap_or("");
            let fallback = if summary.is_empty() {
                format!("[表情#{id}]")
            } else {
                format!("[表情:{summary}]")
            };
            let face = id
                .parse::<i64>()
                .ok()
                .and_then(|id| {
                    ob.as_ref()?.request_bounded(
                        "fetch_face_entity",
                        json!({"face_id": id, "refresh": false}),
                        64 * 1024,
                    )
                })
                .filter(|r| r.get("retcode").and_then(Value::as_i64).unwrap_or(0) == 0)
                .and_then(|r| r.get("data").cloned());
            let desc = face
                .as_ref()
                .and_then(|f| {
                    f.get("q_des")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| f.get("emoji_name_alias")?.as_array()?.first()?.as_str())
                })
                .unwrap_or("");
            let img = face
                .as_ref()
                .and_then(|f| f.get("url")?.as_str())
                .and_then(|u| from_url(u, limits));
            let text = if desc.is_empty() {
                String::new()
            } else {
                format!("[表情:{desc}]")
            };
            (
                img,
                if text.is_empty() {
                    fallback
                } else {
                    text.clone()
                },
                text,
            )
        } else {
            (
                fetch_image(context, cfg, ob.as_ref(), media, limits),
                format!(
                    "（{}获取失败）",
                    if kind == "sticker" {
                        "表情"
                    } else {
                        "图片"
                    }
                ),
                String::new(),
            )
        };
        if let Some((n, raw_len)) = image {
            if total + raw_len > limits.bytes {
                base["text"] = json!(format!(
                    "（{}总大小超限，已跳过）",
                    if kind == "sticker" {
                        "表情"
                    } else {
                        "图片"
                    }
                ));
                out.push(base);
                continue;
            }
            total += raw_len;
            for img in image_values(&n) {
                let mut entry = base.clone();
                entry["text"] = json!(success_text);
                for (k, v) in img.as_object().into_iter().flatten() {
                    entry[k] = v.clone();
                }
                if !success_text.is_empty() {
                    if let Some(note) = img.get("text").and_then(Value::as_str) {
                        entry["text"] = json!(format!("{success_text} {note}"));
                    }
                }
                out.push(entry);
            }
        } else {
            base["text"] = json!(fallback);
            out.push(base);
        }
    }
    out
}

pub fn model_parts(context: &RuntimeContext, cfg: &Value, list: &[Value]) -> Vec<Value> {
    let mut parts = Vec::new();
    for img in images(context, cfg, list) {
        let label = if img["kind"] == "face" {
            "表情"
        } else {
            "图片"
        };
        parts.push(json!({"type": "text", "text": format!("[{label}{}]{}", img["index"], img.get("text").and_then(Value::as_str).unwrap_or(""))}));
        if img.get("data").is_some() {
            parts.push(json!({"type": "image", "mediaType": img["mimeType"], "data": img["data"], "name": format!("qq-image-{}.{}", img["index"], img["mimeType"].as_str().unwrap_or("image/jpeg").split('/').nth(1).unwrap_or("jpg"))}));
        }
    }
    parts
}
