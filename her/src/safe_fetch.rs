//! Guarded media, avatar, and voice byte fetches over `guarded_fetch`.
//!
//! All URL, DNS, private-IP, redirect-revalidation, time-budget, byte-cap, and curl-hardening
//! rules live in `crate::guarded_fetch`.

#[cfg(test)]
use std::net::IpAddr;
use std::time::Duration;

use crate::guarded_fetch::{
    BudgetMode, CurlTransfer, FetchOptions, FetchPolicy, SystemDnsResolver, fetch_bytes_with,
    parse_http_url_parts,
};
#[cfg(test)]
use crate::guarded_fetch::{BodyAccept, resolve_public_addresses};

pub struct Fetched {
    pub bytes: Vec<u8>,
    /// Final URL after following validated redirects.
    #[allow(dead_code)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub rest: String,
}

pub fn parse_url(raw: &str) -> Option<ParsedUrl> {
    parse_http_url_parts(raw).map(|p| ParsedUrl {
        scheme: p.scheme,
        host: p.host,
        port: p.port,
        rest: p.rest,
    })
}

#[cfg(test)]
pub fn ip_allowed(ip: IpAddr) -> bool {
    crate::guarded_fetch::ip_allowed(ip)
}

#[cfg(test)]
pub fn resolve_public(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    resolve_public_addresses(host, port)
}

fn media_options(
    max_bytes: usize,
    max_redirects: usize,
    budget: BudgetMode,
    accept: fn(&[u8]) -> bool,
    refusal: &'static str,
    #[cfg(test)] allow_loopback_pin_for_test: bool,
) -> FetchOptions {
    FetchOptions {
        policy: FetchPolicy::PublicHttpOrHttps,
        max_bytes,
        max_redirects,
        budget,
        connect_timeout: None,
        max_addresses: 4,
        min_remaining: Duration::ZERO,
        headers: Vec::new(),
        strip_fragment_on_redirect: true,
        pass_max_filesize: true,
        accept: Some((accept, refusal)),
        #[cfg(test)]
        allow_loopback_pin_for_test,
    }
}

/// Fetch an image URL with SSRF protection, following at most 5 revalidated redirects.
pub fn fetch_bytes(raw_url: &str, max_bytes: usize) -> Result<Fetched, String> {
    let options = media_options(
        max_bytes,
        5,
        BudgetMode::PerHop {
            hop_timeout: Duration::from_secs(20),
            attempt_cap: Some(Duration::from_secs(8)),
        },
        crate::media_normalize::looks_like_image,
        "抓取内容不是有效图片（PNG/JPEG/GIF/WebP）",
        #[cfg(test)]
        false,
    );
    let out = fetch_bytes_with(raw_url, &options, &SystemDnsResolver, &CurlTransfer)?;
    Ok(Fetched {
        bytes: out.bytes,
        url: out.url,
    })
}

/// One GET of a fixed URL (#206 self avatar): public pinned addresses, size cap, image magic,
/// no redirects followed, and all address attempts within `budget`.
#[cfg_attr(test, allow(dead_code))]
pub fn fetch_exact(raw_url: &str, max_bytes: usize, budget: Duration) -> Result<Fetched, String> {
    let options = media_options(
        max_bytes,
        0,
        BudgetMode::PerHop {
            hop_timeout: budget,
            attempt_cap: Some(Duration::from_secs(8)),
        },
        crate::media_normalize::looks_like_image,
        "抓取内容不是有效图片（PNG/JPEG/GIF/WebP）",
        #[cfg(test)]
        false,
    );
    let out = fetch_bytes_with(raw_url, &options, &SystemDnsResolver, &CurlTransfer)?;
    Ok(Fetched {
        bytes: out.bytes,
        url: out.url,
    })
}

