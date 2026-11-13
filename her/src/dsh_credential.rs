//! DSH credential: the login cookie the bridge mints, refreshes and checks (ticket 10).
//!
//! Format (DSH `isAuthenticated`): `dsh-auth-<b64url(sha256(authority))>=v1.<b64url(payload)>.<b64url(hmac)>`, payload
//! `{version:1, authority, issuedAt, expiresAt}`, HMAC-SHA256 with DSH's persistent browser-session secret
//! (`.credentials.yaml` of the DSH home). DSH requires `issuedAt <= now < expiresAt` with no clock-skew tolerance.
//!
//! * Minting back-dates `issuedAt` by 60 s and extends `expiresAt` by the same amount, so the remaining lifetime is
//!   unchanged (one hour).
//! * A cookie DSH rejects (HTTP 401) is refreshed: the credentials are re-read and a fresh cookie is minted.
//! * [`check_login`] runs one request with a fresh cookie and, after a rejection, once more with a refreshed one;
//!   [`LoginWatch`] classifies two rejections of fresh cookies in a row as a failing login (health check part
//!   `dshLogin`, one administrator alert per streak).

use crate::runtime::{RuntimeArea, RuntimeContext};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const COOKIE_PREFIX: &str = "dsh-auth-";
const COOKIE_PAYLOAD_VERSION: i64 = 1;
const COOKIE_LIFETIME_MS: u128 = 60 * 60 * 1000;
const COOKIE_BACKDATE_MS: u128 = 60 * 1000;
pub const SECRET_BYTES: usize = 32;

/// Why DSH rejects freshly minted cookies, as the health check and the alert name it.
pub const REJECTED_TWICE_REASON: &str = "DSH rejected two freshly minted login cookies in a row; likely causes: clock skew \
between this host and DSH (DSH refuses an issuedAt in the future), a DSH version change in its login rules, or a replaced signing secret";
/// The administrator alert for the same streak.
pub const REJECTED_TWICE_ALERT: &str = "DSH 连续两次拒绝了刚铸造的登录 Cookie，DSH 登录不可用。可能原因：本机与 DSH 的时钟偏差（clock skew，\
DSH 不接受未来的 issuedAt），或 DSH 版本变更改了登录规则（DSH version change）；也请检查签名密钥是否被替换。";

fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn now_ms() -> u128 {
    crate::parity_clock::now_ms() as u128
}

/// 从 baseUrl 取 authority（host:port）。
fn authority_of(base_url: &str) -> Option<String> {
    let rest = base_url.split("://").nth(1)?;
    let host_port = rest.split('/').next()?;
    if host_port.is_empty() {
        None
    } else {
        Some(host_port.to_string())
    }
}

/// 读 DSH 的浏览器会话签名密钥（32 字节）。
///
/// 不引入 YAML 依赖：`.credentials.yaml` 里 `secret:` 出现次数很少，
/// 逐个取出来试着 base64url 解码，正好 32 字节的就是它。
pub fn read_session_secret() -> Option<Vec<u8>> {
    // No DSH home configured means no DSH secret: never fall back to a relative path.
    let path = crate::runtime::dsh_home()?.join(".credentials.yaml");
    let text = std::fs::read_to_string(path).ok()?;
    parse_session_secret(&text)
}

pub fn session_secret_from_text(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(idx) = line.find("secret:") else {
            continue;
        };
        let raw = line[idx + "secret:".len()..]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        if raw.is_empty()
            || !raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        if let Ok(bytes) = URL_SAFE_NO_PAD.decode(raw) {
            if bytes.len() == SECRET_BYTES {
                return Some(raw.to_string());
            }
        }
    }
    None
}

fn parse_session_secret(text: &str) -> Option<Vec<u8>> {
    let encoded = session_secret_from_text(text)?;
    URL_SAFE_NO_PAD.decode(encoded).ok()
}

