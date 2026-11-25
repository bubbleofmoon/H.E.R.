//! Unified guarded HTTP fetch seam (`DnsResolver`, `HttpTransfer`, and hardened `curl` adapter).
//!
//! All trust decisions stay in Rust: URL syntax/scheme/credentials, pre-DNS `.localhost`/`.local`
//! checks, DNS resolution (with timeout), public-IP validation (matching the frozen JS `isPrivateIp`
//! policy), redirect revalidation, byte/character caps, and overall time budgets.
//!
//! The production transport runs the platform `curl` (`curl.exe` on Windows, `curl` elsewhere)
//! hidden (`command_without_console`), with `-q` as the first argument, cleared config/proxy
//! environment variables, `--noproxy *`, `--max-redirs 0`, bracketed IPv6 `--resolve` pinning,
//! `-K -` stdin configuration (so URLs, headers, and API keys never appear on the command line),
//! `ChildGuard` kill-on-drop, and Linux `PR_SET_PDEATHSIG`.

use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::process::Stdio;
use std::time::{Duration, Instant};

use url::Url;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variables that point curl at a config file (`curlrc`) or proxy; cleared on every
/// curl invocation.
pub(crate) const CURL_ENV_CLEARED: [&str; 13] = [
    "CURL_HOME",
    "XDG_CONFIG_HOME",
    "HOME",
    "APPDATA",
    "USERPROFILE",
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "no_proxy",
    "NO_PROXY",
];

/// Host and scheme admission policy for a guarded fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchPolicy {
    /// Media, voice, and auxiliary `web_fetch`: HTTP or HTTPS, no credentials, no `localhost`/
    /// `.localhost`/`.local`, and only public unicast IP addresses.
    PublicHttpOrHttps,
    /// Release feeds and signed assets (`release.rs`, SnowLuma upgrades): HTTPS to public hosts,
    /// or HTTP/HTTPS to a loopback fixture (`127.0.0.1`, `[::1]`, `localhost`) when the initial URL
    /// is loopback; no credentials or fragment. A public initial URL can never redirect to loopback,
    /// private IPs, or plain HTTP.
    ReleaseHttpsOrLoopbackFixture,
    /// Managed npm registry (`dependencies.dsh.registry`): HTTP or HTTPS without credentials.
    /// Loopback fixtures are permitted when the initial URL is loopback; non-loopback hosts must
    /// resolve exclusively to public IP addresses (no `.localhost`/`.local` or private IPs), and a
    /// non-loopback origin may never redirect to loopback or private IPs.
    RegistryHttpOrHttps,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransferError {
    Timeout,
    Interrupted,
    Other(String),
}

pub(crate) struct Head {
    pub status: u16,
    pub location: Option<String>,
}

pub(crate) enum Flow {
    Continue,
    /// Stop reading early on purpose (not an error, e.g. redirect head or character cap reached).
    Stop,
    /// Abort reading with an explicit error (e.g. response exceeds byte cap or disallowed redirect).
    Abort(String),
}

pub(crate) struct TransferRequest<'a> {
    pub url: &'a Url,
    pub pin: IpAddr,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub budget: Duration,
    pub connect_timeout: Option<Duration>,
    pub max_filesize: Option<usize>,
}

/// Hostname resolution seam: production uses `SystemDnsResolver`, tests inject fixed tables.
pub(crate) trait DnsResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// Single-hop HTTP transfer seam: production uses `CurlTransfer`, tests inject scripted doubles.
pub(crate) trait HttpTransfer {
    fn transfer(
        &self,
        request: &TransferRequest<'_>,
        on_head: &mut dyn FnMut(&Head) -> Flow,
        on_body: &mut dyn FnMut(&[u8]) -> Flow,
    ) -> Result<(Head, bool), TransferError>;
}

pub(crate) struct SystemDnsResolver;

impl DnsResolver for SystemDnsResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        system_resolve(host, port)
    }
}

pub(crate) struct CurlTransfer;

impl HttpTransfer for CurlTransfer {
    fn transfer(
        &self,
        request: &TransferRequest<'_>,
        on_head: &mut dyn FnMut(&Head) -> Flow,
        on_body: &mut dyn FnMut(&[u8]) -> Flow,
    ) -> Result<(Head, bool), TransferError> {
        run_transfer(request, on_head, on_body)
    }
}

pub(crate) struct FnResolver<F>(pub F);

impl<F> DnsResolver for FnResolver<F>
where
    F: Fn(&str, u16) -> Result<Vec<IpAddr>, String>,
{
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        (self.0)(host, port)
    }
}

pub(crate) struct FnTransfer<F>(pub F);

impl<F> HttpTransfer for FnTransfer<F>
where
    F: Fn(
        &TransferRequest<'_>,
        &mut dyn FnMut(&Head) -> Flow,
        &mut dyn FnMut(&[u8]) -> Flow,
    ) -> Result<(Head, bool), TransferError>,
{
    fn transfer(
        &self,
        request: &TransferRequest<'_>,
        on_head: &mut dyn FnMut(&Head) -> Flow,
        on_body: &mut dyn FnMut(&[u8]) -> Flow,
    ) -> Result<(Head, bool), TransferError> {
        (self.0)(request, on_head, on_body)
    }
}

// ----- address policy (JS isPrivateIp / resolveSafeAddresses) --------------------------------------------------------

fn ipv4_text(num: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        num >> 24,
        (num >> 16) & 255,
        (num >> 8) & 255,
        num & 255
    )
}