/// Inbound voice record download (#170, `voice_stt`): public pinned addresses, at most 2
/// revalidated redirects, size cap, SILK v3 magic, and a single shared `budget` across all hops
/// and address attempts.
#[cfg_attr(test, allow(dead_code))]
pub fn fetch_voice(raw_url: &str, max_bytes: usize, budget: Duration) -> Result<Fetched, String> {
    let options = FetchOptions {
        policy: FetchPolicy::PublicHttpOrHttps,
        max_bytes,
        max_redirects: 2,
        budget: BudgetMode::Total {
            total_timeout: budget,
            attempt_cap: Some(Duration::from_secs(8)),
        },
        connect_timeout: None,
        max_addresses: 4,
        min_remaining: Duration::ZERO,
        headers: Vec::new(),
        strip_fragment_on_redirect: true,
        pass_max_filesize: true,
        accept: Some((crate::voice::looks_like_silk, "抓取内容不是 SILK 语音")),
        #[cfg(test)]
        allow_loopback_pin_for_test: false,
    };
    let out = fetch_bytes_with(raw_url, &options, &SystemDnsResolver, &CurlTransfer)?;
    Ok(Fetched {
        bytes: out.bytes,
        url: out.url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guarded_fetch::{
        Flow, FnResolver, FnTransfer, Head, TransferError, TransferRequest, build_curl_command,
        is_private_ip,
    };
    use std::cell::RefCell;
    use url::Url;

    fn run_with_legacy<R, F>(
        raw_url: &str,
        max_bytes: usize,
        redirects: usize,
        budget_mode: BudgetMode,
        accept: BodyAccept,
        resolve: R,
        transfer: F,
    ) -> Result<Fetched, String>
    where
        R: FnMut(&str, u16) -> Result<Vec<IpAddr>, String>,
        F: FnMut(&ParsedUrl, IpAddr, usize, Duration) -> Result<(u16, Option<String>, Vec<u8>), String>,
    {
        let resolve_cell = RefCell::new(resolve);
        let transfer_cell = RefCell::new(transfer);
        let resolver = FnResolver(|host: &str, port: u16| (resolve_cell.borrow_mut())(host, port));
        let xfer = FnTransfer(
            |req: &TransferRequest<'_>,
             on_head: &mut dyn FnMut(&Head) -> Flow,
             on_body: &mut dyn FnMut(&[u8]) -> Flow| {
                let parsed = parse_url(req.url.as_str()).expect("valid url");
                let (status, location, body) = (transfer_cell.borrow_mut())(
                    &parsed,
                    req.pin,
                    req.max_filesize.unwrap_or(max_bytes),
                    req.budget,
                )
                .map_err(TransferError::Other)?;
                let head = Head { status, location };
                if let Flow::Stop = on_head(&head) {
                    return Ok((head, false));
                }
                match on_body(&body) {
                    Flow::Continue => Ok((head, true)),
                    Flow::Stop => Ok((head, false)),
                    Flow::Abort(msg) => Err(TransferError::Other(msg)),
                }
            },
        );
        let options = media_options(max_bytes, redirects, budget_mode, accept.0, accept.1, false);
        let out = crate::guarded_fetch::fetch_bytes_with(raw_url, &options, &resolver, &xfer)?;
        Ok(Fetched {
            bytes: out.bytes,
            url: out.url,
        })
    }

    fn fetch_bytes_with<R, F>(
        raw_url: &str,
        max_bytes: usize,
        resolve: R,
        transfer: F,
    ) -> Result<Fetched, String>
    where
        R: FnMut(&str, u16) -> Result<Vec<IpAddr>, String>,
        F: FnMut(&ParsedUrl, IpAddr, usize, Duration) -> Result<(u16, Option<String>, Vec<u8>), String>,
    {
        run_with_legacy(
            raw_url,
            max_bytes,
            5,
            BudgetMode::PerHop {
                hop_timeout: Duration::from_secs(20),
                attempt_cap: Some(Duration::from_secs(8)),
            },
            (
                crate::media_normalize::looks_like_image,
                "抓取内容不是有效图片（PNG/JPEG/GIF/WebP）",
            ),
            resolve,
            transfer,
        )
    }

    fn fetch_with_hops<R, F>(
        raw_url: &str,
        max_bytes: usize,
        redirects: usize,
        budget: Duration,
        resolve: R,
        transfer: F,
    ) -> Result<Fetched, String>
    where
        R: FnMut(&str, u16) -> Result<Vec<IpAddr>, String>,
        F: FnMut(&ParsedUrl, IpAddr, usize, Duration) -> Result<(u16, Option<String>, Vec<u8>), String>,
    {
        run_with_legacy(
            raw_url,
            max_bytes,
            redirects,
            BudgetMode::PerHop {
                hop_timeout: budget,
                attempt_cap: Some(Duration::from_secs(8)),
            },
            (
                crate::media_normalize::looks_like_image,
                "抓取内容不是有效图片（PNG/JPEG/GIF/WebP）",
            ),
            resolve,
            transfer,
        )
    }

    fn fetch_voice_with<R, F>(
        raw_url: &str,
        max_bytes: usize,
        budget: Duration,
        resolve: R,
        transfer: F,
    ) -> Result<Fetched, String>
    where
        R: FnMut(&str, u16) -> Result<Vec<IpAddr>, String>,
        F: FnMut(&ParsedUrl, IpAddr, usize, Duration) -> Result<(u16, Option<String>, Vec<u8>), String>,
    {
        run_with_legacy(
            raw_url,
            max_bytes,
            2,
            BudgetMode::Total {
                total_timeout: budget,
                attempt_cap: Some(Duration::from_secs(8)),
            },
            (crate::voice::looks_like_silk, "抓取内容不是 SILK 语音"),
            resolve,
            transfer,
        )
    }

    #[test]
    fn media_transport_uses_four_address_attempts_and_js_time_budgets() {
        let ips: Vec<IpAddr> = ["1.1.1.1", "8.8.8.8", "9.9.9.9", "8.8.4.4", "1.0.0.1"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let bytes = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let mut seen = Vec::new();
        let result = fetch_bytes_with(
            "https://public.test/a.png",
            1024,
            |_, _| Ok(ips.clone()),
            |_, ip, _, budget| {
                seen.push(ip);
                assert!(budget > Duration::ZERO && budget <= Duration::from_secs(8));
                if seen.len() < 4 {
                    Err("transport failed".into())
                } else {
                    Ok((200, None, bytes.clone()))
                }
            },
        )
        .unwrap();
        assert_eq!(result.bytes, bytes);
        assert_eq!(seen, ips[..4]);
        let mut attempts = 0;
        assert!(fetch_bytes_with(
            "https://public.test/a",
            1024,
            |_, _| Ok(ips.clone()),
            |_, _, _, _| {
                attempts += 1;
                Err("transport failed".into())
            }
        )
        .is_err());
        assert_eq!(attempts, 4);
        fetch_bytes_with(
            "https://public.test/a",
            1024,
            |_, _| Ok(vec![ips[0]]),
            |_, _, _, budget| {
                assert!(
                    budget > Duration::from_secs(19) && budget <= Duration::from_secs(20)
                );
                Ok((200, None, bytes.clone()))
            },
        )
        .unwrap();
    }

    #[test]
    fn media_redirects_revalidate_and_downloads_enforce_bytes_type_and_hops() {
        let bytes = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        // IP-literal loopback redirect is rejected pre-DNS without invoking the resolver.
        let mut hosts = Vec::new();
        let mut calls = 0;
        assert!(fetch_bytes_with(
            "https://public.test/a/b",
            1024,
            |host, _| {
                hosts.push(host.to_string());
                Ok(vec!["1.1.1.1".parse().unwrap()])
            },
            |_, _, _, _| {
                calls += 1;
                Ok((302, Some("//127.0.0.1/private".into()), Vec::new()))
            }
        )
        .is_err());
        assert_eq!(hosts, ["public.test"]);
        assert_eq!(calls, 1);
        // Hostname redirect resolving to a private IP is rejected post-DNS before transfer.
        let mut rebind_hosts = Vec::new();
        let mut rebind_calls = 0;
        assert!(fetch_bytes_with(
            "https://public.test/a/b",
            1024,
            |host, _| {
                rebind_hosts.push(host.to_string());
                if host == "rebind.test" {
                    Ok(vec!["127.0.0.1".parse().unwrap()])
                } else {
                    Ok(vec!["1.1.1.1".parse().unwrap()])
                }
            },
            |_, _, _, _| {
                rebind_calls += 1;
                Ok((302, Some("//rebind.test/private".into()), Vec::new()))
            }
        )
        .is_err());
        assert_eq!(rebind_hosts, ["public.test", "rebind.test"]);
        assert_eq!(rebind_calls, 1);

        let base = Url::parse("https://public.test/a/b?old=1").unwrap();
        let mut next = base.join("../image.png?x=2#fragment").unwrap();
        next.set_fragment(None);
        assert_eq!(next.as_str(), "https://public.test/image.png?x=2");
        assert_eq!(
            base.join("?new=1").unwrap().as_str(),
            "https://public.test/a/b?new=1"
        );

        let resolve = |_: &str, _| Ok(vec!["1.1.1.1".parse().unwrap()]);
        assert!(fetch_bytes_with(
            "https://public.test/a",
            1,
            resolve,
            |_, _, _, _| Ok((200, None, bytes.clone()))
        )
        .is_err());
        assert!(fetch_bytes_with(
            "https://public.test/a",
            1024,
            resolve,
            |_, _, _, _| Ok((200, None, b"<html>bad data</html>".to_vec()))
        )
        .is_err());
        let mut hops = 0;
        assert!(fetch_bytes_with(
            "https://public.test/a",
            1024,
            resolve,
            |_, _, _, _| {
                hops += 1;
                Ok((302, Some("/again".into()), Vec::new()))
            }
        )
        .is_err());
        assert_eq!(hops, 6);
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
        assert!(parse_url("https://user:pw@a.com/x").is_none());
        assert!(parse_url("https:///nohost").is_none());
    }

    #[test]
    fn ip_guard_blocks_internal_addresses() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(!ip_allowed(ip), "{bad} must be blocked");
        }
        for ok in ["1.1.1.1", "8.8.8.8", "2400:3200::1"] {
            let ip: IpAddr = ok.parse().unwrap();
            assert!(ip_allowed(ip), "{ok} must be allowed");
        }
    }

    #[test]
    fn private_address_policy_matches_the_js_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/safe-fetch-ip-corpus.json"
        ))
        .expect("corpus is JSON");
        let cases = corpus["cases"].as_array().unwrap();
        assert!(cases.len() > 9000, "corpus unexpectedly small: {}", cases.len());
        let (mut parsed, mut mismatches) = (0usize, Vec::new());
        for case in cases {
            let text = case[0].as_str().unwrap();
            let private = case[1].as_bool().unwrap();
            if is_private_ip(text) != private {
                mismatches.push(format!("is_private_ip({text:?}) != {private}"));
            }
            if let Ok(ip) = text.parse::<IpAddr>() {
                parsed += 1;
                if ip_allowed(ip) == private {
                    mismatches.push(format!(
                        "ip_allowed({text:?}) == {}, JS private == {private}",
                        !private
                    ));
                }
            }
        }
        assert!(parsed > 8000, "too few parseable literals: {parsed}");
        assert!(
            mismatches.is_empty(),
            "{} mismatches, first: {:#?}",
            mismatches.len(),
            &mismatches[..mismatches.len().min(20)]
        );
    }

    #[test]
    fn ip_guard_blocks_the_ranges_the_js_blocks() {
        for bad in [
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "255.255.255.254",
            "192.0.0.9",
            "2001:db8::1",
            "2001:2::1",
            "ff02::1",
            "fec0::1",
            "64:ff9b:1::1",
            "2002:7f00:1::",
            "::ffff:198.18.0.1",
            "::",
        ] {
            assert!(!ip_allowed(bad.parse().unwrap()), "{bad} must be blocked");
        }
        for ok in [
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "64:ff9b::808:808",
            "2002:808:808::",
            "2606:4700::1111",
        ] {
            assert!(ip_allowed(ok.parse().unwrap()), "{ok} must be allowed");
        }
    }

    #[test]
    fn curl_program_is_curl_exe_on_windows_and_curl_elsewhere() {
        assert_eq!(crate::guarded_fetch::curl_program_for(true), "curl.exe");
        assert_eq!(crate::guarded_fetch::curl_program_for(false), "curl");
        assert_eq!(
            crate::guarded_fetch::curl_program(),
            if cfg!(windows) { "curl.exe" } else { "curl" }
        );
    }

    #[test]
    fn media_curl_command_names_the_platform_curl_and_pins_the_address() {
        let url = Url::parse("https://cdn.test/a.png").unwrap();
        let req = TransferRequest {
            url: &url,
            pin: "1.1.1.1".parse().unwrap(),
            headers: Vec::new(),
            body: None,
            budget: Duration::from_secs(5),
            connect_timeout: None,
            max_filesize: Some(1024),
        };
        let (command, _) = build_curl_command(&req).unwrap();
        assert_eq!(
            command.get_program(),
            std::ffi::OsStr::new(crate::guarded_fetch::curl_program())
        );
        if cfg!(windows) {
            assert_eq!(command.get_program(), std::ffi::OsStr::new("curl.exe"));
        } else {
            assert_eq!(command.get_program(), std::ffi::OsStr::new("curl"));
        }
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "--resolve").expect("--resolve pin");
        assert_eq!(args[at + 1], "cdn.test:443:1.1.1.1");
        assert!(args.windows(2).any(|w| w == ["--max-redirs", "0"]));
    }

    #[cfg(unix)]
    fn fake_media_server(
        routes: Vec<(&'static str, u16, Vec<(String, String)>, Vec<u8>)>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
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
                let (status, headers, body) = routes
                    .iter()
                    .find(|r| r.0 == path)
                    .map(|r| (r.1, r.2.clone(), r.3.clone()))
                    .unwrap_or((404, vec![], b"nope".to_vec()));
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
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
        let resolver = FnResolver(|_: &str, _| Ok(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]));
        let direct_opts = media_options(
            1 << 20,
            5,
            BudgetMode::PerHop {
                hop_timeout: Duration::from_secs(20),
                attempt_cap: Some(Duration::from_secs(8)),
            },
            crate::media_normalize::looks_like_image,
            true,
        );
        let direct = fetch_bytes_with(
            &format!("http://fake.test:{port}/a.png"),
            &direct_opts,
            &resolver,
            &CurlTransfer,
        )
        .expect("direct download");
        assert_eq!(direct.bytes, png);
        let redirected = fetch_bytes_with(
            &format!("http://fake.test:{port}/go"),
            &direct_opts,
            &resolver,
            &CurlTransfer,
        )
        .expect("redirected download");
        assert_eq!(redirected.bytes, png);
        assert_eq!(redirected.url, format!("http://fake.test:{port}/a.png"));
        let small_opts = media_options(
            png.len() - 1,
            5,
            BudgetMode::PerHop {
                hop_timeout: Duration::from_secs(20),
                attempt_cap: Some(Duration::from_secs(8)),
            },
            crate::media_normalize::looks_like_image,
            true,
        );
        assert!(
            fetch_bytes_with(
                &format!("http://fake.test:{port}/big.png"),
                &small_opts,
                &resolver,
                &CurlTransfer,
            )
            .is_err(),
            "size cap"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[test]
    fn self_avatar_exact_fetch_follows_no_redirect_and_keeps_its_budget() {
        let bytes = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let budget = Duration::from_secs(8);
        let (mut lookups, mut transfers) = (Vec::new(), 0);
        let refused = fetch_with_hops(
            "https://public.test/g?x=1",
            1024,
            0,
            budget,
            |host, _| {
                lookups.push(host.to_string());
                Ok(vec!["1.1.1.1".parse().unwrap()])
            },
            |_, _, _, given| {
                transfers += 1;
                assert!(given > Duration::ZERO && given <= budget, "{given:?}");
                Ok((302, Some("https://public.test/elsewhere.png".into()), Vec::new()))
            },
        );
        assert!(refused.is_err(), "a redirect is never followed");
        assert_eq!((lookups, transfers), (vec!["public.test".to_string()], 1));
        let ips: Vec<IpAddr> = ["1.1.1.1", "8.8.8.8"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let mut given_budgets = Vec::new();
        let fetched = fetch_with_hops(
            "https://public.test/g?x=1",
            1024,
            0,
            budget,
            |_, _| Ok(ips.clone()),
            |_, _, _, given| {
                given_budgets.push(given);
                if given_budgets.len() == 1 {
                    Err("first address down".into())
                } else {
                    Ok((200, None, bytes.clone()))
                }
            },
        )
        .unwrap();
        assert_eq!(fetched.bytes, bytes);
        assert_eq!(given_budgets.len(), 2);
        assert!(given_budgets.iter().all(|given| *given <= budget), "{given_budgets:?}");
        let resolve = |_: &str, _| Ok(vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
        assert!(
            fetch_with_hops(
                "https://public.test/g",
                bytes.len() - 1,
                0,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, bytes.clone()))
            )
            .is_err(),
            "one byte over the cap"
        );
        assert!(
            fetch_with_hops(
                "https://public.test/g",
                bytes.len(),
                0,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, bytes.clone()))
            )
            .is_ok(),
            "exactly the cap"
        );
        assert!(
            fetch_with_hops(
                "https://public.test/g",
                1024,
                0,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, b"<html>no</html>".to_vec()))
            )
            .is_err(),
            "not an image"
        );
        assert!(
            fetch_with_hops(
                "https://public.test/g",
                1024,
                0,
                budget,
                resolve,
                |_, _, _, _| Ok((404, None, bytes.clone()))
            )
            .is_err(),
            "not 2xx"
        );
        let mut late = 0;
        assert!(
            fetch_with_hops(
                "https://public.test/g",
                1024,
                0,
                Duration::ZERO,
                resolve,
                |_, _, _, _| {
                    late += 1;
                    Ok((200, None, bytes.clone()))
                }
            )
            .is_err(),
            "a spent budget starts no transfer"
        );
        assert_eq!(late, 0);
    }

    #[test]
    #[cfg(unix)]
    fn self_avatar_exact_fetch_through_system_curl_stops_at_a_redirect() {
        let png = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let (port, hits) = fake_media_server(vec![
            ("/g?b=qq&nk=10001&s=640", 200, vec![], png.clone()),
            (
                "/go",
                302,
                vec![("Location".into(), "/g?b=qq&nk=10001&s=640".into())],
                Vec::new(),
            ),
        ]);
        let resolver = FnResolver(|_: &str, _| Ok(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]));
        let options = media_options(
            1 << 20,
            0,
            BudgetMode::PerHop {
                hop_timeout: Duration::from_secs(8),
                attempt_cap: Some(Duration::from_secs(8)),
            },
            crate::media_normalize::looks_like_image,
            true,
        );
        let direct = fetch_bytes_with(
            &format!("http://fake.test:{port}/g?b=qq&nk=10001&s=640"),
            &options,
            &resolver,
            &CurlTransfer,
        )
        .expect("direct avatar");
        assert_eq!(direct.bytes, png);
        assert!(
            fetch_bytes_with(
                &format!("http://fake.test:{port}/go"),
                &options,
                &resolver,
                &CurlTransfer,
            )
            .is_err(),
            "the redirect is not followed"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the redirect target was never requested"
        );
    }

    #[test]
    fn self_avatar_curl_reads_no_config_and_no_proxy_environment() {
        let url = Url::parse("https://q1.qlogo.cn/g?b=qq&nk=10001&s=640").unwrap();
        let req = TransferRequest {
            url: &url,
            pin: "1.1.1.1".parse().unwrap(),
            headers: Vec::new(),
            body: None,
            budget: Duration::from_secs(5),
            connect_timeout: None,
            max_filesize: Some(1024),
        };
        let (command, _) = build_curl_command(&req).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[0], "-q",
            "curl reads its config files unless -q comes first: {args:?}"
        );
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
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for name in crate::guarded_fetch::CURL_ENV_CLEARED {
            assert_eq!(
                envs.get(&key(name)),
                Some(&Some(String::new())),
                "{name} is empty for curl"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn self_avatar_curl_ignores_a_curlrc_in_its_home() {
        let png = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        let (port, hits) = fake_media_server(vec![
            ("/g?b=qq&nk=10001&s=640", 200, vec![], png.clone()),
            ("/from-curlrc", 200, vec![], b"config".to_vec()),
        ]);
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let home = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-curlrc-{}-{sequence}",
            std::process::id()
        )));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".curlrc"),
            format!("url = \"http://127.0.0.1:{port}/from-curlrc\"\n"),
        )
        .unwrap();
        let url = Url::parse(&format!("http://fake.test:{port}/g?b=qq&nk=10001&s=640")).unwrap();
        let req = TransferRequest {
            url: &url,
            pin: "127.0.0.1".parse().unwrap(),
            headers: Vec::new(),
            body: None,
            budget: Duration::from_secs(8),
            connect_timeout: None,
            max_filesize: Some(1 << 20),
        };
        let (mut command, config) = build_curl_command(&req).unwrap();
        command.env("CURL_HOME", &home).env("HOME", &home);
        let mut child = command.spawn().unwrap();
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(config.as_bytes());
        }
        let output = child.wait_with_output().unwrap();
        let _ = std::fs::remove_dir_all(&home);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.ends_with(&png),
            "the avatar body, not the curlrc's URL"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the curlrc's extra URL was never requested"
        );
    }

    fn silk_record() -> Vec<u8> {
        let mut silk = vec![0x02];
        silk.extend_from_slice(b"#!SILK_V3");
        silk.extend_from_slice(&[3, 0, 1, 2, 3]);
        silk
    }

    #[test]
    fn voice_download_accepts_silk_only_within_the_cap_and_two_redirects() {
        let silk = silk_record();
        let budget = Duration::from_secs(8);
        let resolve = |_: &str, _| Ok(vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
        let exact = fetch_voice_with(
            "https://public.test/v",
            silk.len(),
            budget,
            resolve,
            |_, _, _, _| Ok((200, None, silk.clone())),
        );
        assert_eq!(exact.unwrap().bytes, silk, "exactly the cap");
        assert!(
            fetch_voice_with(
                "https://public.test/v",
                silk.len() - 1,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, silk.clone()))
            )
            .is_err(),
            "one byte over the cap"
        );
        let png = crate::media_normalize::fixtures::still(image::ImageFormat::Png, 2, 2, false);
        assert!(
            fetch_voice_with(
                "https://public.test/v",
                1 << 20,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, png.clone()))
            )
            .is_err(),
            "an image is not a voice record"
        );
        assert!(
            fetch_voice_with(
                "https://public.test/v",
                1 << 20,
                budget,
                resolve,
                |_, _, _, _| Ok((200, None, b"#!AMR\n\x3c".to_vec()))
            )
            .is_err(),
            "only SILK v3"
        );
        let mut hops = 0;
        assert!(fetch_voice_with(
            "https://public.test/v",
            1 << 20,
            budget,
            resolve,
            |_, _, _, _| {
                hops += 1;
                Ok((302, Some("/again".into()), Vec::new()))
            }
        )
        .is_err());
        assert_eq!(hops, 3, "the first address and at most two redirects");
        let mut hosts = Vec::new();
        assert!(
            fetch_voice_with(
                "https://public.test/v",
                1 << 20,
                budget,
                |host: &str, _| {
                    hosts.push(host.to_string());
                    if host == "rebind.test" {
                        Ok(vec!["127.0.0.1".parse::<IpAddr>().unwrap()])
                    } else {
                        Ok(vec!["1.1.1.1".parse::<IpAddr>().unwrap()])
                    }
                },
                |_, _, _, _| Ok((302, Some("https://rebind.test/v".into()), Vec::new()))
            )
            .is_err(),
            "a redirect to this host is refused"
        );
        assert_eq!(hosts, ["public.test", "rebind.test"]);
        assert!(
            fetch_voice_with(
                "https://public.test/v",
                1 << 20,
                budget,
                resolve,
                |_, _, _, _| Ok((
                    302,
                    Some("http://127.0.0.1:9000/v1/transcribe".into()),
                    Vec::new()
                ))
            )
            .is_err(),
            "a redirect to loopback IP literal is refused"
        );
    }

    #[test]
    fn voice_download_shares_one_budget_across_its_redirects() {
        let resolve = |_: &str, _| Ok(vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
        let budget = Duration::from_millis(600);
        let mut given_each = Vec::new();
        let slow = fetch_voice_with(
            "https://public.test/v",
            1 << 20,
            budget,
            resolve,
            |_, _, _, given| {
                given_each.push(given);
                std::thread::sleep(Duration::from_millis(150));
                Ok((302, Some("/again".into()), Vec::new()))
            },
        );
        assert!(slow.is_err());
        assert_eq!(given_each.len(), 3, "the first address and two redirects");
        assert!(
            given_each[0] >= Duration::from_millis(550) && given_each[0] <= budget,
            "the first transfer may use all of it: {given_each:?}"
        );
        assert!(
            given_each[1] <= Duration::from_millis(500),
            "a redirect gets what is left, not a fresh budget: {given_each:?}"
        );
        assert!(
            given_each[2] <= Duration::from_millis(350),
            "{given_each:?}"
        );
        let mut calls = 0;
        let spent = fetch_voice_with(
            "https://public.test/v",
            1 << 20,
            Duration::from_millis(300),
            resolve,
            |_, _, _, _| {
                calls += 1;
                std::thread::sleep(Duration::from_millis(200));
                Ok((302, Some("/again".into()), Vec::new()))
            },
        );
        assert!(spent.is_err());
        assert_eq!(
            calls, 2,
            "no transfer starts once the budget is spent: the third hop is refused"
        );
    }

    #[test]
    #[cfg(unix)]
    fn voice_download_through_system_curl_leaves_no_transfer_file() {
        let silk = silk_record();
        let (port, hits) = fake_media_server(vec![("/v.silk", 200, vec![], silk.clone())]);
        let resolver = FnResolver(|_: &str, _| Ok(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]));
        let options = FetchOptions {
            policy: FetchPolicy::PublicHttpOrHttps,
            max_bytes: 1 << 20,
            max_redirects: 2,
            budget: BudgetMode::Total {
                total_timeout: Duration::from_secs(8),
                attempt_cap: Some(Duration::from_secs(8)),
            },
            connect_timeout: None,
            max_addresses: 4,
            min_remaining: Duration::ZERO,
            headers: Vec::new(),
            strip_fragment_on_redirect: true,
            pass_max_filesize: true,
            accept: Some((crate::voice::looks_like_silk, "抓取内容不是 SILK 语音")),
            allow_loopback_pin_for_test: true,
        };
        let url = format!("http://fake.test:{port}/v.silk");
        let fetched = fetch_bytes_with(&url, &options, &resolver, &CurlTransfer);
        assert_eq!(fetched.expect("the voice record").bytes, silk);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Verify streaming mode executes curl cleanly without leaving her-fetch-* files in temp_dir.
        let _ = crate::guarded_fetch::run_transfer(
            &TransferRequest {
                url: &Url::parse(&url).unwrap(),
                pin: "127.0.0.1".parse().unwrap(),
                headers: Vec::new(),
                body: None,
                budget: Duration::from_secs(5),
                connect_timeout: None,
                max_filesize: None,
            },
            &mut |_| Flow::Continue,
            &mut |_| Flow::Continue,
        )
        .unwrap();
    }

    #[test]
    fn resolve_public_rejects_loopback_literal() {
        assert!(resolve_public("127.0.0.1", 80).is_err());
        assert!(resolve_public("localhost", 80).is_err());
    }

    #[test]
    fn safe_fetch_uses_stdin_config_brackets_ipv6_and_rejects_local_pre_dns() {
        let url = Url::parse("https://cdn.test/a.png?token=secret").unwrap();
        let req = TransferRequest {
            url: &url,
            pin: "2606:4700::1".parse().unwrap(),
            headers: Vec::new(),
            body: None,
            budget: Duration::from_secs(5),
            connect_timeout: None,
            max_filesize: Some(1024),
        };
        let (command, config) = build_curl_command(&req).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(|w| w == ["-K", "-"]),
            "curl must read URL via -K -: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains("token=secret")),
            "URL must not appear in CLI args: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "cdn.test:443:[2606:4700::1]"),
            "IPv6 --resolve must be bracketed: {args:?}"
        );
        assert!(config.contains("url = \"https://cdn.test/a.png?token=secret\""));
        let res = fetch_bytes_with(
            "https://printer.local/a.png",
            1024,
            |host, _| panic!("resolver must not be called for pre-DNS rejected host {host}"),
            |_, _, _, _| panic!("transfer must not start"),
        );
        assert!(res.is_err());
    }
}