/// Read the DSH signing secret from the selected runtime's user area.
pub fn read_session_secret_for_runtime(context: &RuntimeContext) -> Option<Vec<u8>> {
    if context.is_isolated() {
        return parse_session_secret(
            &context
                .read_text(RuntimeArea::DshUser, ".credentials.yaml")
                .ok()?,
        );
    }
    read_session_secret()
}

/// 铸造会话 Cookie（返回完整 `name=value`）。
pub fn mint_cookie(base_url: &str, secret: &[u8]) -> Option<String> {
    mint_cookie_at(base_url, secret, now_ms())
}

/// Mint for the bridge clock reading `now`.
fn mint_cookie_at(base_url: &str, secret: &[u8], now: u128) -> Option<String> {
    let authority = authority_of(base_url)?;
    // DSH rejects `issuedAt > Date.now()` with no clock-skew tolerance, and over
    // loopback our high-resolution clock can be a few ms ahead of Node's. Back-date
    // issuance and extend expiry by the same amount so the remaining lifetime is unchanged.
    let issued_at = now.saturating_sub(COOKIE_BACKDATE_MS);
    let expires_at = issued_at + COOKIE_LIFETIME_MS + COOKIE_BACKDATE_MS;
    let payload = serde_json::json!({
        "version": COOKIE_PAYLOAD_VERSION,
        "authority": authority,
        "issuedAt": issued_at,
        "expiresAt": expires_at,
    });
    let body = b64url(payload.to_string().as_bytes());

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(body.as_bytes());
    let signature = b64url(&mac.finalize().into_bytes());

    let name = format!(
        "{COOKIE_PREFIX}{}",
        b64url(&Sha256::digest(authority.as_bytes()))
    );
    Some(format!("{name}=v1.{body}.{signature}"))
}

pub fn session_cookie_for_runtime(context: &RuntimeContext, base_url: &str) -> Option<String> {
    let secret = read_session_secret_for_runtime(context)?;
    mint_cookie(base_url, &secret)
}

fn unavailable(context: &RuntimeContext, ambient: &str) -> String {
    if context.is_isolated() {
        "selected runtime DSH credentials are unavailable or invalid".to_string()
    } else {
        ambient.to_string()
    }
}

/// The login in force for one DSH endpoint of one runtime. A rejection refreshes it from the same runtime's
/// credentials (never the ambient DSH home for an isolated runtime), so it is shared state behind `&self`.
pub struct Credential {
    base: String,
    context: RuntimeContext,
    cookie: Mutex<String>,
    /// One refresh after a rejection (the frozen JS `_doFetchWithAuth`). The 401 drill (`--selftest-401`) turns it
    /// off to observe the raw refusal.
    refresh: AtomicBool,
}

impl Credential {
    fn with_cookie(base: &str, cookie: String, context: RuntimeContext) -> Self {
        Self {
            base: base.to_string(),
            context,
            cookie: Mutex::new(cookie),
            refresh: AtomicBool::new(true),
        }
    }

    /// Read the runtime's credentials and mint a fresh cookie for `base`.
    pub fn mint(context: &RuntimeContext, base: &str) -> Result<Self, String> {
        let cookie = session_cookie_for_runtime(context, base)
            .ok_or_else(|| unavailable(context, "读不到签名密钥（~/.dsh/.credentials.yaml）"))?;
        Ok(Self::with_cookie(base, cookie, context.clone()))
    }