fn is_hex_group(s: &str) -> bool {
    (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_dotted(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn ipv4_from_last32(lower: &str) -> Option<String> {
    let parts: Vec<&str> = lower.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let last = parts[parts.len() - 1];
    let second = parts[parts.len() - 2];
    if is_dotted(last) {
        return Some(last.to_string());
    }
    if is_hex_group(second) && is_hex_group(last) {
        let num = (u32::from_str_radix(second, 16).ok()? << 16)
            | u32::from_str_radix(last, 16).ok()?;
        return Some(ipv4_text(num));
    }
    None
}

fn embedded_ipv4(h: &str) -> Option<String> {
    let lower = h.to_ascii_lowercase();
    if !lower.contains(':') {
        return None;
    }
    let tail_start = lower
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map_or(0, |i| i + 1);
    if is_dotted(&lower[tail_start..]) {
        return Some(lower[tail_start..].to_string());
    }
    if let Some(rest) = lower.strip_prefix("::") {
        let rest = rest.strip_prefix("ffff:").unwrap_or(rest);
        if let Some((a, b)) = rest.split_once(':') {
            if is_hex_group(a) && is_hex_group(b) {
                let num =
                    (u32::from_str_radix(a, 16).ok()? << 16) | u32::from_str_radix(b, 16).ok()?;
                return Some(ipv4_text(num));
            }
        }
        if let Some(embedded) = ipv4_from_last32(&lower) {
            return Some(embedded);
        }
    }
    if lower.starts_with("64:ff9b") {
        if let Some(embedded) = ipv4_from_last32(&lower) {
            return Some(embedded);
        }
    }
    if let Some(rest) = lower.strip_prefix("64:ff9b:") {
        let rest = rest.strip_prefix("::").unwrap_or(rest);
        if is_dotted(rest) {
            return Some(rest.to_string());
        }
        if let Some((a, b)) = rest.split_once(':') {
            if is_hex_group(a) && is_hex_group(b) {
                let num =
                    (u32::from_str_radix(a, 16).ok()? << 16) | u32::from_str_radix(b, 16).ok()?;
                return Some(ipv4_text(num));
            }
        }
    }
    None
}

fn canonical_ipv6(text: &str) -> Option<String> {
    let url = Url::parse(&format!("http://[{text}]/")).ok()?;
    let host = url.host_str()?;
    Some(
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    )
}

/// JS `isPrivateIp`: anything that is not a valid public unicast literal is "private".
pub fn is_private_ip(ip: &str) -> bool {
    let lowered = ip.to_ascii_lowercase();
    let stripped = lowered.strip_prefix('[').unwrap_or(&lowered);
    let h = stripped.strip_suffix(']').unwrap_or(stripped);
    if h.contains('%') {
        return true;
    }
    if h.parse::<std::net::Ipv4Addr>().is_ok() {
        let p: Vec<u32> = h.split('.').map(|s| s.parse().unwrap_or(0)).collect();
        return p[0] == 10
            || p[0] == 127
            || p[0] == 0
            || (p[0] == 169 && p[1] == 254)
            || (p[0] == 172 && (16..=31).contains(&p[1]))
            || (p[0] == 192 && p[1] == 168)
            || (p[0] == 100 && (64..=127).contains(&p[1]))
            || (p[0] == 198 && (18..=19).contains(&p[1]))
            || (p[0] == 192 && p[1] == 0 && p[2] == 0)
            || p[0] >= 224;
    }
    if h.parse::<std::net::Ipv6Addr>().is_err() {
        return true;
    }
    let Some(h) = canonical_ipv6(h) else {
        return true;
    };
    let h = h.as_str();
    if h == "::" || h == "::1" || h.starts_with("fc") || h.starts_with("fd") {
        return true;
    }
    let b = h.as_bytes();
    if b.len() >= 3
        && b[0] == b'f'
        && b[1] == b'e'
        && matches!(b[2], b'8' | b'9' | b'a' | b'b' | b'c' | b'd' | b'e' | b'f')
    {
        return true;
    }
    if h.starts_with("64:ff9b:1:") {
        return true;
    }
    if let Some(embedded) = embedded_ipv4(h) {
        return is_private_ip(&embedded);
    }
    if h.starts_with("2001:db8")
        || h.starts_with("2001:2:")
        || h.starts_with("2001:10:")
        || h.starts_with("2001:20:")
    {
        return true;
    }
    if let Some(rest) = h.strip_prefix("2002:") {
        let mut groups = rest.splitn(3, ':');
        if let (Some(a), Some(b), Some(_)) = (groups.next(), groups.next(), groups.next()) {
            if is_hex_group(a) && is_hex_group(b) {
                let num = (u32::from_str_radix(a, 16).unwrap_or(0) << 16)
                    | u32::from_str_radix(b, 16).unwrap_or(0);
                if is_private_ip(&ipv4_text(num)) {
                    return true;
                }
            }
        }
    }
    h.starts_with("ff")
}

pub fn ip_allowed(ip: IpAddr) -> bool {
    !is_private_ip(&ip.to_string())
}

/// IPv4 first, the two families interleaved.
pub(crate) fn order_addresses(addresses: &[IpAddr]) -> Vec<IpAddr> {
    let v4: Vec<IpAddr> = addresses.iter().copied().filter(IpAddr::is_ipv4).collect();
    let v6: Vec<IpAddr> = addresses.iter().copied().filter(IpAddr::is_ipv6).collect();
    let mut out = Vec::new();
    for i in 0..v4.len().max(v6.len()) {
        out.extend(v4.get(i));
        out.extend(v6.get(i));
    }
    out
}

pub(crate) fn system_resolve(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let host_owned = host.to_string();
    let lookup_host = host_owned.clone();
    std::thread::spawn(move || {
        let result = (lookup_host.as_str(), port)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|a| a.ip()).collect::<Vec<_>>())
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    match rx.recv_timeout(DNS_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(format!("操作超时(5000ms)：DNS 解析 {host_owned}")),
    }
}

fn normalize_host(hostname: &str) -> String {
    let lowered = hostname.to_ascii_lowercase();
    let h = lowered.strip_prefix('[').unwrap_or(&lowered);
    let h = h.strip_suffix(']').unwrap_or(h);
    h.strip_suffix('.').unwrap_or(h).to_string()
}

fn is_local_name(h: &str) -> bool {
    h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local")
}

fn is_loopback_host_str(h: &str) -> bool {
    h == "localhost" || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

pub(crate) fn resolve_public_addresses_with(
    hostname: &str,
    port: u16,
    resolver: &dyn DnsResolver,
) -> Result<Vec<IpAddr>, String> {
    let h = normalize_host(hostname);
    if h.is_empty() {
        return Err("主机名为空".to_string());
    }
    if is_local_name(&h) {
        return Err("禁止访问内网/本机地址".to_string());
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        if !ip_allowed(ip) {
            return Err("禁止访问内网/本机地址".to_string());
        }
        return Ok(vec![ip]);
    }
    let addresses = resolver
        .resolve(&h, port)
        .map_err(|e| format!("域名解析失败：{e}"))?;
    if addresses.is_empty() {
        return Err("域名没有解析结果".to_string());
    }
    if addresses.iter().any(|a| !ip_allowed(*a)) {
        return Err("域名解析到内网/本机地址，已阻止".to_string());
    }
    let ordered = order_addresses(&addresses);
    Ok(if ordered.is_empty() {
        vec![addresses[0]]
    } else {
        ordered
    })
}

#[cfg(test)]
pub(crate) fn resolve_public_addresses(hostname: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    resolve_public_addresses_with(hostname, port, &SystemDnsResolver)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHttpUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub rest: String,
}

fn has_empty_authority(trimmed: &str) -> bool {
    trimmed
        .split_once("://")
        .is_some_and(|(_, after)| after.starts_with('/'))
}

/// Parse an HTTP(S) URL without credentials and with a non-empty host.
pub(crate) fn parse_http_url_parts(raw: &str) -> Option<ParsedHttpUrl> {
    let trimmed = crate::aux_text::js_trim(raw);
    if has_empty_authority(trimmed) {
        return None;
    }
    let url = Url::parse(trimmed).ok()?;
    let scheme = url.scheme().to_string();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str().filter(|h| !h.is_empty())?.to_string();
    let port = url.port_or_known_default()?;
    let rest = url[url::Position::BeforePath..].to_string();
    Some(ParsedHttpUrl {
        scheme,
        host,
        port,
        rest,
    })
}

/// Validate URL syntax and static host/IP rules (pre-DNS) according to `policy`.
/// Used by configuration validation (`release::validate_url`, `release::validate_registry_url`)
/// as well as before every network transfer.
pub(crate) fn validate_url_syntax(raw: &str, policy: FetchPolicy) -> Result<Url, String> {
    let trimmed = crate::aux_text::js_trim(raw);
    if has_empty_authority(trimmed) {
        return Err(match policy {
            FetchPolicy::ReleaseHttpsOrLoopbackFixture => "release URL is invalid".to_string(),
            FetchPolicy::RegistryHttpOrHttps => "dependencies.dsh.registry is invalid".to_string(),
            FetchPolicy::PublicHttpOrHttps => "URL 无效".to_string(),
        });
    }
    let url = Url::parse(trimmed).map_err(|_| match policy {
        FetchPolicy::ReleaseHttpsOrLoopbackFixture => "release URL is invalid".to_string(),
        FetchPolicy::RegistryHttpOrHttps => "dependencies.dsh.registry is invalid".to_string(),
        FetchPolicy::PublicHttpOrHttps => "URL 无效".to_string(),
    })?;
    match policy {
        FetchPolicy::PublicHttpOrHttps => {
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err("仅允许 http/https".to_string());
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err("URL 不能包含凭据".to_string());
            }
            let host = url
                .host_str()
                .filter(|h| !h.is_empty())
                .ok_or_else(|| "主机名为空".to_string())?;
            let h = normalize_host(host);
            if is_local_name(&h) || (h.parse::<IpAddr>().is_ok() && is_private_ip(&h)) {
                return Err("禁止访问内网/本机地址".to_string());
            }
        }
        FetchPolicy::ReleaseHttpsOrLoopbackFixture => {
            let host_raw = url.host_str().filter(|h| !h.is_empty());
            let loopback = host_raw.is_some_and(|h| is_loopback_host_str(&normalize_host(h)));
            if !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            {
                return Err(
                    "release URL requires HTTPS or a loopback HTTP fixture, without credentials or fragment"
                        .to_string(),
                );
            }
            let h = normalize_host(host_raw.unwrap_or(""));
            if !loopback && (is_local_name(&h) || (h.parse::<IpAddr>().is_ok() && is_private_ip(&h))) {
                return Err("release URL cannot target a private or local address".to_string());
            }
        }
        FetchPolicy::RegistryHttpOrHttps => {
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(
                    "dependencies.dsh.registry must be an http(s) URL without credentials"
                        .to_string(),
                );
            }
            let host = url
                .host_str()
                .filter(|h| !h.is_empty())
                .ok_or_else(|| "dependencies.dsh.registry is invalid".to_string())?;
            let h = normalize_host(host);
            let loopback = is_loopback_host_str(&h);
            if !loopback && (is_local_name(&h) || (h.parse::<IpAddr>().is_ok() && is_private_ip(&h))) {
                return Err(
                    "dependencies.dsh.registry cannot target a private or local address"
                        .to_string(),
                );
            }
        }
    }
    Ok(url)
}

pub(crate) struct Validated {
    pub url: Url,
    pub addresses: Vec<IpAddr>,
    pub is_loopback: bool,
}

pub(crate) fn validate_and_resolve_with(
    raw: &str,
    policy: FetchPolicy,
    initial_is_loopback: Option<bool>,
    resolver: &dyn DnsResolver,
    #[cfg(test)] allow_loopback_pin_for_test: bool,
) -> Result<Validated, String> {
    let url = validate_url_syntax(raw, policy)?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "主机名为空".to_string())?;
    let port = url.port_or_known_default().unwrap_or(80);
    let h = normalize_host(host);
    let is_loopback = is_loopback_host_str(&h);

    if matches!(
        policy,
        FetchPolicy::ReleaseHttpsOrLoopbackFixture | FetchPolicy::RegistryHttpOrHttps
    ) && is_loopback
    {
        if initial_is_loopback == Some(false) {
            return Err("redirect to loopback address is refused".to_string());
        }
        let addresses = if let Ok(ip) = h.parse::<IpAddr>() {
            vec![ip]
        } else {
            // Host is literal "localhost": resolve and require every address to be loopback.
            let resolved = resolver
                .resolve(&h, port)
                .map_err(|e| format!("域名解析失败：{e}"))?;
            if resolved.is_empty() {
                return Err("域名没有解析结果".to_string());
            }
            if resolved.iter().any(|ip| !ip.is_loopback()) {
                return Err("localhost resolved to a non-loopback address".to_string());
            }
            order_addresses(&resolved)
        };
        return Ok(Validated {
            url,
            addresses,
            is_loopback: true,
        });
    }

    // For non-loopback targets (and all PublicHttpOrHttps targets):
    if initial_is_loopback == Some(false)
        && policy == FetchPolicy::ReleaseHttpsOrLoopbackFixture
        && url.scheme() != "https"
    {
        return Err("release redirect must remain HTTPS".to_string());
    }

    #[cfg(test)]
    if allow_loopback_pin_for_test {
        let resolved = resolver
            .resolve(&h, port)
            .map_err(|e| format!("域名解析失败：{e}"))?;
        if resolved.is_empty() {
            return Err("域名没有解析结果".to_string());
        }
        return Ok(Validated {
            url,
            addresses: order_addresses(&resolved),
            is_loopback: false,
        });
    }

    let addresses = resolve_public_addresses_with(host, port, resolver)?;
    Ok(Validated {
        url,
        addresses,
        is_loopback: false,
    })
}

pub(crate) fn validate_fetch_url_with(
    raw: &str,
    resolver: &dyn DnsResolver,
) -> Result<Validated, String> {
    validate_and_resolve_with(
        raw,
        FetchPolicy::PublicHttpOrHttps,
        None,
        resolver,
        #[cfg(test)]
        false,
    )
}

// ----- hardened curl transport ---------------------------------------------------------------------------------------

/// The system curl: `curl.exe` on Windows, `curl` elsewhere.
pub(crate) fn curl_program() -> &'static str {
    curl_program_for(cfg!(windows))
}

