//! Frozen-JS media acquisition/fallbacks, shared by inline delivery and media tools.
use crate::media_normalize::{normalize_diagnosed, Normalized};
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

fn read_image_path_diagnosed(path: &std::path::Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "gateway_local_read_failed".to_string())?;
    let meta_len = file
        .metadata()
        .map_err(|_| "gateway_local_read_failed".to_string())?
        .len();
    if meta_len > max_bytes as u64 {
        return Err(format!(
            "local_bytes_exceeded(bytes={meta_len},max={max_bytes})"
        ));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "gateway_local_read_failed".to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "local_bytes_exceeded(bytes={},max={max_bytes})",
            bytes.len()
        ));
    }
    Ok(bytes)
}

pub fn read_image_path(path: &std::path::Path, max_bytes: usize) -> Result<Vec<u8>, ()> {
    read_image_path_diagnosed(path, max_bytes).map_err(|_| ())
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub bytes: usize,
    pub pixels: u64,
    pub count: usize,
}

impl Limits {
    pub fn from_config(cfg: &Value) -> Self {
        Self {
            bytes: crate::settings::MEDIA_BYTES.integer(cfg) as usize,
            pixels: crate::settings::MEDIA_PIXELS.integer(cfg) as u64,
            count: crate::settings::MEDIA_COUNT.integer(cfg) as usize,
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

/// Map an internal `safe_fetch` / `guarded_fetch` error string to a content-free symbolic code.
pub fn classify_fetch_error(err: &str) -> &'static str {
    let err = err.trim();
    if err.starts_with("响应体超过大小上限") {
        "fetch_body_too_large"
    } else if err.starts_with("请求超时：")
        || err.starts_with("请求超出时限")
        || err.starts_with("操作超时(")
    {
        "fetch_timeout"
    } else if err.starts_with("抓取失败：HTTP ") {
        "fetch_http_status"
    } else if err.starts_with("已拦截内网或本地地址")
        || err.starts_with("解析结果命中私有或本地 IP")
        || err == "主机名为空"
        || err == "release URL cannot target a private or local address"
        || err == "redirect to loopback address is refused"
        || err == "localhost resolved to a non-loopback address"
    {
        "fetch_blocked_network"
    } else if err.starts_with("DNS 未返回任何记录") || err.starts_with("域名解析失败：")
    {
        "fetch_dns_error"
    } else if err.starts_with("重定向超出次数限制")
        || err.starts_with("重定向缺少 Location")
        || err == "release redirect must remain HTTPS"
    {
        "fetch_redirect_refused"
    } else if err.starts_with("仅允许")
        || err == "URL 无效"
        || err == "URL 不能包含凭据"
        || err == "URL 包含非法控制字符"
        || err == "请求头包含非法控制字符"
        || err == "缺少可用的目标地址"
    {
        "fetch_invalid_url"
    } else if err.starts_with("远端响应并非合法图档") {
        "fetch_non_image_magic"
    } else if err.starts_with("调用 curl 失败") {
        "fetch_curl_exec"
    } else if err.starts_with("响应体读取中断") {
        "fetch_interrupted"
    } else if err.starts_with("请求失败：") {
        "fetch_connect_or_tls"
    } else {
        "fetch_failed"
    }
}

/// Format a standardized `[media] acquire failed:` diagnostic line for stderr logging.
pub fn format_failure_log(media_type: &str, reason: &str) -> String {
    format!("[media] acquire failed: media_type={media_type} reason={reason}")
}

fn accepted_diagnosed(bytes: Vec<u8>, limits: Limits) -> Result<(Normalized, usize), String> {
    let raw = bytes.len();
    if raw > limits.bytes {
        return Err(format!("bytes_exceeded(bytes={raw},max={})", limits.bytes));
    }
    let normalized = normalize_diagnosed(&bytes, limits.pixels)
        .map_err(|e| format!("normalize:{}", e.diagnostic()))?;
    Ok((normalized, raw))
}

fn from_url_diagnosed(url: &str, limits: Limits) -> Result<(Normalized, usize), String> {
    let fetched = crate::safe_fetch::fetch_bytes(url, limits.bytes)
        .map_err(|err| classify_fetch_error(&err).to_string())?;
    accepted_diagnosed(fetched.bytes, limits)
}

fn try_gateway_b64_fields(
    obj: &Value,
    limits: Limits,
    reasons: &mut Vec<String>,
) -> Option<(Normalized, usize)> {
    for name in ["data", "base64", "file"] {
        let Some(s) = obj.get(name).and_then(Value::as_str) else {
            continue;
        };
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        let explicit_b64 = trimmed.starts_with("base64://")
            || (trimmed.starts_with("data:image/") && trimmed.contains(','));
        // Bound encoded input before allocating the whitespace-free representation.
        if s.len() > limits.bytes.saturating_mul(4) / 3 + 4096 {
            if name != "file" || explicit_b64 {
                reasons.push(format!(
                    "gateway_b64:{name}:exceeded(len={},max_bytes={})",
                    s.len(),
                    limits.bytes
                ));
            }
            continue;
        }
        let payload = trimmed
            .strip_prefix("base64://")
            .or_else(|| {
                trimmed
                    .starts_with("data:image/")
                    .then(|| trimmed.split_once(',').map(|(_, b)| b))
                    .flatten()
            })
            .unwrap_or(trimmed);
        let compact: String = payload
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        if compact.is_empty() {
            continue;
        }
        if compact.len().saturating_mul(3) / 4 > limits.bytes {
            if name != "file" || explicit_b64 {
                reasons.push(format!(
                    "gateway_b64:{name}:exceeded(len={},max_bytes={})",
                    compact.len(),
                    limits.bytes
                ));
            }
            continue;
        }
        match STANDARD.decode(compact) {
            Ok(bytes) => match accepted_diagnosed(bytes, limits) {
                Ok(img) => return Some(img),
                Err(reason) => reasons.push(format!("gateway_b64:{name}:{reason}")),
            },
            Err(_) => {
                if name != "file" || explicit_b64 {
                    reasons.push(format!("gateway_b64:{name}:decode_failed"));
                }
            }
        }
    }
    None
}

fn from_gateway_diagnosed(
    context: &RuntimeContext,
    cfg: &Value,
    ob: &OneBot,
    media: &Value,
    limits: Limits,
) -> Result<(Normalized, usize), String> {
    let file = media
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "gateway_missing_file".to_string())?;
    if !safe_file_ref(file) {
        return Err("gateway_unsafe_file_ref".to_string());
    }
    let reply = ob
        .request_bounded(
            "get_image",
            json!({"file": file}),
            limits.bytes.saturating_mul(4) / 3 + 4096,
        )
        .ok_or_else(|| "gateway_request_failed".to_string())?;
    if reply.get("retcode").and_then(Value::as_i64).unwrap_or(0) != 0 || reply["status"] == "failed"
    {
        return Err("gateway_reply_failed".to_string());
    }
    let obj = reply
        .get("data")
        .ok_or_else(|| "gateway_missing_data".to_string())?;
    let mut reasons = Vec::new();
    if let Some(img) = try_gateway_b64_fields(obj, limits, &mut reasons) {
        return Ok(img);
    }
    if let Some(url) = obj.get("url").and_then(Value::as_str) {
        match from_url_diagnosed(url, limits) {
            Ok(img) => return Ok(img),
            Err(reason) => reasons.push(format!("gateway_url:{reason}")),
        }
    }
    let push_joined = |mut reasons: Vec<String>, tail: &str| {
        reasons.push(tail.to_string());
        Err(reasons.join(";"))
    };
    // A gateway-returned local file is trusted only inside the explicitly configured home;
    // selected roots additionally require it to be inside that root (no legacy fallback).
    let Some(raw_path) = obj.get("file").and_then(Value::as_str) else {
        return Err(if reasons.is_empty() {
            "gateway_no_source".to_string()
        } else {
            reasons.join(";")
        });
    };
    let Ok(path) = std::fs::canonicalize(raw_path) else {
        return push_joined(reasons, "gateway_local_missing");
    };
    let Some(home_dir) = cfg.pointer("/snowluma/homeDir").and_then(Value::as_str) else {
        return push_joined(reasons, "gateway_missing_home_dir");
    };
    let Ok(home) = std::fs::canonicalize(home_dir) else {
        return push_joined(reasons, "gateway_missing_home_dir");
    };
    if !path.starts_with(home) {
        return push_joined(reasons, "gateway_local_outside_home");
    }
    let Ok(runtime_root) = std::fs::canonicalize(context.runtime_root()) else {
        return push_joined(reasons, "gateway_missing_runtime_root");
    };
    if !path.starts_with(runtime_root) {
        return push_joined(reasons, "gateway_local_outside_root");
    }
    match read_image_path_diagnosed(&path, limits.bytes) {
        Ok(bytes) => match accepted_diagnosed(bytes, limits) {
            Ok(img) => Ok(img),
            Err(reason) => push_joined(reasons, &reason),
        },
        Err(reason) => push_joined(reasons, &reason),
    }
}

fn try_media_sources(
    sticker: bool,
    urls: impl FnOnce() -> Result<(Normalized, usize), String>,
    gateway: impl FnOnce() -> Result<(Normalized, usize), String>,
) -> Result<(Normalized, usize), String> {
    if sticker {
        match urls() {
            Ok(img) => Ok(img),
            Err(url_err) => match gateway() {
                Ok(img) => Ok(img),
                Err(gw_err) => Err(format!("{url_err};gateway:{gw_err}")),
            },
        }
    } else {
        match gateway() {
            Ok(img) => Ok(img),
            Err(gw_err) => match urls() {
                Ok(img) => Ok(img),
                Err(url_err) => Err(format!("gateway:{gw_err};{url_err}")),
            },
        }
    }
}

fn fetch_image_diagnosed(
    context: &RuntimeContext,
    cfg: &Value,
    ob: Option<&OneBot>,
    media: &Value,
    limits: Limits,
) -> Result<(Normalized, usize), String> {
    let id = emoji_id(media);
    let sticker = media["kind"] == "sticker"
        || !id.is_empty()
        || media
            .get("subType")
            .map(|v| v == "1" || v == 1)
            .unwrap_or(false);
    let gateway = || match ob {
        Some(ob) => from_gateway_diagnosed(context, cfg, ob, media, limits),
        None => Err("gateway_unavailable".to_string()),
    };
    let urls = || {
        let mut errors = Vec::new();
        if let Some(url) = media.get("url").and_then(Value::as_str) {
            match from_url_diagnosed(url, limits) {
                Ok(img) => return Ok(img),
                Err(reason) => errors.push(format!("url:{reason}")),
            }
        }
        if (16..=64).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_hexdigit()) {
            let candidate = format!(
                "https://gxh.vip.qq.com/club/item/parcel/item/{}/{id}/raw300.gif",
                &id[..2]
            );
            if media.get("url").and_then(Value::as_str).map(str::trim) != Some(candidate.as_str()) {
                match from_url_diagnosed(&candidate, limits) {
                    Ok(img) => return Ok(img),
                    Err(reason) => errors.push(format!("qq_sticker:{reason}")),
                }
            }
        }
        Err(if errors.is_empty() {
            "url_missing".to_string()
        } else {
            errors.join(";")
        })
    };
    try_media_sources(sticker, urls, gateway)
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
            eprintln!(
                "{}",
                format_failure_log(
                    kind,
                    &format!("batch_count_exceeded(index={},max={})", i + 1, limits.count)
                )
            );
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
            eprintln!("{}", format_failure_log(kind, "source_gate_busy"));
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
            let img = match face.as_ref().and_then(|f| f.get("url")?.as_str()) {
                Some(u) => from_url_diagnosed(u, limits),
                None => Err(if face.is_some() {
                    "face_missing_url".to_string()
                } else if id.parse::<i64>().is_err() {
                    "face_invalid_id".to_string()
                } else {
                    "face_gateway_failed".to_string()
                }),
            };
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
                fetch_image_diagnosed(context, cfg, ob.as_ref(), media, limits),
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
        match image {
            Ok((n, raw_len)) => {
                if total + raw_len > limits.bytes {
                    eprintln!(
                        "{}",
                        format_failure_log(
                            kind,
                            &format!(
                                "batch_bytes_exceeded(total={total},raw={raw_len},max={})",
                                limits.bytes
                            )
                        )
                    );
                    base["text"] = json!(format!(
                        "（{}体积超过上限，已略过）",
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
            }
            Err(reason) => {
                eprintln!("{}", format_failure_log(kind, &reason));
                base["text"] = json!(fallback);
                out.push(base);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_fetch_error_maps_safe_fetch_and_redacts_secrets() {
        assert_eq!(
            classify_fetch_error("响应体超过大小上限"),
            "fetch_body_too_large"
        );
        assert_eq!(classify_fetch_error("请求超出时限"), "fetch_timeout");
        assert_eq!(
            classify_fetch_error("请求超时：example.com"),
            "fetch_timeout"
        );
        assert_eq!(
            classify_fetch_error("请求失败：redirect-loopback.example"),
            "fetch_connect_or_tls"
        );
        assert_eq!(
            classify_fetch_error("抓取失败：HTTP 403"),
            "fetch_http_status"
        );
        assert_eq!(
            classify_fetch_error("已拦截内网或本地地址"),
            "fetch_blocked_network"
        );
        assert_eq!(
            classify_fetch_error("仅允许 http/https"),
            "fetch_invalid_url"
        );
        assert_eq!(
            classify_fetch_error("远端响应并非合法图档（仅接受 PNG、JPEG、GIF 或 WebP）"),
            "fetch_non_image_magic"
        );
        assert_eq!(
            classify_fetch_error("域名解析失败：example.com"),
            "fetch_dns_error"
        );
        assert_eq!(
            classify_fetch_error("something with https://secret.example/x"),
            "fetch_failed"
        );
        let line = format_failure_log("image", "normalize:gif_too_many_frames(frames=300,max=256)");
        assert!(line.contains("[media] acquire failed: media_type=image"));
        assert!(line.contains("reason=normalize:gif_too_many_frames(frames=300,max=256)"));
        assert!(!line.contains("http"));
    }

    #[test]
    fn try_gateway_b64_fields_keeps_bare_file_b64_and_preserves_reasons() {
        let limits = Limits {
            count: 4,
            bytes: 64 * 1024,
            pixels: 64_000_000,
        };
        let mut reasons = Vec::new();
        let png_b64 = STANDARD.encode(crate::media_normalize::fixtures::still(
            image::ImageFormat::Png,
            2,
            2,
            false,
        ));
        let ok_obj = json!({"data": "", "base64": "not-base64!", "file": png_b64});
        let (_, raw) = try_gateway_b64_fields(&ok_obj, limits, &mut reasons).unwrap();
        assert!(raw > 0);
        assert_eq!(reasons, vec!["gateway_b64:base64:decode_failed"]);

        let mut err_reasons = Vec::new();
        let bad_gif = STANDARD.encode(crate::media_normalize::fixtures::gif(257, 1, 1));
        let fail_obj = json!({"data": bad_gif, "base64": "", "file": "C:/not/a/file.gif"});
        assert!(try_gateway_b64_fields(&fail_obj, limits, &mut err_reasons).is_none());
        assert_eq!(
            err_reasons,
            vec!["gateway_b64:data:normalize:gif_too_many_frames(frames=257,max=256)"]
        );
    }

    #[test]
    fn accepted_diagnosed_reports_bytes_limit_and_normalize_reason() {
        let limits = Limits {
            count: 4,
            bytes: 10,
            pixels: 64_000_000,
        };
        let err = accepted_diagnosed(vec![0u8; 11], limits).unwrap_err();
        assert_eq!(err, "bytes_exceeded(bytes=11,max=10)");

        let err2 = accepted_diagnosed(b"<html>".to_vec(), limits).unwrap_err();
        assert_eq!(err2, "normalize:unsupported_format");
    }

    #[test]
    fn try_media_sources_and_from_url_diagnosed_redact_urls() {
        let limits = Limits {
            count: 4,
            bytes: 1024,
            pixels: 64_000_000,
        };
        let url_err = from_url_diagnosed("ftp://secret.example/private.png", limits).unwrap_err();
        assert_eq!(url_err, "fetch_invalid_url");
        assert!(!url_err.contains("secret"));

        let sticker_err = try_media_sources(
            true,
            || Err(format!("url:{url_err}")),
            || Err("gateway_unavailable".to_string()),
        )
        .unwrap_err();
        assert_eq!(
            sticker_err,
            "url:fetch_invalid_url;gateway:gateway_unavailable"
        );

        let image_err = try_media_sources(
            false,
            || Err("url:fetch_http_status".to_string()),
            || Err("gateway_reply_failed".to_string()),
        )
        .unwrap_err();
        assert_eq!(
            image_err,
            "gateway:gateway_reply_failed;url:fetch_http_status"
        );
    }
}