    /// For tests that already hold a credentials-file secret (ambient refresh).
    #[cfg(test)]
    pub fn from_secret(base: &str, secret: &str) -> Result<Self, String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(secret.trim())
            .ok()
            .filter(|bytes| bytes.len() == SECRET_BYTES)
            .ok_or_else(|| "DSH session secret is invalid".to_string())?;
        let cookie = mint_cookie(base, &bytes)
            .ok_or_else(|| "DSH session cookie could not be created".to_string())?;
        Ok(Self::with_cookie(base, cookie, RuntimeContext::legacy()))
    }

    /// A synthetic cookie that is never refreshed (transport tests).
    #[cfg(test)]
    pub fn fixed(base: &str, cookie: String) -> Self {
        let credential = Self::with_cookie(base, cookie, RuntimeContext::legacy());
        credential.disable_refresh();
        credential
    }

    /// The runtime whose credentials this login uses.
    pub fn context(&self) -> &RuntimeContext {
        &self.context
    }

    /// The cookie currently in force (after any refresh).
    pub fn cookie(&self) -> String {
        self.cookie
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_cookie(&self, cookie: String) {
        *self.cookie.lock().unwrap_or_else(|p| p.into_inner()) = cookie;
    }

    /// Turn the refresh after a rejection off (only the `--selftest-401` drill does).
    pub fn disable_refresh(&self) {
        self.refresh.store(false, Ordering::SeqCst);
    }

    /// DSH rejected the cookie in force: re-read the credentials and mint a fresh one. `None` when refresh is off or
    /// the credentials cannot be read.
    pub fn refresh_after_rejection(&self) -> Option<String> {
        if !self.refresh.load(Ordering::SeqCst) {
            return None;
        }
        let fresh = session_cookie_for_runtime(&self.context, &self.base)?;
        self.set_cookie(fresh.clone());
        Some(fresh)
    }

    /// 重新读密钥并铸一个新 cookie（原地替换），并当场用 `verify` 验一次。
    ///
    /// 为什么要它：cookie 是**本地铸造**的，有效期 1 小时；secret 被替换（凭据文件改写）后，手里这个 cookie
    /// 立刻全部 401。重读密钥并重铸即可恢复，无需重启。为什么不能只铸不验：实测有一次铸完仍然 401——DSH 换密钥时，
    /// 文件先变、进程内存后变（或反过来），中间那几百毫秒里刚铸的票也是 401。所以 4 次、递增间隔。
    pub fn relogin(&self, mut verify: impl FnMut() -> Result<(), String>) -> Result<(), String> {
        let mut last = String::new();
        for attempt in 0..4 {
            let cookie =
                session_cookie_for_runtime(&self.context, &self.base).ok_or_else(|| {
                    unavailable(&self.context, "重新读密钥失败（~/.dsh/.credentials.yaml）")
                })?;
            self.set_cookie(cookie);
            match verify() {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = e;
                    let wait = [200u64, 500, 1000, 0][attempt.min(3)];
                    if wait > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(wait));
                    }
                }
            }
        }
        Err(format!("重铸后仍 401：{last}"))
    }
}

/// The error text of a request DSH refused for its cookie (HTTP 401).
pub fn is_rejection(error: &str) -> bool {
    error.starts_with("401 ")
}

/// What one login check proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// DSH answered a request carrying a fresh cookie with something other than 401.
    Accepted,
    /// DSH rejected this many fresh cookies in a row (a refresh that could not be minted ends the check at one).
    Rejected { in_a_row: u32 },
    /// DSH gave no answer, so the login is unknown.
    Unchecked,
}

impl Login {
    /// Two rejections in a row: the health check fails the DSH login part.
    pub fn failing(&self) -> bool {
        matches!(self, Self::Rejected { in_a_row } if *in_a_row >= LoginWatch::LIMIT)
    }

    fn of<T>(result: &Result<T, String>) -> Self {
        match result {
            Ok(_) => Self::Accepted,
            Err(error) if is_rejection(error) => Self::Rejected { in_a_row: 1 },
            // DSH answered and refused the method itself: the cookie was accepted.
            Err(error) if error.contains(" 被拒：") => Self::Accepted,
            Err(_) => Self::Unchecked,
        }
    }
}

pub struct LoginCheck<T> {
    /// The last attempt's result.
    pub result: Result<T, String>,
    pub login: Login,
}

/// How long a rejected check waits before it re-reads the secret and repeats: a secret rotation in progress must not
/// count as two rejections at once.
pub const REPEAT_PAUSE: Duration = Duration::from_millis(300);