pub(crate) fn curl_program_for(windows: bool) -> &'static str {
    if windows {
        "curl.exe"
    } else {
        "curl"
    }
}

fn config_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0B}' => out.push_str("\\v"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn has_header_ctl(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// Build the hardened curl `Command` and its `-K -` stdin configuration payload.
/// `-q` is placed first and config/proxy env vars are cleared so no external `curlrc` or proxy
/// setting can override pinned resolution.
pub(crate) fn build_curl_command(
    transfer: &TransferRequest<'_>,
) -> Result<(std::process::Command, String), TransferError> {
    if has_header_ctl(transfer.url.as_str()) {
        return Err(TransferError::Other("URL 包含非法控制字符".to_string()));
    }
    for (name, value) in &transfer.headers {
        if has_header_ctl(name) || has_header_ctl(value) {
            return Err(TransferError::Other("请求头包含非法控制字符".to_string()));
        }
    }

    let max_secs = format!("{:.3}", transfer.budget.as_secs_f64().max(0.001));
    let connect_budget = transfer
        .connect_timeout
        .map_or(transfer.budget, |cap| transfer.budget.min(cap));
    let connect_secs = format!("{:.3}", connect_budget.as_secs_f64().max(0.001));
    let proto = if transfer.url.scheme() == "https" {
        "=https"
    } else {
        "=http"
    };

    let mut command = crate::subprocess::command_without_console(curl_program());
    for name in CURL_ENV_CLEARED {
        command.env(name, "");
    }
    command.args([
        "-q",
        "-sS",
        "-i",
        "--globoff",
        "--max-redirs",
        "0",
        "--proto",
        proto,
        "--noproxy",
        "*",
        "--connect-timeout",
        &connect_secs,
        "--max-time",
        &max_secs,
    ]);
    if let Some(max_filesize) = transfer.max_filesize {
        command.args(["--max-filesize", &max_filesize.to_string()]);
    }
    if let Some(host) = transfer.url.host_str() {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if bare.parse::<IpAddr>().is_err() {
            let port = transfer.url.port_or_known_default().unwrap_or(80);
            let pin = match transfer.pin {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            };
            command.args(["--resolve", &format!("{host}:{port}:{pin}")]);
        }
    }
    command
        .args(["-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut config = format!("url = {}\n", config_quote(transfer.url.as_str()));
    for (name, value) in &transfer.headers {
        config.push_str(&format!(
            "header = {}\n",
            config_quote(&format!("{name}: {value}"))
        ));
    }
    if let Some(body) = &transfer.body {
        config.push_str(&format!("data-binary = {}\n", config_quote(body)));
    }
    Ok((command, config))
}

fn spawn_curl(transfer: &TransferRequest<'_>) -> Result<ChildGuard, TransferError> {
    let (mut command, config) = build_curl_command(transfer)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe prctl between fork and exec; the child dies with this parent.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| TransferError::Other(format!("调用 curl 失败：{e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    Ok(ChildGuard(child))
}

fn parse_head(raw: &[u8]) -> Option<Head> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let location = lines.find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("location")
            .then(|| value.trim().to_string())
    });
    Some(Head { status, location })
}

enum ReadEnd {
    Stopped(Head),
    Aborted(String),
    Eof(Option<Head>),
}

fn read_response(
    stdout: &mut impl Read,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> ReadEnd {
    let mut pending: Vec<u8> = Vec::new();
    let mut head: Option<Head> = None;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => return ReadEnd::Eof(head),
            Ok(n) => n,
        };
        if head.is_some() {
            match on_body(&chunk[..n]) {
                Flow::Continue => continue,
                Flow::Stop => return ReadEnd::Stopped(head.take().expect("head present")),
                Flow::Abort(message) => return ReadEnd::Aborted(message),
            }
        }
        pending.extend_from_slice(&chunk[..n]);
        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let Some(parsed) = parse_head(&pending[..end]) else {
                return ReadEnd::Aborted("响应头无法解析".to_string());
            };
            let rest = pending.split_off(end + 4);
            pending.clear();
            if (100..200).contains(&parsed.status) && parsed.status != 101 {
                pending = rest;
                continue;
            }
            match on_head(&parsed) {
                Flow::Continue => {}
                Flow::Stop => return ReadEnd::Stopped(parsed),
                Flow::Abort(message) => return ReadEnd::Aborted(message),
            }
            if !rest.is_empty() {
                match on_body(&rest) {
                    Flow::Continue => {}
                    Flow::Stop => return ReadEnd::Stopped(parsed),
                    Flow::Abort(message) => return ReadEnd::Aborted(message),
                }
            }
            head = Some(parsed);
            break;
        }
        if head.is_none() && pending.len() > MAX_HEADER_BYTES {
            return ReadEnd::Aborted("响应头过大".to_string());
        }
    }
}

