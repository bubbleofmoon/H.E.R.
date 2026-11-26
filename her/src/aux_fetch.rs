//! Auxiliary MCP HTTP operations (`web_fetch`, `web_search` providers) over `guarded_fetch`.

use std::net::IpAddr;
use std::time::Duration;

use serde_json::{json, Value};
use url::Url;

pub(crate) use crate::guarded_fetch::{
    Flow, Head, TransferError, TransferRequest as Transfer,
};
use crate::guarded_fetch::{
    FnResolver, FnTransfer, TextFetchOptions, fetch_text_with as guarded_fetch_text_with,
    post_json_with as guarded_post_json_with, resolve_public_addresses_with,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_ADDRESS_ATTEMPTS: usize = 4;
const MAX_REDIRECTS: usize = 5;
const FETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0 Safari/537.36";

/// DNS lookup seam: the real resolver in production, a fixed table in unit tests.
pub(crate) type Resolver<'a> = dyn Fn(&str) -> Result<Vec<IpAddr>, String> + 'a;

pub(crate) fn system_resolver(host: &str) -> Result<Vec<IpAddr>, String> {
    crate::guarded_fetch::system_resolve(host, 0)
}

pub(crate) fn resolve_safe_addresses_with(
    hostname: &str,
    resolver: &Resolver,
) -> Result<Vec<IpAddr>, String> {
    let adapter = FnResolver(|host: &str, _port: u16| resolver(host));
    resolve_public_addresses_with(hostname, 0, &adapter)
}

/// The seam between the fetch logic and curl: the production transport is `curl_transfer`.
pub(crate) type TransferFn<'a> = dyn Fn(
        &Transfer,
        &mut dyn FnMut(&Head) -> Flow,
        &mut dyn FnMut(&[u8]) -> Flow,
    ) -> Result<(Head, bool), TransferError>
    + 'a;

