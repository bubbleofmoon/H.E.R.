//! 带 SSRF 防线的图片抓取（对应 JS 的 `safe-fetch.js` 的 `safeFetchBuffer`）。
//!
//! 为什么不能"直接把 URL 交给网关去抓"：网关会把 `http://127.0.0.1:3100/api/...`
//! 也照抓不误 —— 那就是一个能读本机控制台的 SSRF。所以图片必须在桥接内下载。
//!
//! 实现上**不引入 TLS 依赖**：her 目前没有任何 https 客户端，而 QQ 的 CDN 是 https。
//! 为了一个端点引入 rustls 会在 2 vCPU 的机器上显著拖慢构建，所以这里把 TLS 交给
//! 系统自带的 curl（Windows 为内置的 `curl.exe`，其他平台为 `curl`），**但所有安全判定都留在 Rust 里**：
//!   * 自己解析 URL、自己解析 DNS、自己判断 IP 是否公网；
//!   * 用 `--resolve host:port:ip` 把连接**钉死**在已经校验过的 IP 上（防 DNS 重绑定）；
//!   * `--max-redirs 0`，重定向自己跟着走并**每跳重新校验**；
//!   * 大小上限 + 图片魔数检查。
//!
//! 也就是说 curl 只负责"把字节搬回来"，它拿不到任何信任。

use std::net::{IpAddr, ToSocketAddrs};
static FETCH_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一次抓取的上限（默认 8 MB，与 config 的 media.maxBytes 对齐由调用方决定）。
pub struct Fetched {
    pub bytes: Vec<u8>,
    /// 最终落到的那一跳（跟着重定向走过之后）。调用方要记录来源时用它。
    #[allow(dead_code)]
    pub url: String,
}

/// 解析出来的 URL 片段。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub rest: String,
}

/// 只接受 http/https，且必须带 host。返回 (scheme, host, port, rest)。
///
/// `rest` 包含 path + query，直接拼回 URL 用。
pub fn parse_url(raw: &str) -> Option<ParsedUrl> {
    let s = raw.trim();
    let (scheme, after) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, rest) = match after.find(['/', '?', '#']) {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        // 带账号密码的 URL 不认（JS 的 url_key 也不认）
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().ok()?)
        }
        _ => (
            authority.to_string(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    if host.is_empty() {
        return None;
    }
    let rest = if rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    };
    Some(ParsedUrl {
        scheme,
        host,
        port,
        rest,
    })
}

/// 这个 IP 能不能连？只允许公网地址。
///
/// 与冻结的 JS `isPrivateIp` 逐位一致：判定直接复用 `aux_fetch::is_private_ip`（同一份移植），
/// 不再另维护一套更窄的规则。拦掉的包括 0/8、10/8、100.64/10、127/8、169.254/16、172.16/12、
/// 192.0.0/24、192.168/16、198.18/15、224.0.0.0 起的组播/保留/广播段，以及 IPv6 的 ::、::1、
/// fc00::/7、fe80::/10 与 fec0::/10 起的 fe8-ff 段、2001:db8/2001:2/2001:10/2001:20、64:ff9b:1::/48、
/// ff00::/8，还有 IPv4 映射/兼容/NAT64/6to4 内嵌的 IPv4 再按 v4 规则判一次。
pub fn ip_allowed(ip: IpAddr) -> bool {
    !crate::aux_fetch::is_private_ip(&ip.to_string())
}

/// 解析主机名并校验所有解析结果都必须是公网地址。
///
/// **要求全部通过**而不是"有一个通过就行"：DNS 返回多个地址时，
/// 只要有一个是内网，就可能被用在重试/轮询里。
pub fn resolve_public(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    // 主机名本身就是 IP 字面量时直接判
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if ip_allowed(ip) {
            Ok(vec![ip])
        } else {
            Err(format!("目标地址不允许访问：{ip}"))
        };
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let host = host.to_string();
    std::thread::spawn(move || {
        let result = (host.as_str(), port).to_socket_addrs()
            .map(|a| a.map(|a| a.ip()).collect::<Vec<_>>())
            .map_err(|_| "域名解析失败".to_string());
        let _ = tx.send(result);
    });
    let addrs = rx.recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "域名解析超时".to_string())??;
    if addrs.is_empty() {
        return Err("域名没有解析出任何地址".to_string());
    }
    for ip in &addrs {
        if !ip_allowed(*ip) {
            return Err(format!("目标解析到不允许访问的地址：{ip}"));
        }
    }
    Ok(addrs)
}