pub(crate) fn run_transfer(
    transfer: &TransferRequest<'_>,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> Result<(Head, bool), TransferError> {
    let mut child = spawn_curl(transfer)?;
    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| TransferError::Other("curl 无输出".to_string()))?;
    let mut stderr = child.0.stderr.take();
    let diagnostics = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = Read::take(s, 8192).read_to_string(&mut text);
        }
        text
    });
    let outcome = read_response(&mut stdout, on_head, on_body);
    if !matches!(outcome, ReadEnd::Eof(_)) {
        let _ = child.0.kill();
    }
    drop(stdout);
    let code = child.0.wait().ok().and_then(|s| s.code());
    let diagnostics = diagnostics.join().unwrap_or_default();
    match outcome {
        ReadEnd::Stopped(head) => Ok((head, false)),
        ReadEnd::Aborted(message) => Err(TransferError::Other(message)),
        ReadEnd::Eof(Some(head)) if code == Some(0) => Ok((head, true)),
        ReadEnd::Eof(_) if code == Some(28) => Err(TransferError::Timeout),
        ReadEnd::Eof(Some(_)) if code == Some(63) => {
            Err(TransferError::Other("响应体超过大小上限".to_string()))
        }
        ReadEnd::Eof(Some(_)) => Err(TransferError::Interrupted),
        ReadEnd::Eof(None) => {
            if code == Some(63) {
                return Err(TransferError::Other("响应体超过大小上限".to_string()));
            }
            let text = diagnostics.trim().trim_start_matches("curl: ");
            Err(TransferError::Other(if text.is_empty() {
                "curl 无输出".to_string()
            } else {
                text.to_string()
            }))
        }
    }
}