pub(crate) fn curl_transfer(
    transfer: &Transfer,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> Result<(Head, bool), TransferError> {
    crate::guarded_fetch::run_transfer(transfer, on_head, on_body)
}

fn fetch_headers() -> Vec<(String, String)> {
    vec![
        ("user-agent".into(), FETCH_USER_AGENT.into()),
        (
            "accept".into(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".into(),
        ),
        ("accept-language".into(), "zh-CN,zh;q=0.9".into()),
    ]
}

fn text_options(max_chars: usize) -> TextFetchOptions {
    TextFetchOptions {
        max_chars,
        headers: fetch_headers(),
        hop_timeout: REQUEST_TIMEOUT,
        attempt_cap: ATTEMPT_TIMEOUT,
        max_addresses: MAX_ADDRESS_ATTEMPTS,
        max_redirects: MAX_REDIRECTS,
    }
}

/// JS `safeFetch(url, maxChars)`: the page text (first `max_chars` characters) with status and
/// truncation flag.
pub(crate) fn fetch_text_with(
    raw_url: &str,
    max_chars: usize,
    resolver: &Resolver,
    transfer_fn: &TransferFn,
) -> Result<Value, String> {
    let dns = FnResolver(|host: &str, _port: u16| resolver(host));
    let xfer = FnTransfer(
        |req: &Transfer,
         on_head: &mut dyn FnMut(&Head) -> Flow,
         on_body: &mut dyn FnMut(&[u8]) -> Flow| transfer_fn(req, on_head, on_body),
    );
    let outcome = guarded_fetch_text_with(raw_url, &text_options(max_chars), &dns, &xfer)?;
    Ok(json!({
        "url": outcome.url,
        "statusCode": outcome.status,
        "truncated": outcome.truncated,
        "body": outcome.body,
    }))
}

pub(crate) fn fetch_text(raw_url: &str, max_chars: usize) -> Result<Value, String> {
    fetch_text_with(raw_url, max_chars, &system_resolver, &curl_transfer)
}

/// One JSON POST to an endpoint whose addresses already passed the policy. Redirects are refused;
/// an HTTP status is a result (returned), network failures move on to the next address (at most two).
pub(crate) fn post_json_with(
    url: &Url,
    addresses: &[IpAddr],
    headers: Vec<(String, String)>,
    body: &str,
    timeout: Duration,
    max_bytes: usize,
    transfer_fn: &TransferFn,
) -> Result<(u16, String), String> {
    let xfer = FnTransfer(
        |req: &Transfer,
         on_head: &mut dyn FnMut(&Head) -> Flow,
         on_body: &mut dyn FnMut(&[u8]) -> Flow| transfer_fn(req, on_head, on_body),
    );
    guarded_post_json_with(url, addresses, headers, body, timeout, max_bytes, &xfer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guarded_fetch::{CURL_ENV_CLEARED, build_curl_command, is_private_ip};
    use std::cell::RefCell;

    #[test]
    fn private_address_policy_matches_the_frozen_js() {
        let oracle: Value = serde_json::from_str(include_str!("../tests/fixtures/aux-oracle.json"))
            .expect("aux oracle");
        let cases = oracle["privateIp"].as_array().unwrap();
        assert!(cases.len() > 90);
        for case in cases {
            let ip = case[0].as_str().unwrap();
            assert_eq!(
                is_private_ip(ip),
                case[1].as_bool().unwrap(),
                "ip {ip:?}"
            );
        }
    }

    fn table(host: &str) -> Result<Vec<IpAddr>, String> {
        match host {
            "public.test" => Ok(vec![
                "2606:4700::1".parse().unwrap(),
                "93.184.216.34".parse().unwrap(),
                "93.184.216.35".parse().unwrap(),
            ]),
            "rebind.test" => Ok(vec![
                "93.184.216.34".parse().unwrap(),
                "10.0.0.5".parse().unwrap(),
            ]),
            "empty.test" => Ok(vec![]),
            _ => Err("getaddrinfo ENOTFOUND".to_string()),
        }
    }

    type Script = Box<dyn Fn(&Transfer) -> Result<(u16, Option<String>, Vec<Vec<u8>>), TransferError>>;
    type RunLog = Vec<(String, String, u64)>;

    fn run(
        url: &str,
        limit: usize,
        script: Script,
    ) -> (Result<Value, String>, RunLog) {
        let log: RefCell<Vec<(String, String, u64)>> = RefCell::new(Vec::new());
        let transfer = |t: &Transfer,
                        on_head: &mut dyn FnMut(&Head) -> Flow,
                        on_body: &mut dyn FnMut(&[u8]) -> Flow| {
            log.borrow_mut()
                .push((t.url.to_string(), t.pin.to_string(), t.budget.as_secs()));
            let (status, location, chunks) = script(t)?;
            let head = Head { status, location };
            if let Flow::Stop = on_head(&head) {
                return Ok((head, false));
            }
            for chunk in chunks {
                if let Flow::Stop = on_body(&chunk) {
                    return Ok((head, false));
                }
            }
            Ok((head, true))
        };
        let result = fetch_text_with(url, limit, &table, &transfer);
        (result, log.into_inner())
    }

    fn page(status: u16, body: &str) -> Script {
        let body = body.as_bytes().to_vec();
        Box::new(move |_| Ok((status, None, vec![body.clone()])))
    }

    #[test]
    fn a_page_is_returned_with_status_url_and_untruncated_body() {
        let (result, log) = run("HTTP://Public.Test/a b?q=1#frag", 50_000, page(200, "你好 world"));
        assert_eq!(
            result.unwrap(),
            json!({"url": "http://public.test/a%20b?q=1#frag", "statusCode": 200, "truncated": false, "body": "你好 world"})
        );
        assert_eq!(
            log,
            vec![(
                "http://public.test/a%20b?q=1#frag".to_string(),
                "93.184.216.34".to_string(),
                8
            )]
        );
    }

    #[test]
    fn the_body_is_cut_at_the_character_limit_and_flagged_truncated() {
        let (result, _) = run("http://public.test/", 4, page(200, "你好世界再见"));
        assert_eq!(result.unwrap()["body"], "你好世界");
        let (result, _) = run("http://public.test/", 4, page(200, "abcd"));
        let value = result.unwrap();
        assert_eq!(
            (value["body"].as_str().unwrap(), value["truncated"].as_bool().unwrap()),
            ("abcd", true)
        );
        let (result, _) = run("http://public.test/", 5, page(200, "abcd"));
        assert_eq!(result.unwrap()["truncated"], false);
    }

    #[test]
    fn multi_byte_characters_split_across_chunks_decode_and_invalid_bytes_become_replacement_characters() {
        let bytes = "中文".as_bytes().to_vec();
        let split: Script = Box::new(move |_| {
            Ok((
                200,
                None,
                vec![bytes[..2].to_vec(), bytes[2..].to_vec(), vec![0xff, b'x']],
            ))
        });
        let (result, _) = run("http://public.test/", 100, split);
        assert_eq!(result.unwrap()["body"], "中文\u{fffd}x");
        let cut: Script = Box::new(|_| Ok((200, None, vec![vec![b'a', 0xe4, 0xb8]])));
        let (result, _) = run("http://public.test/", 100, cut);
        assert_eq!(result.unwrap()["body"], "a\u{fffd}");
    }

    #[test]
    fn non_success_statuses_are_results_not_errors() {
        let (result, _) = run("http://public.test/", 100, page(404, "gone"));
        let value = result.unwrap();
        assert_eq!(
            (value["statusCode"].as_u64(), value["body"].as_str()),
            (Some(404), Some("gone"))
        );
    }

    #[test]
    fn every_redirect_hop_is_revalidated_and_the_chain_is_bounded() {
        let to_loopback: Script =
            Box::new(|_| Ok((302, Some("http://127.0.0.1:8080/admin".into()), vec![])));
        let (result, log) = run("http://public.test/", 100, to_loopback);
        assert_eq!(result.unwrap_err(), "已拦截内网或本地地址");
        assert_eq!(log.len(), 1);
        let hops = std::cell::Cell::new(0);
        let relative: Script = Box::new(move |t| {
            hops.set(hops.get() + 1);
            if t.url.path() == "/dir/start" {
                Ok((301, Some("../next?x=1".into()), vec![]))
            } else {
                Ok((200, None, vec![b"done".to_vec()]))
            }
        });
        let (result, _) = run("http://public.test/dir/start", 100, relative);
        assert_eq!(result.unwrap()["url"], "http://public.test/next?x=1");
        let rebind: Script = Box::new(|_| Ok((307, Some("http://rebind.test/".into()), vec![])));
        assert_eq!(
            run("http://public.test/", 100, rebind).0.unwrap_err(),
            "解析结果命中私有或本地 IP，已拦截"
        );
        let missing: Script = Box::new(|_| Ok((302, None, vec![])));
        assert_eq!(
            run("http://public.test/", 100, missing).0.unwrap_err(),
            "重定向缺少 Location: 302"
        );
        let endless: Script = Box::new(|_| Ok((302, Some("/again".into()), vec![])));
        let (result, log) = run("http://public.test/", 100, endless);
        assert_eq!(result.unwrap_err(), "重定向超出次数限制，已中止");
        assert_eq!(log.len(), 6, "the first request plus five redirects");
    }

    #[test]
    fn the_url_policy_rejects_before_any_transfer() {
        for (url, why) in [
            ("ftp://public.test/", "仅允许 http/https"),
            ("not a url", "URL 无效"),
            ("http://user:pw@public.test/", "URL 不能包含凭据"),
            ("http://localhost/", "已拦截内网或本地地址"),
            ("http://printer.local/", "已拦截内网或本地地址"),
            ("http://a.localhost./", "已拦截内网或本地地址"),
            ("http://127.0.0.1/", "已拦截内网或本地地址"),
            ("http://2130706433/", "已拦截内网或本地地址"),
            ("http://0x7f.1/", "已拦截内网或本地地址"),
            ("http://[::1]/", "已拦截内网或本地地址"),
            ("http://[::ffff:10.0.0.1]/", "已拦截内网或本地地址"),
            ("http://rebind.test/", "解析结果命中私有或本地 IP，已拦截"),
            ("http://empty.test/", "DNS 未返回任何记录"),
            ("http://nx.test/", "域名解析失败：getaddrinfo ENOTFOUND"),
        ] {
            assert_eq!(
                run(url, 100, Box::new(|_| panic!("no transfer may start")))
                    .0
                    .unwrap_err(),
                why,
                "{url}"
            );
        }
    }

    #[test]
    fn network_failures_move_to_the_next_address_and_the_error_names_the_attempts() {
        let calls = std::cell::Cell::new(0);
        let flaky: Script = Box::new(move |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(TransferError::Timeout)
            } else {
                Ok((200, None, vec![b"third".to_vec()]))
            }
        });
        let (result, log) = run("http://public.test/", 100, flaky);
        assert_eq!(result.unwrap()["body"], "third");
        assert_eq!(
            log.iter().map(|l| l.1.as_str()).collect::<Vec<_>>(),
            ["93.184.216.34", "2606:4700::1", "93.184.216.35"]
        );
        let down: Script = Box::new(|_| Err(TransferError::Timeout));
        assert_eq!(
            run("http://public.test/", 100, down).0.unwrap_err(),
            "请求超时：public.test（已尝试 3 个地址）"
        );
        let reset: Script = Box::new(|_| Err(TransferError::Interrupted));
        assert_eq!(
            run("http://public.test/", 100, reset).0.unwrap_err(),
            "响应体读取中断（已尝试 3 个地址）"
        );
        let single: Script = Box::new(|_| Err(TransferError::Other("connect refused".into())));
        let (result, log) = run("http://93.184.216.34/", 100, single);
        assert_eq!(result.unwrap_err(), "connect refused");
        assert_eq!(log[0].2, 20);
    }

    #[test]
    fn aux_curl_hardening_passes_dash_q_and_clears_config_and_proxy_env() {
        let url = Url::parse("https://public.test/page").unwrap();
        let transfer = Transfer {
            url: &url,
            pin: "1.1.1.1".parse().unwrap(),
            headers: Vec::new(),
            body: None,
            budget: Duration::from_secs(5),
            connect_timeout: None,
            max_filesize: None,
        };
        let (command, _) = build_curl_command(&transfer).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.first().map(String::as_str),
            Some("-q"),
            "aux curl must put -q first: {args:?}"
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
}