/// 图片魔数：PNG / JPEG / GIF / WebP。
pub fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// 抓一个 URL 的字节，全程带 SSRF 防线。最多跟 5 次重定向。
pub fn fetch_bytes(raw_url: &str, max_bytes: usize) -> Result<Fetched, String> {
    fetch_bytes_with(raw_url, max_bytes, resolve_public, curl_once)
}

fn fetch_bytes_with<R, F>(raw_url: &str, max_bytes: usize, mut resolve: R, mut transfer: F) -> Result<Fetched, String>
where
    R: FnMut(&str, u16) -> Result<Vec<IpAddr>, String>,
    F: FnMut(&ParsedUrl, IpAddr, usize, std::time::Duration) -> Result<(u16, Option<String>, Vec<u8>), String>,
{
    let mut current = raw_url.trim().to_string();
    for _hop in 0..=5 {
        let parsed = parse_url(&current).ok_or_else(|| format!("URL 不合法：{current}"))?;
        let ips = resolve(&parsed.host, parsed.port)?;
        let started = std::time::Instant::now();
        let mut result = Err("图片抓取失败".to_string());
        for pin in ips.iter().take(4) {
            let remaining = std::time::Duration::from_secs(20).saturating_sub(started.elapsed());
            if remaining.is_zero() { break; }
            let budget = remaining.min(std::time::Duration::from_secs(if ips.len() > 1 { 8 } else { 20 }));
            result = transfer(&parsed, *pin, max_bytes, budget);
            if result.is_ok() { break; }
        }
        let (status, location, body) = result?;
        if [301, 302, 303, 307, 308].contains(&status) {
            let loc = location.ok_or_else(|| format!("重定向缺少 Location：{status}"))?;
            current = redirect_target(&parsed, &loc)?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("图片抓取失败：HTTP {status}"));
        }
        if body.len() > max_bytes {
            return Err(format!("图片超过上限（{} 字节 > {max_bytes}）", body.len()));
        }
        if !looks_like_image(&body) {
            return Err("抓取内容不是有效图片（PNG/JPEG/GIF/WebP）".to_string());
        }
        return Ok(Fetched {
            bytes: body,
            url: current,
        });
    }
    Err("重定向次数过多，已停止".to_string())
}

fn redirect_target(base: &ParsedUrl, location: &str) -> Result<String, String> {
    let loc = location.trim().split('#').next().unwrap_or("");
    if loc.contains("://") { return Ok(loc.to_string()); }
    if loc.starts_with("//") { return Ok(format!("{}:{loc}", base.scheme)); }
    let origin = format!("{}://{}", base.scheme, host_port(base));
    let path = base.rest.split(['?', '#']).next().unwrap_or("/");
    if loc.is_empty() { return Ok(format!("{origin}{}", base.rest)); }
    if loc.starts_with('?') { return Ok(format!("{origin}{path}{loc}")); }
    let joined = if loc.starts_with('/') { loc.to_string() } else {
        format!("{}{loc}", path.rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_else(|| "/".to_string()))
    };
    let (path, query) = joined.split_once('?').map(|(p, q)| (p, format!("?{q}"))).unwrap_or((&joined, String::new()));
    let mut components = Vec::new();
    for part in path.split('/') {
        match part { ".." => { components.pop(); }, "." => {}, other => components.push(other) }
    }
    Ok(format!("{origin}/{}{query}", components.join("/").trim_start_matches('/')))
}

fn host_port(p: &ParsedUrl) -> String {
    let default = if p.scheme == "https" { 443 } else { 80 };
    if p.port == default {
        p.host.clone()
    } else {
        format!("{}:{}", p.host, p.port)
    }
}