/// The pause that fits the probe `deadline` (none: the whole pause).
pub fn repeat_pause(deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(deadline) => REPEAT_PAUSE.min(deadline.saturating_duration_since(Instant::now())),
        None => REPEAT_PAUSE,
    }
}

/// Run `attempt` (one request with `credential`'s cookie, minted just before by the caller) and, when DSH rejects the
/// cookie, wait `pause`, refresh it from the re-read credentials and run `attempt` exactly once more.
pub fn check_login<T>(
    credential: &Credential,
    pause: Duration,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> LoginCheck<T> {
    let first = attempt();
    let login = Login::of(&first);
    if login != (Login::Rejected { in_a_row: 1 }) {
        return LoginCheck {
            result: first,
            login,
        };
    }
    if !pause.is_zero() {
        std::thread::sleep(pause);
    }
    if credential.refresh_after_rejection().is_none() {
        return LoginCheck {
            result: first,
            login,
        };
    }
    let second = attempt();
    let login = match Login::of(&second) {
        Login::Rejected { .. } => Login::Rejected { in_a_row: 2 },
        other => other,
    };
    LoginCheck {
        result: second,
        login,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginEdge {
    Steady,
    /// The streak just reached two rejected fresh cookies: alert once.
    Failing,
    /// A fresh cookie was accepted after a failing streak.
    Recovered,
}

/// An alert the notice budget refused is offered again at most this often (probes run every few seconds and each
/// refused offer is logged).
pub const ALERT_OFFER_INTERVAL_MS: i64 = 60_000;

/// Consecutive rejections of fresh cookies across checks; an acceptance ends the streak, no answer leaves it alone.
/// A failing streak owes one alert, which stays owed until the caller reports it delivered (the notice budget may
/// refuse it) or the streak ends; it is offered at once on the failing edge, then at most once per
/// [`ALERT_OFFER_INTERVAL_MS`].
#[derive(Debug, Default)]
pub struct LoginWatch {
    in_a_row: u32,
    alert_owed: bool,
    last_offer_ms: Option<i64>,
}

impl LoginWatch {
    pub const LIMIT: u32 = 2;

    pub fn observe(&mut self, login: &Login) -> LoginEdge {
        let was = self.failing();
        match login {
            Login::Accepted => self.in_a_row = 0,
            Login::Rejected { in_a_row } => self.in_a_row = self.in_a_row.saturating_add(*in_a_row),
            Login::Unchecked => {}
        }
        match (was, self.failing()) {
            (false, true) => {
                self.alert_owed = true;
                self.last_offer_ms = None;
                LoginEdge::Failing
            }
            (true, false) => {
                self.alert_owed = false;
                self.last_offer_ms = None;
                LoginEdge::Recovered
            }
            _ => LoginEdge::Steady,
        }
    }

    pub fn failing(&self) -> bool {
        self.in_a_row >= Self::LIMIT
    }

    /// The streak's alert has not been accepted for sending yet.
    pub fn alert_owed(&self) -> bool {
        self.alert_owed
    }

    /// Whether to offer the owed alert now (`now_ms` from the caller's clock); a true answer starts the next
    /// interval. A clock that stepped back counts as due.
    pub fn alert_due(&mut self, now_ms: i64) -> bool {
        let due = self.alert_owed
            && self
                .last_offer_ms
                .is_none_or(|last| now_ms < last || now_ms - last >= ALERT_OFFER_INTERVAL_MS);
        if due {
            self.last_offer_ms = Some(now_ms);
        }
        due
    }

    pub fn alert_delivered(&mut self) {
        self.alert_owed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeContext;

    /// DSH's own cookie rules (dsh-client-connection `isAuthenticated`): the name binds the authority, the value is
    /// `v1.<payload>.<HMAC-SHA256 of payload>`, the payload names the same authority, and
    /// `issuedAt <= now < expiresAt` with a lifetime of at most 30 days. There is no clock-skew tolerance.
    fn dsh_verify(
        cookie: &str,
        authority: &str,
        secret: &[u8],
        dsh_now_ms: u128,
    ) -> Result<(), &'static str> {
        let (name, value) = cookie.split_once('=').ok_or("no name")?;
        if name
            != format!(
                "dsh-auth-{}",
                URL_SAFE_NO_PAD.encode(Sha256::digest(authority.as_bytes()))
            )
        {
            return Err("cookie name does not bind the authority");
        }
        let parts: Vec<&str> = value.split('.').collect();
        if parts.len() != 3 || parts[0] != "v1" {
            return Err("not a v1 cookie");
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(parts[1].as_bytes());
        let signature = URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| "signature encoding")?;
        mac.verify_slice(&signature).map_err(|_| "bad signature")?;
        let payload: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[1])
                .map_err(|_| "payload encoding")?,
        )
        .map_err(|_| "payload json")?;
        if payload["version"] != 1 || payload["authority"] != authority {
            return Err("payload version or authority");
        }
        let issued = payload["issuedAt"].as_u64().ok_or("issuedAt")? as u128;
        let expires = payload["expiresAt"].as_u64().ok_or("expiresAt")? as u128;
        if issued > dsh_now_ms {
            return Err("issuedAt in the future");
        }
        if dsh_now_ms >= expires {
            return Err("expired");
        }
        if expires - issued > 30 * 24 * 60 * 60 * 1000 {
            return Err("lifetime above 30 days");
        }
        Ok(())
    }

    const BASE: &str = "http://127.0.0.1:3080";
    const AUTHORITY: &str = "127.0.0.1:3080";
    const T: u128 = 1_760_000_000_000;

    #[test]
    fn authority_parsing() {
        assert_eq!(
            authority_of("http://127.0.0.1:3080").as_deref(),
            Some("127.0.0.1:3080")
        );
        assert_eq!(
            authority_of("http://127.0.0.1:3080/").as_deref(),
            Some("127.0.0.1:3080")
        );
        assert!(authority_of("nonsense").is_none());
    }

    #[test]
    fn cookie_shape() {
        let secret = [7u8; 32];
        let c = mint_cookie("http://127.0.0.1:3080", &secret).unwrap();
        assert!(c.starts_with("dsh-auth-"), "前缀不对: {c}");
        let (name, value) = c.split_once('=').unwrap();
        assert!(name.len() > COOKIE_PREFIX.len());
        let parts: Vec<&str> = value.split('.').collect();
        assert_eq!(parts.len(), 3, "应该是 v1.body.sig 三段: {value}");
        assert_eq!(parts[0], "v1");
        let raw = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["authority"], "127.0.0.1:3080");
        assert!(v["expiresAt"].as_u64().unwrap() > v["issuedAt"].as_u64().unwrap());
    }

    #[test]
    fn cookie_is_backdated_with_unchanged_remaining_lifetime() {
        let secret = [7u8; 32];
        let before = now_ms();
        let c = mint_cookie("http://127.0.0.1:3080", &secret).unwrap();
        let after = now_ms();
        let value = c.split_once('=').unwrap().1;
        let raw = URL_SAFE_NO_PAD
            .decode(value.split('.').nth(1).unwrap())
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let issued = v["issuedAt"].as_u64().unwrap() as u128;
        let expires = v["expiresAt"].as_u64().unwrap() as u128;
        assert!(
            issued + 60_000 <= after,
            "issuedAt must be at least 60 s in the past"
        );
        assert!(
            issued + 60_000 + 5_000 >= before,
            "issuedAt must not be back-dated much further"
        );
        assert_eq!(expires - issued, COOKIE_LIFETIME_MS + 60_000);
        assert!(expires - issued < 30 * 24 * 60 * 60 * 1000);
        assert!(expires > after + COOKIE_LIFETIME_MS - 5_000);
    }

    #[test]
    fn signature_is_hmac_sha256() {
        let a = mint_cookie("http://127.0.0.1:3080", &[1u8; 32]).unwrap();
        let b = mint_cookie("http://127.0.0.1:3080", &[2u8; 32]).unwrap();
        let sig = |s: &str| s.rsplit('.').next().unwrap().to_string();
        assert_ne!(sig(&a), sig(&b));
    }

    #[test]
    fn public_session_secret_parser_returns_only_a_valid_encoded_key() {
        let secret = URL_SAFE_NO_PAD.encode([7u8; SECRET_BYTES]);
        let credentials = format!("other: ignored\nsecret: \"{secret}\"\n");
        assert_eq!(
            session_secret_from_text(&credentials).as_deref(),
            Some(secret.as_str())
        );
        assert_eq!(
            parse_session_secret(&credentials),
            Some([7u8; SECRET_BYTES].to_vec())
        );
        assert!(session_secret_from_text("secret: not-a-key\n").is_none());
        assert!(session_secret_from_text(&format!(
            "secret: {}\n",
            URL_SAFE_NO_PAD.encode([7u8; 31])
        ))
        .is_none());
    }

    #[test]
    fn a_dsh_rules_verifier_accepts_the_minted_cookie_and_rejects_a_future_dated_one() {
        let secret = [7u8; 32];
        let minted = mint_cookie_at(BASE, &secret, T).unwrap();
        assert_eq!(
            dsh_verify(&minted, AUTHORITY, &secret, T),
            Ok(()),
            "same clock"
        );
        assert_eq!(
            dsh_verify(&minted, AUTHORITY, &secret, T - 59_000),
            Ok(()),
            "DSH's clock 59 s behind ours"
        );
        assert_eq!(
            dsh_verify(&minted, AUTHORITY, &secret, T + COOKIE_LIFETIME_MS - 1),
            Ok(()),
            "the full hour remains usable"
        );
        assert_eq!(
            dsh_verify(&minted, AUTHORITY, &secret, T + COOKIE_LIFETIME_MS),
            Err("expired")
        );
        assert_eq!(
            dsh_verify(&minted, AUTHORITY, &[8u8; 32], T),
            Err("bad signature")
        );
        assert_eq!(
            dsh_verify(&minted, "127.0.0.1:3081", &secret, T),
            Err("cookie name does not bind the authority")
        );
        // A cookie whose issuedAt is ahead of DSH's clock is refused: our clock more than the back-date ahead.
        let future = mint_cookie_at(BASE, &secret, T + 120_000).unwrap();
        assert_eq!(
            dsh_verify(&future, AUTHORITY, &secret, T),
            Err("issuedAt in the future")
        );
        // The cookie the bridge actually mints from its clock is accepted by a DSH reading the same clock.
        let live = mint_cookie(BASE, &secret).unwrap();
        assert_eq!(dsh_verify(&live, AUTHORITY, &secret, now_ms()), Ok(()));
    }

    fn fixture_runtime(secret: &[u8]) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-dsh-credential-{}",
            crate::turn_ledger::mint_id("")
        )));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"fixture-token-abcdefghij"}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:3080","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        write_secret(&root, secret);
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    fn write_secret(root: &std::path::Path, secret: &[u8]) {
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            format!("secret: \"{}\"\n", URL_SAFE_NO_PAD.encode(secret)),
        )
        .unwrap();
    }

    const REFUSED: &str = "401 未授权：settings/describe";

    #[test]
    fn a_rejected_cookie_is_refreshed_from_the_reread_credentials_and_tried_once_more() {
        let (root, context) = fixture_runtime(&[1u8; 32]);
        let credential = Credential::mint(&context, BASE).unwrap();
        let mut seen = Vec::new();
        let check = check_login(&credential, Duration::ZERO, || {
            seen.push(credential.cookie());
            if seen.len() == 1 {
                // DSH restarted with a new signing secret: the cookie in force is refused.
                write_secret(&root, &[2u8; 32]);
                Err(REFUSED.to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(check.login, Login::Accepted);
        assert!(check.result.is_ok());
        assert_eq!(seen.len(), 2, "one refresh, one repeat");
        assert_eq!(
            dsh_verify(&seen[1], AUTHORITY, &[2u8; 32], now_ms()),
            Ok(()),
            "minted from the new secret"
        );
        assert_eq!(
            credential.cookie(),
            seen[1],
            "the fresh cookie stays in force"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn two_fresh_cookies_rejected_in_a_row_are_classified_as_a_failing_login() {
        let (root, context) = fixture_runtime(&[1u8; 32]);
        let credential = Credential::mint(&context, BASE).unwrap();
        let mut attempts = 0;
        let check = check_login(&credential, Duration::ZERO, || {
            attempts += 1;
            Err::<(), _>(REFUSED.to_string())
        });
        assert_eq!(attempts, 2, "never more than one repeat");
        assert_eq!(check.login, Login::Rejected { in_a_row: 2 });
        assert!(check.login.failing());
        assert_eq!(check.result.unwrap_err(), REFUSED);

        // No answer from DSH proves nothing about the login and is not repeated.
        let mut attempts = 0;
        let check = check_login(&credential, Duration::ZERO, || {
            attempts += 1;
            Err::<(), _>("HTTP 连接失败：settings/describe".to_string())
        });
        assert_eq!((attempts, check.login), (1, Login::Unchecked));

        // A refusal of the method proves DSH accepted the cookie.
        let check = check_login(&credential, Duration::ZERO, || {
            Err::<(), _>("settings/describe 被拒：x: y".to_string())
        });
        assert_eq!(check.login, Login::Accepted);

        // Without a readable secret there is no fresh replacement: one rejection only.
        std::fs::write(root.join("user/dsh/.credentials.yaml"), "secret: broken\n").unwrap();
        let mut attempts = 0;
        let check = check_login(&credential, Duration::ZERO, || {
            attempts += 1;
            Err::<(), _>(REFUSED.to_string())
        });
        assert_eq!(
            (attempts, check.login),
            (1, Login::Rejected { in_a_row: 1 })
        );
        assert!(!check.login.failing());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_watch_fails_once_per_streak_of_two_rejected_fresh_cookies() {
        let mut watch = LoginWatch::default();
        assert_eq!(watch.observe(&Login::Accepted), LoginEdge::Steady);
        assert_eq!(
            watch.observe(&Login::Rejected { in_a_row: 1 }),
            LoginEdge::Steady,
            "one rejection is refreshed"
        );
        assert!(!watch.failing());
        assert_eq!(
            watch.observe(&Login::Accepted),
            LoginEdge::Steady,
            "an acceptance ends the streak"
        );
        assert_eq!(
            watch.observe(&Login::Rejected { in_a_row: 1 }),
            LoginEdge::Steady
        );
        assert_eq!(
            watch.observe(&Login::Unchecked),
            LoginEdge::Steady,
            "no answer is no evidence"
        );
        assert_eq!(
            watch.observe(&Login::Rejected { in_a_row: 1 }),
            LoginEdge::Failing,
            "two in a row across checks"
        );
        assert!(watch.failing());
        for _ in 0..10 {
            assert_eq!(
                watch.observe(&Login::Rejected { in_a_row: 2 }),
                LoginEdge::Steady,
                "one alert per streak"
            );
        }
        assert_eq!(watch.observe(&Login::Accepted), LoginEdge::Recovered);
        assert!(!watch.failing());
        assert_eq!(
            watch.observe(&Login::Rejected { in_a_row: 2 }),
            LoginEdge::Failing,
            "a new streak alerts again"
        );
    }

    #[test]
    fn a_failing_streak_owes_its_one_alert_until_it_is_delivered() {
        let mut watch = LoginWatch::default();
        assert!(!watch.alert_owed());
        watch.observe(&Login::Rejected { in_a_row: 1 });
        assert!(!watch.alert_owed(), "one rejection is not a failing streak");
        assert_eq!(
            watch.observe(&Login::Rejected { in_a_row: 1 }),
            LoginEdge::Failing
        );
        assert!(watch.alert_owed());
        for _ in 0..3 {
            watch.observe(&Login::Rejected { in_a_row: 2 });
            watch.observe(&Login::Unchecked);
            assert!(
                watch.alert_owed(),
                "still owed until a notice was accepted for sending"
            );
        }
        watch.alert_delivered();
        assert!(!watch.alert_owed());
        watch.observe(&Login::Rejected { in_a_row: 2 });
        assert!(!watch.alert_owed(), "delivered once per streak");

        // A recovery ends the debt even if the alert never got through.
        let mut watch = LoginWatch::default();
        watch.observe(&Login::Rejected { in_a_row: 2 });
        assert!(watch.alert_owed());
        assert_eq!(watch.observe(&Login::Accepted), LoginEdge::Recovered);
        assert!(!watch.alert_owed());
        watch.observe(&Login::Rejected { in_a_row: 2 });
        assert!(watch.alert_owed(), "a new streak owes a new alert");
    }

    #[test]
    fn an_owed_alert_is_offered_at_once_then_at_most_once_per_interval() {
        let mut watch = LoginWatch::default();
        assert!(!watch.alert_due(0), "nothing owed before a failing streak");
        watch.observe(&Login::Rejected { in_a_row: 2 });
        let t0 = 1_000_000;
        assert!(watch.alert_due(t0), "the first offer on the failing edge is immediate");
        for step in [1, 5_000, 30_000, ALERT_OFFER_INTERVAL_MS - 1] {
            assert!(!watch.alert_due(t0 + step), "a refused offer is not repeated {step} ms later");
        }
        assert!(watch.alert_due(t0 + ALERT_OFFER_INTERVAL_MS), "offered again after the interval");
        assert!(!watch.alert_due(t0 + ALERT_OFFER_INTERVAL_MS + 1), "the interval restarts at each offer");
        assert!(watch.alert_due(t0 + 2 * ALERT_OFFER_INTERVAL_MS));

        watch.alert_delivered();
        assert!(!watch.alert_due(t0 + 10 * ALERT_OFFER_INTERVAL_MS), "delivered: nothing owed");

        // A recovery clears the debt and the interval; the next streak is offered at once.
        let mut watch = LoginWatch::default();
        watch.observe(&Login::Rejected { in_a_row: 2 });
        assert!(watch.alert_due(t0));
        watch.observe(&Login::Accepted);
        assert!(!watch.alert_due(t0 + ALERT_OFFER_INTERVAL_MS));
        watch.observe(&Login::Rejected { in_a_row: 2 });
        assert!(watch.alert_due(t0 + 1), "a new streak is offered at once");

        // A clock that stepped back does not silence the alert for good.
        assert!(watch.alert_due(t0 - 10 * ALERT_OFFER_INTERVAL_MS));
    }

    #[test]
    fn the_pause_before_the_repeat_waits_and_never_outlasts_the_deadline() {
        assert_eq!(repeat_pause(None), REPEAT_PAUSE);
        assert_eq!(REPEAT_PAUSE, Duration::from_millis(300));
        let near = Instant::now() + Duration::from_millis(100);
        assert!(repeat_pause(Some(near)) <= Duration::from_millis(100));
        assert_eq!(
            repeat_pause(Some(Instant::now() - Duration::from_secs(1))),
            Duration::ZERO
        );
        assert_eq!(
            repeat_pause(Some(Instant::now() + Duration::from_secs(60))),
            REPEAT_PAUSE
        );

        let (root, context) = fixture_runtime(&[1u8; 32]);
        let credential = Credential::mint(&context, BASE).unwrap();
        let pause = Duration::from_millis(80);
        let mut stamps = Vec::new();
        check_login(&credential, pause, || {
            stamps.push(Instant::now());
            Err::<(), _>(REFUSED.to_string())
        });
        assert_eq!(stamps.len(), 2);
        assert!(
            stamps[1] - stamps[0] >= pause,
            "the repeat waits for a rotation in progress to settle"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