// ----- guarded fetch orchestration -----------------------------------------------------------------------------------

pub(crate) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Decode UTF-8 the way Node's `StringDecoder` does: invalid bytes become U+FFFD; an incomplete tail
/// is held back until more bytes arrive (`complete` flushes it as one U+FFFD).
pub(crate) fn decode_utf8(bytes: &[u8], complete: bool) -> String {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                return out;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or(""));
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        rest = &after[n..];
                    }
                    None => {
                        if complete {
                            out.push('\u{FFFD}');
                        }
                        return out;
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BudgetMode {
    PerHop {
        hop_timeout: Duration,
        attempt_cap: Option<Duration>,
    },
    Total {
        total_timeout: Duration,
        attempt_cap: Option<Duration>,
    },
}

pub(crate) type BodyAccept = (fn(&[u8]) -> bool, &'static str);

pub(crate) struct FetchOptions {
    pub policy: FetchPolicy,
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub budget: BudgetMode,
    pub connect_timeout: Option<Duration>,
    pub max_addresses: usize,
    pub min_remaining: Duration,
    pub headers: Vec<(String, String)>,
    pub strip_fragment_on_redirect: bool,
    pub pass_max_filesize: bool,
    pub accept: Option<BodyAccept>,
    #[cfg(test)]
    pub allow_loopback_pin_for_test: bool,
}

pub(crate) struct TextFetchOptions {
    pub max_chars: usize,
    pub headers: Vec<(String, String)>,
    pub hop_timeout: Duration,
    pub attempt_cap: Duration,
    pub max_addresses: usize,
    pub max_redirects: usize,
}

pub(crate) struct BytesOutcome {
    pub bytes: Vec<u8>,
    pub url: String,
}

pub(crate) struct TextOutcome {
    pub url: String,
    pub status: u16,
    pub body: String,
    pub truncated: bool,
}

fn attempt_budget(
    mode: BudgetMode,
    hop_deadline: Instant,
    attempts_total: usize,
    min_remaining: Duration,
) -> Option<Duration> {
    let now = Instant::now();
    let remaining = hop_deadline.saturating_duration_since(now);
    if remaining <= min_remaining || remaining.is_zero() {
        return None;
    }
    let attempt_cap = match mode {
        BudgetMode::PerHop { attempt_cap, .. } | BudgetMode::Total { attempt_cap, .. } => {
            attempt_cap
        }
    };
    let budget = match attempt_cap {
        Some(cap) if attempts_total > 1 => remaining.min(cap),
        _ => match mode {
            BudgetMode::PerHop { hop_timeout, .. } => remaining.min(hop_timeout),
            BudgetMode::Total { .. } => remaining,
        },
    };
    (!budget.is_zero()).then_some(budget)
}

fn transfer_error_message(error: &TransferError, host: &str) -> String {
    match error {
        TransferError::Timeout => format!("请求超时：{host}"),
        TransferError::Interrupted => "响应体读取中断".to_string(),
        TransferError::Other(message) => message.clone(),
    }
}

pub(crate) fn fetch_bytes_with(
    raw_url: &str,
    options: &FetchOptions,
    resolver: &dyn DnsResolver,
    transfer: &dyn HttpTransfer,
) -> Result<BytesOutcome, String> {
    enum BytesHopOutcome {
        Response(u16, Vec<u8>),
        Redirect(u16, String),
    }

    let overall_start = Instant::now();
    let mut current = raw_url.trim().to_string();
    let mut initial_is_loopback: Option<bool> = None;

    for hop in 0..=options.max_redirects {
        let hop_deadline = match options.budget {
            BudgetMode::Total { total_timeout, .. } => {
                if total_timeout.saturating_sub(overall_start.elapsed()).is_zero() {
                    return Err("请求超出时限".to_string());
                }
                overall_start + total_timeout
            }
            BudgetMode::PerHop { hop_timeout, .. } => Instant::now() + hop_timeout,
        };
        let validated = validate_and_resolve_with(
            &current,
            options.policy,
            initial_is_loopback,
            resolver,
            #[cfg(test)]
            options.allow_loopback_pin_for_test,
        )?;
        if initial_is_loopback.is_none() {
            initial_is_loopback = Some(validated.is_loopback);
        }
        let host = validated
            .url
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let attempts = validated.addresses.len().min(options.max_addresses);
        let mut last_error: Option<String> = None;
        let mut hop_result: Option<Result<BytesHopOutcome, String>> = None;

        for pin in validated.addresses.iter().take(attempts) {
            let Some(budget) = attempt_budget(
                options.budget,
                hop_deadline,
                validated.addresses.len(),
                options.min_remaining,
            ) else {
                break;
            };
            let req = TransferRequest {
                url: &validated.url,
                pin: *pin,
                headers: options.headers.clone(),
                body: None,
                budget,
                connect_timeout: options.connect_timeout,
                max_filesize: options.pass_max_filesize.then_some(options.max_bytes),
            };
            let mut redirect: Option<(u16, String)> = None;
            let mut body = Vec::new();
            let mut aborted_by_policy = false;
            let mut on_head = |head: &Head| {
                if is_redirect(head.status) {
                    redirect = Some((head.status, head.location.clone().unwrap_or_default()));
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            };
            let mut on_body = |chunk: &[u8]| {
                if body.len().saturating_add(chunk.len()) > options.max_bytes {
                    aborted_by_policy = true;
                    return Flow::Abort(format!("响应体超过大小上限（> {} 字节）", options.max_bytes));
                }
                body.extend_from_slice(chunk);
                Flow::Continue
            };
            match transfer.transfer(&req, &mut on_head, &mut on_body) {
                Ok((head, complete)) => {
                    if let Some((status, loc)) = redirect {
                        hop_result = Some(Ok(BytesHopOutcome::Redirect(status, loc)));
                    } else if !complete {
                        last_error = Some("响应体读取中断".to_string());
                        continue;
                    } else {
                        hop_result = Some(Ok(BytesHopOutcome::Response(head.status, body)));
                    }
                    break;
                }
                Err(error) => {
                    let msg = transfer_error_message(&error, &host);
                    if aborted_by_policy {
                        hop_result = Some(Err(msg));
                        break;
                    }
                    last_error = Some(msg);
                }
            }
        }

        let (status, body) = match hop_result {
            Some(Ok(BytesHopOutcome::Redirect(status, loc))) => {
                if hop == options.max_redirects {
                    return Err("重定向次数过多，已停止".to_string());
                }
                if loc.is_empty() {
                    return Err(format!("重定向缺少 Location：{status}"));
                }
                let mut next = validated
                    .url
                    .join(&loc)
                    .map_err(|_| "URL 无效".to_string())?;
                if options.strip_fragment_on_redirect {
                    next.set_fragment(None);
                }
                current = next.to_string();
                continue;
            }
            Some(Ok(BytesHopOutcome::Response(status, body))) => (status, body),
            Some(Err(err)) => return Err(err),
            None => {
                let mut err = last_error.unwrap_or_else(|| format!("请求失败：{host}"));
                if attempts > 1 && !err.is_empty() {
                    err = format!("{err}（已尝试 {attempts} 个地址）");
                }
                return Err(err);
            }
        };

        if !(200..300).contains(&status) {
            return Err(format!("抓取失败：HTTP {status}"));
        }
        if body.len() > options.max_bytes {
            return Err(format!(
                "响应体超过大小上限（{} 字节 > {}）",
                body.len(),
                options.max_bytes
            ));
        }
        if let Some((accept_fn, refusal)) = options.accept {
            if !accept_fn(&body) {
                return Err(refusal.to_string());
            }
        }
        return Ok(BytesOutcome {
            bytes: body,
            url: current,
        });
    }
    Err("重定向次数过多，已停止".to_string())
}

pub(crate) fn fetch_text_with(
    raw_url: &str,
    options: &TextFetchOptions,
    resolver: &dyn DnsResolver,
    transfer: &dyn HttpTransfer,
) -> Result<TextOutcome, String> {
    enum HopOutcome {
        Page(TextOutcome),
        Redirect(u16, String),
    }

    let mut current = validate_fetch_url_with(raw_url, resolver)?;
    for hop in 0..=options.max_redirects {
        let host = current
            .url
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let hop_deadline = Instant::now() + options.hop_timeout;
        let attempts = current.addresses.len().min(options.max_addresses);
        let mut last_error: Option<String> = None;
        let mut outcome: Option<Result<HopOutcome, String>> = None;

        for pin in current.addresses.iter().take(attempts) {
            let remaining = hop_deadline.saturating_duration_since(Instant::now());
            if remaining <= Duration::from_millis(250) {
                break;
            }
            let budget = if current.addresses.len() == 1 {
                options.hop_timeout
            } else {
                options.attempt_cap.min(remaining)
            };
            let req = TransferRequest {
                url: &current.url,
                pin: *pin,
                headers: options.headers.clone(),
                body: None,
                budget,
                connect_timeout: None,
                max_filesize: None,
            };
            let mut redirect: Option<(u16, String)> = None;
            let mut bytes: Vec<u8> = Vec::new();
            let mut on_head = |head: &Head| {
                if is_redirect(head.status) {
                    redirect = Some((head.status, head.location.clone().unwrap_or_default()));
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            };
            let mut on_body = |chunk: &[u8]| {
                bytes.extend_from_slice(chunk);
                if bytes.len() >= options.max_chars
                    && decode_utf8(&bytes, false).chars().count() >= options.max_chars
                {
                    return Flow::Stop;
                }
                Flow::Continue
            };
            match transfer.transfer(&req, &mut on_head, &mut on_body) {
                Ok((head, complete)) => {
                    if let Some((status, location)) = redirect {
                        outcome = Some(Ok(HopOutcome::Redirect(status, location)));
                    } else {
                        let decoded = decode_utf8(&bytes, complete);
                        let truncated = decoded.chars().count() >= options.max_chars;
                        let body: String = decoded.chars().take(options.max_chars).collect();
                        outcome = Some(Ok(HopOutcome::Page(TextOutcome {
                            url: current.url.as_str().to_string(),
                            status: head.status,
                            body,
                            truncated,
                        })));
                    }
                    break;
                }
                Err(error) => last_error = Some(transfer_error_message(&error, &host)),
            }
        }

        match outcome {
            Some(Ok(HopOutcome::Page(page))) => return Ok(page),
            Some(Ok(HopOutcome::Redirect(status, location))) => {
                if hop == options.max_redirects {
                    return Err("重定向次数过多，已停止".to_string());
                }
                if location.is_empty() {
                    return Err(format!("重定向缺少 Location: {status}"));
                }
                let next = current
                    .url
                    .join(&location)
                    .map_err(|_| "URL 无效".to_string())?;
                current = validate_fetch_url_with(next.as_str(), resolver)?;
            }
            Some(Err(err)) => return Err(err),
            None => {
                let mut error = last_error.unwrap_or_else(|| format!("请求失败：{host}"));
                if attempts > 1 && !error.is_empty() {
                    error = format!("{error}（已尝试 {attempts} 个地址）");
                }
                return Err(error);
            }
        }
    }
    Err("重定向次数过多，已停止".to_string())
}

pub(crate) fn post_json_with(
    url: &Url,
    addresses: &[IpAddr],
    headers: Vec<(String, String)>,
    body: &str,
    timeout: Duration,
    max_bytes: usize,
    transfer: &dyn HttpTransfer,
) -> Result<(u16, String), String> {
    let deadline = Instant::now() + timeout;
    let attempts = addresses.len().min(2);
    if attempts == 0 {
        return Err("没有任何可用地址".to_string());
    }
    let mut last_error: Option<String> = None;
    for pin in addresses.iter().take(attempts) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= Duration::from_millis(500) {
            break;
        }
        let budget = if attempts == 1 {
            timeout
        } else {
            remaining.min(timeout.div_f64(2.0))
        };
        let req = TransferRequest {
            url,
            pin: *pin,
            headers: headers.clone(),
            body: Some(body.to_string()),
            budget,
            connect_timeout: None,
            max_filesize: Some(max_bytes),
        };
        let mut bytes: Vec<u8> = Vec::new();
        let aborted_by_policy = std::cell::Cell::new(false);
        let mut on_head = |head: &Head| {
            if (300..400).contains(&head.status) {
                aborted_by_policy.set(true);
                Flow::Abort(format!("联网搜索端点返回重定向（HTTP {}），已拒绝跟随", head.status))
            } else {
                Flow::Continue
            }
        };
        let mut on_body = |chunk: &[u8]| {
            bytes.extend_from_slice(chunk);
            if bytes.len() > max_bytes {
                aborted_by_policy.set(true);
                Flow::Abort("联网搜索响应体过大，已中止".to_string())
            } else {
                Flow::Continue
            }
        };
        match transfer.transfer(&req, &mut on_head, &mut on_body) {
            Ok((head, _)) => return Ok((head.status, String::from_utf8_lossy(&bytes).into_owned())),
            Err(TransferError::Timeout) => {
                last_error = Some(format!(
                    "联网搜索请求超时（{}s）",
                    budget.as_secs_f64().round() as u64
                ));
            }
            Err(TransferError::Interrupted) => last_error = Some("联网搜索响应中断".to_string()),
            Err(TransferError::Other(message)) => {
                if aborted_by_policy.get() {
                    return Err(message);
                }
                last_error = Some(message);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "联网搜索请求失败".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptedResolver;
    impl DnsResolver for ScriptedResolver {
        fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
            match host {
                "public.test" => Ok(vec!["93.184.216.34".parse().unwrap()]),
                "rebind.test" => Ok(vec!["10.0.0.5".parse().unwrap()]),
                "localhost" => Ok(vec!["127.0.0.1".parse().unwrap()]),
                "printer.local" | "sub.localhost" => {
                    panic!("pre-DNS rejection failed for {host}")
                }
                _ => Err("not found".into()),
            }
        }
    }

    struct ScriptedTransfer {
        status: u16,
        location: Option<String>,
        body: Vec<u8>,
        fail_timeout: bool,
    }
    impl HttpTransfer for ScriptedTransfer {
        fn transfer(
            &self,
            _request: &TransferRequest<'_>,
            on_head: &mut dyn FnMut(&Head) -> Flow,
            on_body: &mut dyn FnMut(&[u8]) -> Flow,
        ) -> Result<(Head, bool), TransferError> {
            if self.fail_timeout {
                return Err(TransferError::Timeout);
            }
            let head = Head {
                status: self.status,
                location: self.location.clone(),
            };
            if let Flow::Stop = on_head(&head) {
                return Ok((head, false));
            }
            for chunk in self.body.chunks(2) {
                match on_body(chunk) {
                    Flow::Continue => {}
                    Flow::Stop => return Ok((head, false)),
                    Flow::Abort(msg) => return Err(TransferError::Other(msg)),
                }
            }
            Ok((head, true))
        }
    }

    fn public_options(max_bytes: usize, max_redirects: usize, budget: Duration) -> FetchOptions {
        FetchOptions {
            policy: FetchPolicy::PublicHttpOrHttps,
            max_bytes,
            max_redirects,
            budget: BudgetMode::PerHop {
                hop_timeout: budget,
                attempt_cap: Some(Duration::from_secs(8)),
            },
            connect_timeout: None,
            max_addresses: 4,
            min_remaining: Duration::ZERO,
            headers: Vec::new(),
            strip_fragment_on_redirect: true,
            pass_max_filesize: true,
            accept: None,
            allow_loopback_pin_for_test: false,
        }
    }

    #[test]
    fn guarded_fetch_enforces_size_cap() {
        let opts = public_options(4, 0, Duration::from_secs(5));
        let exact = ScriptedTransfer {
            status: 200,
            location: None,
            body: b"1234".to_vec(),
            fail_timeout: false,
        };
        assert_eq!(
            fetch_bytes_with("https://public.test/a", &opts, &ScriptedResolver, &exact)
                .unwrap()
                .bytes,
            b"1234"
        );
        let over = ScriptedTransfer {
            status: 200,
            location: None,
            body: b"12345".to_vec(),
            fail_timeout: false,
        };
        assert!(
            fetch_bytes_with("https://public.test/a", &opts, &ScriptedResolver, &over).is_err(),
            "oversized body must be rejected by size cap guard"
        );
    }

    #[test]
    fn guarded_fetch_enforces_timeout_budget() {
        let opts = public_options(1024, 0, Duration::ZERO);
        let ok = ScriptedTransfer {
            status: 200,
            location: None,
            body: b"ok".to_vec(),
            fail_timeout: false,
        };
        assert!(
            fetch_bytes_with("https://public.test/a", &opts, &ScriptedResolver, &ok).is_err(),
            "zero/expired timeout budget must fail before starting transfer"
        );
        let timed_out = ScriptedTransfer {
            status: 200,
            location: None,
            body: Vec::new(),
            fail_timeout: true,
        };
        let err = fetch_bytes_with(
            "https://public.test/a",
            &public_options(1024, 0, Duration::from_secs(5)),
            &ScriptedResolver,
            &timed_out,
        )
        .err()
        .unwrap();
        assert!(err.contains("超时"), "unexpected timeout message: {err}");
    }

    #[test]
    fn guarded_fetch_refuses_redirect_to_private_host() {
        let opts = public_options(1024, 5, Duration::from_secs(5));
        for target in [
            "http://127.0.0.1:3100/api/settings",
            "https://rebind.test/secret",
            "https://printer.local/secret",
        ] {
            let redir = ScriptedTransfer {
                status: 302,
                location: Some(target.into()),
                body: Vec::new(),
                fail_timeout: false,
            };
            assert!(
                fetch_bytes_with("https://public.test/a", &opts, &ScriptedResolver, &redir)
                    .is_err(),
                "redirect to {target} must be refused"
            );
        }
        // Public release origin must never downgrade or redirect to a loopback fixture.
        let release_opts = FetchOptions {
            policy: FetchPolicy::ReleaseHttpsOrLoopbackFixture,
            ..public_options(1024, 5, Duration::from_secs(5))
        };
        for target in [
            "http://127.0.0.1:3100/api/settings",
            "https://127.0.0.1:3100/api/settings",
            "http://public.test/downgrade",
        ] {
            let redir = ScriptedTransfer {
                status: 302,
                location: Some(target.into()),
                body: Vec::new(),
                fail_timeout: false,
            };
            assert!(
                fetch_bytes_with(
                    "https://public.test/feed",
                    &release_opts,
                    &ScriptedResolver,
                    &redir
                )
                .is_err(),
                "public release origin must refuse redirect to {target}"
            );
        }
    }

    #[test]
    fn guarded_fetch_refuses_private_and_local_hosts_pre_dns() {
        let opts = public_options(1024, 0, Duration::from_secs(5));
        let ok = ScriptedTransfer {
            status: 200,
            location: None,
            body: b"ok".to_vec(),
            fail_timeout: false,
        };
        for url in [
            "https://printer.local/a",
            "https://printer.local./a",
            "https://sub.localhost/a",
            "https://localhost/a",
            "http://169.254.169.254/latest/meta-data",
            "https://[::1]/a",
            "https://rebind.test/a",
        ] {
            assert!(
                fetch_bytes_with(url, &opts, &ScriptedResolver, &ok).is_err(),
                "host in {url} must be rejected"
            );
        }
    }

    #[test]
    fn curl_command_hardens_all_callers_with_q_cleared_env_stdin_and_bracketed_ipv6() {
        let url = Url::parse("https://cdn.test/a.png?token=secret").unwrap();
        let req = TransferRequest {
            url: &url,
            pin: "2606:4700::1".parse().unwrap(),
            headers: vec![("x-api-key".into(), "secret-key-value".into())],
            body: Some("{\"q\":1}".into()),
            budget: Duration::from_secs(5),
            connect_timeout: None,
            max_filesize: Some(1024),
        };
        let (command, config) = build_curl_command(&req).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("-q"), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--max-redirs", "0"]));
        assert!(args.windows(2).any(|w| w == ["--noproxy", "*"]));
        assert!(args.windows(2).any(|w| w == ["-K", "-"]));
        assert!(args.iter().any(|a| a == "cdn.test:443:[2606:4700::1]"), "{args:?}");
        assert!(
            !args
                .iter()
                .any(|a| a.contains("secret") || a.contains("cdn.test/a.png")),
            "URL and secrets must stay in stdin config, never CLI args: {args:?}"
        );
        assert!(config.contains("url = \"https://cdn.test/a.png?token=secret\""));
        assert!(config.contains("header = \"x-api-key: secret-key-value\""));
        assert!(config.contains("data-binary = \"{\\\"q\\\":1}\""));

        let key = |name: &str| {
            if cfg!(windows) {
                name.to_ascii_uppercase()
            } else {
                name.to_string()
            }
        };
        let envs: std::collections::HashMap<String, Option<String>> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    key(&name.to_string_lossy()),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for name in CURL_ENV_CLEARED {
            assert_eq!(
                envs.get(&key(name)),
                Some(&Some(String::new())),
                "{name} must be cleared"
            );
        }
    }

    #[test]
    fn read_response_rejects_oversized_headers() {
        let huge = vec![b'X'; MAX_HEADER_BYTES + 32];
        let mut cursor = std::io::Cursor::new(huge);
        let end = read_response(&mut cursor, &mut |_| Flow::Continue, &mut |_| Flow::Continue);
        assert!(matches!(end, ReadEnd::Aborted(_)));
    }
}