/// The curl invocation for one pinned hop (`curl.exe` on Windows, `curl` elsewhere; no console window).
fn curl_command(p: &ParsedUrl, pin: IpAddr, max_bytes: usize, budget: std::time::Duration, out_path: &std::path::Path, hdr_path: &std::path::Path) -> Result<std::process::Command, String> {
    let url = format!("{}://{}{}", p.scheme, host_port(p), p.rest);
    let mut command = crate::subprocess::command_without_console(crate::aux_fetch::curl_program());
    command.args([
            "-sS",
            "--globoff",
            "--max-redirs",
            "0",
            "--connect-timeout",
            &format!("{:.3}", budget.as_secs_f64()),
            "--max-time",
            &format!("{:.3}", budget.as_secs_f64()),
            "--max-filesize",
            &max_bytes.to_string(),
            "--noproxy",
            "*",
            "--resolve",
            &format!("{}:{}:{}", p.host, p.port, pin),
            "-o",
            &crate::subprocess::external_path(out_path)?,
            "-D",
            &crate::subprocess::external_path(hdr_path)?,
            "-w",
            "%{http_code}",
            &url,
        ]);
    Ok(command)
}

/// 调一次 curl：把连接钉死在 `pin` 上，返回 (状态码, Location 头, 正文)。
fn curl_once(p: &ParsedUrl, pin: IpAddr, max_bytes: usize, budget: std::time::Duration) -> Result<(u16, Option<String>, Vec<u8>), String> {
    // 临时名必须带**唯一后缀**：控制台是每连接一线程，并发抓两张图时如果只用 pid，
    // 两个线程会写同一个文件，结果是一张图的内容混进另一张（A2 审计的 M5）。
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = FETCH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_path = std::env::temp_dir().join(format!("her-fetch-{}-{uniq}-{sequence}.bin", std::process::id()));
    let hdr_path = std::env::temp_dir().join(format!("her-fetch-{}-{uniq}-{sequence}.hdr", std::process::id()));
    let status = curl_command(p, pin, max_bytes, budget, &out_path, &hdr_path)?
        .output()
        .map_err(|e| format!("调用 curl 失败：{e}"))?;
    let code: u16 = String::from_utf8_lossy(&status.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    let headers = std::fs::read_to_string(&hdr_path).unwrap_or_default();
    let location = headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("location:"))
        .map(|l| l[9..].trim().to_string());
    use std::io::Read;
    let mut body = Vec::new();
    if let Ok(file) = std::fs::File::open(&out_path) {
        let _ = file.take(max_bytes as u64 + 1).read_to_end(&mut body);
    }
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&hdr_path);
    if !status.status.success() || body.len() > max_bytes {
        return Err("图片下载失败或超过大小上限".to_string());
    }
    if code == 0 && body.is_empty() {
        let err = String::from_utf8_lossy(&status.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "curl 无输出".to_string()
        } else {
            format!("下载失败：{err}")
        });
    }
    Ok((code, location, body))
}

/// 读图片尺寸（对应 JS 的 `getImageDimensions`）。认不出来返回 None。
///
/// 为什么要自己解析：表情图片的像素上限（`media.maxPixels`）是**防图片炸弹**的唯一一道闸，
/// 靠文件大小挡不住（一张 300 KB 的 PNG 解压后可以是几亿像素）。
pub fn image_dimensions(b: &[u8]) -> Option<(u64, u64)> {
    if b.len() < 24 {
        return None;
    }
    // PNG：IHDR 的宽高在固定偏移
    if b[0] == 0x89 && &b[1..4] == b"PNG" {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]) as u64;
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]) as u64;
        return Some((w, h));
    }
    // GIF：小端 16 位
    if &b[0..6] == b"GIF87a" || &b[0..6] == b"GIF89a" {
        let w = u16::from_le_bytes([b[6], b[7]]) as u64;
        let h = u16::from_le_bytes([b[8], b[9]]) as u64;
        return Some((w, h));
    }
    // JPEG：扫 SOF 段
    if b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        let mut off = 2usize;
        while off + 9 < b.len() {
            if b[off] != 0xFF {
                off += 1;
                continue;
            }
            let marker = b[off + 1];
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                off += 2;
                continue;
            }
            let len = u16::from_be_bytes([b[off + 2], b[off + 3]]) as usize;
            if len < 2 {
                return None;
            }
            // SOF0..SOF15，排除 DHT(C4)/JPG(C8)/DAC(CC)
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                let h = u16::from_be_bytes([b[off + 5], b[off + 6]]) as u64;
                let w = u16::from_be_bytes([b[off + 7], b[off + 8]]) as u64;
                return Some((w, h));
            }
            off += 2 + len;
        }
    }
    // WebP：VP8X / VP8L / VP8 三种容器
    if &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" && b.len() >= 30 {
        let fourcc = &b[12..16];
        if fourcc == b"VP8X" {
            let w = 1 + b[24] as u64 + ((b[25] as u64) << 8) + ((b[26] as u64) << 16);
            let h = 1 + b[27] as u64 + ((b[28] as u64) << 8) + ((b[29] as u64) << 16);
            return Some((w, h));
        }
        if fourcc == b"VP8L" && b.len() >= 25 {
            // 14 位宽高，各减一
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            let w = (bits & 0x3FFF) as u64 + 1;
            let h = ((bits >> 14) & 0x3FFF) as u64 + 1;
            return Some((w, h));
        }
        if fourcc == b"VP8 " && b.len() >= 30 {
            let w = u16::from_le_bytes([b[26], b[27]]) as u64 & 0x3FFF;
            let h = u16::from_le_bytes([b[28], b[29]]) as u64 & 0x3FFF;
            return Some((w, h));
        }
    }
    None
}
    #[test]
    fn image_dimensions_reads_headers() {
        // PNG 1x1
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&7u32.to_be_bytes());
        png.extend_from_slice(&9u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(image_dimensions(&png), Some((7, 9)));

        // GIF 宽 3 高 5
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&3u16.to_le_bytes());
        gif.extend_from_slice(&5u16.to_le_bytes());
        gif.extend_from_slice(&[0u8; 20]);
        assert_eq!(image_dimensions(&gif), Some((3, 5)));

        assert_eq!(image_dimensions(b"not an image at all............"), None);
    }
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_transport_uses_four_address_attempts_and_js_time_budgets() {
        let ips: Vec<IpAddr> = ["1.1.1.1", "8.8.8.8", "9.9.9.9", "8.8.4.4", "1.0.0.1"].iter().map(|s| s.parse().unwrap()).collect();
        let bytes = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let mut seen = Vec::new();
        let result = fetch_bytes_with("https://public.test/a.png", 1024,
            |_, _| Ok(ips.clone()),
            |_, ip, _, budget| {
                seen.push(ip);
                assert!(budget > std::time::Duration::ZERO && budget <= std::time::Duration::from_secs(8));
                if seen.len() < 4 { Err("transport failed".into()) } else { Ok((200, None, bytes.clone())) }
            }).unwrap();
        assert_eq!(result.bytes, bytes);
        assert_eq!(seen, ips[..4]);
        let mut attempts = 0;
        assert!(fetch_bytes_with("https://public.test/a", 1024, |_, _| Ok(ips.clone()), |_, _, _, _| {
            attempts += 1; Err("transport failed".into())
        }).is_err());
        assert_eq!(attempts, 4);
        fetch_bytes_with("https://public.test/a", 1024, |_, _| Ok(vec![ips[0]]), |_, _, _, budget| {
            assert!(budget > std::time::Duration::from_secs(19) && budget <= std::time::Duration::from_secs(20));
            Ok((200, None, bytes.clone()))
        }).unwrap();
    }

    #[test]
    fn media_redirects_revalidate_and_downloads_enforce_bytes_type_and_hops() {
        let bytes = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let mut hosts = Vec::new();
        let mut calls = 0;
        assert!(fetch_bytes_with("https://public.test/a/b",1024, |host, _| {
            hosts.push(host.to_string());
            if host == "127.0.0.1" { Err("private IP blocked".into()) } else { Ok(vec!["1.1.1.1".parse().unwrap()]) }
        }, |_, _, _, _| { calls+=1; Ok((302,Some("//127.0.0.1/private".into()),Vec::new())) }).is_err());
        assert_eq!(hosts, ["public.test", "127.0.0.1"]);
        assert_eq!(calls, 1);
        let base = parse_url("https://public.test/a/b?old=1").unwrap();
        assert_eq!(redirect_target(&base,"../image.png?x=2#fragment").unwrap(),"https://public.test/image.png?x=2");
        assert_eq!(redirect_target(&base,"?new=1").unwrap(),"https://public.test/a/b?new=1");
        let resolve=|_:&str,_|Ok(vec!["1.1.1.1".parse().unwrap()]);
        assert!(fetch_bytes_with("https://public.test/a",1,resolve,|_,_,_,_|Ok((200,None,bytes.clone()))).is_err());
        assert!(fetch_bytes_with("https://public.test/a",1024,resolve,|_,_,_,_|Ok((200,None,b"<html>bad data</html>".to_vec()))).is_err());
        let mut hops=0;
        assert!(fetch_bytes_with("https://public.test/a",1024,resolve,|_,_,_,_| { hops+=1; Ok((302,Some("/again".into()),Vec::new())) }).is_err());
        assert_eq!(hops,6);
    }

    #[test]
    fn parse_url_accepts_normal_https() {
        let p = parse_url("https://multimedia.nt.qq.com.cn/download?x=1").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.host, "multimedia.nt.qq.com.cn");
        assert_eq!(p.port, 443);
        assert_eq!(p.rest, "/download?x=1");
    }

    #[test]
    fn parse_url_rejects_bad_scheme_and_credentials() {
        assert!(parse_url("file:///etc/passwd").is_none());
        assert!(parse_url("ftp://x/y").is_none());
        assert!(parse_url("https://user:pw@a.com/x").is_none(), "带账号密码不认");
        assert!(parse_url("https:///nohost").is_none());
    }

    #[test]
    fn ip_guard_blocks_internal_addresses() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254", // 云元数据
            "0.0.0.0",
            "100.64.0.1", // CGNAT
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1", // IPv4 映射的环回
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(!ip_allowed(ip), "{bad} 应该被拦");
        }
        for ok in ["1.1.1.1", "8.8.8.8", "2400:3200::1"] {
            let ip: IpAddr = ok.parse().unwrap();
            assert!(ip_allowed(ip), "{ok} 应该放行");
        }
    }

    #[test]
    fn private_address_policy_matches_the_js_corpus() {
        // Verdicts come from the independent recorded IP policy corpus, not from this code.
        let corpus: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/safe-fetch-ip-corpus.json")).expect("corpus is JSON");
        let cases = corpus["cases"].as_array().unwrap();
        assert!(cases.len() > 9000, "corpus unexpectedly small: {}", cases.len());
        let (mut parsed, mut mismatches) = (0usize, Vec::new());
        for case in cases {
            let text = case[0].as_str().unwrap();
            let private = case[1].as_bool().unwrap();
            if crate::aux_fetch::is_private_ip(text) != private {
                mismatches.push(format!("aux_fetch::is_private_ip({text:?}) != {private}"));
            }
            if let Ok(ip) = text.parse::<IpAddr>() {
                parsed += 1;
                if ip_allowed(ip) == private {
                    mismatches.push(format!("ip_allowed({text:?}) == {}, JS private == {private}", !private));
                }
            }
        }
        assert!(parsed > 8000, "too few parseable literals: {parsed}");
        assert!(mismatches.is_empty(), "{} mismatches, first: {:#?}", mismatches.len(), &mismatches[..mismatches.len().min(20)]);
    }

    #[test]
    fn ip_guard_blocks_the_ranges_the_js_blocks() {
        for bad in ["198.18.0.1", "198.19.255.255", "240.0.0.1", "255.255.255.254", "192.0.0.9", "2001:db8::1", "2001:2::1", "ff02::1", "fec0::1", "64:ff9b:1::1", "2002:7f00:1::", "::ffff:198.18.0.1", "::"] {
            assert!(!ip_allowed(bad.parse().unwrap()), "{bad} must be blocked");
        }
        for ok in ["198.17.255.255", "198.20.0.1", "223.255.255.255", "64:ff9b::808:808", "2002:808:808::", "2606:4700::1111"] {
            assert!(ip_allowed(ok.parse().unwrap()), "{ok} must be allowed");
        }
    }

    #[test]
    fn curl_program_is_curl_exe_on_windows_and_curl_elsewhere() {
        assert_eq!(crate::aux_fetch::curl_program_for(true), "curl.exe");
        assert_eq!(crate::aux_fetch::curl_program_for(false), "curl");
        assert_eq!(crate::aux_fetch::curl_program(), if cfg!(windows) { "curl.exe" } else { "curl" });
    }

    #[test]
    fn media_curl_command_names_the_platform_curl_and_pins_the_address() {
        let p = parse_url("https://cdn.test/a.png").unwrap();
        let pin: IpAddr = "1.1.1.1".parse().unwrap();
        let command = curl_command(&p, pin, 1024, std::time::Duration::from_secs(5), std::path::Path::new("o.bin"), std::path::Path::new("h.hdr")).unwrap();
        assert_eq!(command.get_program(), std::ffi::OsStr::new(crate::aux_fetch::curl_program()));
        if cfg!(windows) {
            assert_eq!(command.get_program(), std::ffi::OsStr::new("curl.exe"));
        } else {
            assert_eq!(command.get_program(), std::ffi::OsStr::new("curl"));
        }
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let at = args.iter().position(|a| a == "--resolve").expect("--resolve pin");
        assert_eq!(args[at + 1], "cdn.test:443:1.1.1.1");
        assert!(args.windows(2).any(|w| w == ["--max-redirs", "0"]));
    }

    /// One-connection-per-request HTTP server on 127.0.0.1; `routes` maps a request path to (status, extra headers, body).
    fn fake_media_server(routes: Vec<(&'static str, u16, Vec<(String, String)>, Vec<u8>)>) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (status, headers, body) = routes.iter().find(|r| r.0 == path).map(|r| (r.1, r.2.clone(), r.3.clone())).unwrap_or((404, vec![], b"nope".to_vec()));
                let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (port, hits)
    }

    #[test]
    #[cfg(unix)]
    fn media_download_runs_through_system_curl_on_linux_against_a_started_server() {
        let png = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let (port, hits) = fake_media_server(vec![
            ("/a.png", 200, vec![], png.clone()),
            ("/go", 302, vec![("Location".into(), "/a.png".into())], Vec::new()),
            ("/big.png", 200, vec![], png.clone()),
        ]);
        // `fake.test` does not resolve anywhere: the transfer only succeeds if curl is pinned to the validated address.
        let resolve = |_: &str, _| Ok(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
        let direct = fetch_bytes_with(&format!("http://fake.test:{port}/a.png"), 1 << 20, resolve, curl_once).expect("direct download");
        assert_eq!(direct.bytes, png);
        let redirected = fetch_bytes_with(&format!("http://fake.test:{port}/go"), 1 << 20, resolve, curl_once).expect("redirected download");
        assert_eq!(redirected.bytes, png);
        assert_eq!(redirected.url, format!("http://fake.test:{port}/a.png"));
        assert!(fetch_bytes_with(&format!("http://fake.test:{port}/big.png"), png.len() - 1, resolve, curl_once).is_err(), "size cap");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[test]
    fn resolve_public_rejects_loopback_literal() {
        assert!(resolve_public("127.0.0.1", 80).is_err());
        assert!(resolve_public("localhost", 80).is_err());
    }

    #[test]
    fn image_magic_detection() {
        assert!(looks_like_image(&[0x89, b'P', b'N', b'G', 0x0d]));
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_image(b"GIF89a...."));
        assert!(looks_like_image(b"RIFF____WEBP"));
        assert!(!looks_like_image(b"<html>not an image"));
        assert!(!looks_like_image(b""));
    }
}
