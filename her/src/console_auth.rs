//! Console credential comparison and the selected runtime's offline password commands.

mod storage;

pub(crate) use storage::{
    administrator_password_is_set_in, admit_browser, archive_credentials, auth_status,
    scrub_auth_snapshot, start_browser_clock, ArchivedCredentials, BrowserSessionView,
    IssuedSession, SCHEMA_V3_ADDED, SCHEMA_V3_IMPLICIT,
};
#[cfg(test)]
pub(crate) use storage::{
    issue_browser_session, reset_administrator_password, revoke_browser_sessions,
    seed_browser_session, throttle_delay_ms, THROTTLE_MAX_MS,
};

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use argon2::password_hash::{
    Error as PhcError, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, IsTerminal, Read};
use std::net::TcpStream;
use std::path::Path;
use subtle::ConstantTimeEq;

/// A generated console token: 24 bytes of OS randomness as 48 hex characters.
pub(crate) fn generate_console_token() -> Result<String, String> {
    generate_console_token_from(&mut |bytes: &mut [u8]| {
        getrandom::getrandom(bytes).map_err(|e| format!("系统随机数不可用：{e}"))
    })
}

/// The token made from exactly the bytes `fill` provides (the entropy source is the only input).
pub(crate) fn generate_console_token_from(
    fill: &mut dyn FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<String, String> {
    let mut bytes = [0u8; 24];
    fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn credential_digest(credential: &str) -> [u8; 32] {
    Sha256::digest(credential.as_bytes()).into()
}

pub(crate) fn digests_equal<const N: usize>(expected: &[u8; N], presented: &[u8; N]) -> bool {
    bool::from(expected.ct_eq(presented))
}

/// Compare nonempty credentials after the caller has applied its own input rules.
pub(crate) fn credential_eq(expected: &str, presented: &str) -> bool {
    if expected.is_empty() || presented.is_empty() {
        return false;
    }
    digests_equal(&credential_digest(expected), &credential_digest(presented))
}

/// Read the lowercase hex spelling used for persisted credential digests.
pub(crate) fn decode_digest<const N: usize>(text: &str) -> Option<[u8; N]> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }

    if text.len() != N * 2 {
        return None;
    }
    let mut digest = [0; N];
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (value, pair) in digest.iter_mut().zip(pairs) {
        *value = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(digest)
}

pub(crate) const RESTORE_GENERATION_FILE: &str = "console-auth-generation";
const GENERATION_UNAVAILABLE: &str = "console restore generation is unavailable";
pub(crate) type Generation = [u8; 32];

pub(crate) fn new_generation() -> Result<Generation, PasswordError> {
    let mut generation = [0; 32];
    getrandom::getrandom(&mut generation).map_err(|_| PasswordError::Unavailable)?;
    Ok(generation)
}

/// Lock order: credential_mutation_gate -> config_gate -> browser_anchors -> store_gate
/// (Argon2id hashing/verification always runs outside all gates).
pub(crate) fn credential_mutation_gate(context: &RuntimeContext) -> Result<fs::File, String> {
    context
        .open_locked_file(RuntimeArea::Data, "locks/console-credential.lock")
        .map_err(|_| "console credential mutation is busy or unavailable".to_string())
}

fn unauthenticated_context(context: &RuntimeContext) -> RuntimeContext {
    let mut unauth = context.clone();
    unauth.console_authority = None;
    unauth
}

/// Read only under the configuration gate. A bad existing marker stays distinct from absence.
pub(crate) fn restore_generation(context: &RuntimeContext) -> Result<Option<Generation>, String> {
    let resolve = || {
        context.resolve_path(
            RuntimeArea::Data,
            RESTORE_GENERATION_FILE,
            PathIntent::ReadOptionalFile,
        )
    };
    let path = resolve()?;
    let Some(mut file) =
        crate::guarded_fs::open_protected_file(&path).map_err(|_| GENERATION_UNAVAILABLE)?
    else {
        return Ok(None);
    };
    if resolve()? != path {
        return Err(GENERATION_UNAVAILABLE.to_string());
    }
    let mut generation = [0; 32];
    file.read_exact(&mut generation).map_err(|_| GENERATION_UNAVAILABLE)?;
    if file.read(&mut [0]).map_err(|_| GENERATION_UNAVAILABLE)? != 0 {
        return Err(GENERATION_UNAVAILABLE.to_string());
    }
    Ok(Some(generation))
}

#[derive(Debug)]
pub(crate) enum GenerationWriteError {
    Unpublished(String),
    Published(String),
}

/// The caller holds the configuration gate. The destination marker is outside the replacement set.
pub(crate) fn rotate_restore_generation(
    context: &RuntimeContext,
) -> Result<Generation, GenerationWriteError> {
    use GenerationWriteError::{Published, Unpublished};
    let generation = new_generation().map_err(|_| Unpublished(GENERATION_UNAVAILABLE.to_string()))?;
    let area = RuntimeArea::Data;
    let name = RESTORE_GENERATION_FILE;
    let (target, staged) = context.stage_atomic_from(area, name, &mut generation.as_slice())
        .map_err(|failure| Unpublished(failure.to_string()))?;
    staged.commit(&mut || {
        context.revalidate_atomic_target(area, name, &target)?;
        crate::guarded_fs::open_protected_file(&target)
            .map_err(|_| GENERATION_UNAVAILABLE.to_string())?;
        Ok(())
    }).map_err(|failure| {
        // Directory sync happens after replacement. Preserve the new marker and use rollback.
        if failure.class() == "dir/sync" {
            Published(failure.to_string())
        } else {
            Unpublished(failure.to_string())
        }
    })?;
    context.resolve_path(area, name, PathIntent::ReadFile)
        .map_err(|error| Published(error.to_string()))?;
    Ok(generation)
}

pub(crate) const MAX_BROWSER_SESSIONS: usize = 128;
pub(crate) const BROWSER_IDLE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(crate) fn canonical_host(port: u16) -> String {
    if port == 80 {
        "her.localhost".into()
    } else {
        format!("her.localhost:{port}")
    }
}

pub(crate) fn canonical_origin(port: u16) -> String {
    format!("http://{}", canonical_host(port))
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserProof {
    pub(crate) id_digest: [u8; 32],
    pub(crate) origin: String,
    pub(crate) auth_generation: Generation,
    pub(crate) restore_generation: Generation,
}

/// Lock order: credential_mutation_gate -> config_gate -> browser_anchors -> store_gate
/// (Argon2id hashing/verification always runs outside all gates).
#[derive(Debug, Default)]
pub(crate) struct BrowserAnchors(
    pub(crate) std::sync::Mutex<std::collections::HashMap<[u8; 32], (i64, std::time::Instant)>>,
);

pub(crate) const DESKTOP_GRANTS_FILE: &str = "console-desktop-grants";

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopProof {
    bearer_digest: [u8; 32],
    generation: [u8; 32],
    issuer_pid: u32,
    issuer_start: u64,
}

pub(crate) fn recheck_desktop(
    context: &RuntimeContext,
    proof: &DesktopProof,
    token: &[u8; 32],
) -> Result<bool, (u16, String)> {
    let current = selected_desktop_proof(context, &proof.bearer_digest, token)?;
    Ok(current.is_some_and(|current| {
        current.issuer_pid == proof.issuer_pid
            && current.issuer_start == proof.issuer_start
            && digests_equal(&current.generation, &proof.generation)
    }))
}

#[cfg(not(windows))]
fn selected_desktop_proof(
    _: &RuntimeContext,
    _: &[u8; 32],
    _: &[u8; 32],
) -> Result<Option<DesktopProof>, (u16, String)> {
    Ok(None)
}

#[cfg(windows)]
fn selected_desktop_proof(
    context: &RuntimeContext,
    bearer: &[u8; 32],
    token: &[u8; 32],
) -> Result<Option<DesktopProof>, (u16, String)> {
    let Some(registry) = desktop_grants::read(context)? else {
        return Ok(None);
    };
    let now = desktop_grants::ticks();
    if !registry.verified(context) {
        return Ok(None);
    }
    Ok(registry
        .grants
        .iter()
        .find(|grant| {
            grant.issued <= now
                && now < grant.expires
                && digests_equal(bearer, &grant.bearer_digest)
                && digests_equal(token, &grant.token_digest)
        })
        .map(|_| DesktopProof {
            bearer_digest: *bearer,
            generation: registry.generation,
            issuer_pid: registry.issuer_pid,
            issuer_start: registry.issuer_start,
        }))
}

#[cfg(windows)]
mod desktop_grants {
    use super::*;

    pub(super) const LIFETIME: u64 = 12 * 60 * 60 * 1000;
    pub(super) const LIMIT: usize = 128;

    #[cfg(test)]
    thread_local! {
        pub(super) static TEST_TICKS: std::cell::Cell<Option<u64>> =
            const { std::cell::Cell::new(None) };
    }

    pub(super) fn ticks() -> u64 {
        #[cfg(test)]
        if let Some(ticks) = TEST_TICKS.with(std::cell::Cell::get) {
            return ticks;
        }
        // SAFETY: GetTickCount64 takes no arguments.
        unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Registry {
        pub(super) v: u8,
        pub(super) generation: [u8; 32],
        pub(super) issuer_pid: u32,
        pub(super) issuer_start: u64,
        pub(super) grants: std::collections::VecDeque<Grant>,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Grant {
        pub(super) bearer_digest: [u8; 32],
        pub(super) token_digest: [u8; 32],
        pub(super) issued: u64,
        pub(super) expires: u64,
    }

    fn unavailable() -> (u16, String) {
        (503, "desktop grant registry is unavailable".into())
    }

    fn owner(context: &RuntimeContext) -> Option<crate::lifecycle::ProcessIdentity> {
        let _path = context
            .resolve_path(
                RuntimeArea::Data,
                &format!("logs/{}", crate::lifecycle::CONSOLE_LOCK),
                PathIntent::ReadFile,
            )
            .ok()?;
        match crate::lifecycle::verify_owner(
            &mut crate::lifecycle::LiveLifecycle,
            context.data_dir(),
            crate::lifecycle::OwnerRole::Console,
            &crate::lifecycle::ImagePolicy::HealthReadOnly,
            crate::lifecycle::ProbeMode::Existing,
        ) {
            crate::lifecycle::OwnerObservation::Verified(owner) => Some(owner.marker.identity),
            _ => None,
        }
    }

    impl Registry {
        pub(super) fn verified(&self, context: &RuntimeContext) -> bool {
            owner(context).is_some_and(|identity| {
                identity.pid == self.issuer_pid && identity.start == self.issuer_start
            })
        }
    }

    pub(super) fn read(context: &RuntimeContext) -> Result<Option<Registry>, (u16, String)> {
        let Some(bytes) = context
            .read_protected_data_file(DESKTOP_GRANTS_FILE, 128 * 1024)
            .map_err(|_| unavailable())?
        else {
            return Ok(None);
        };
        let registry: Registry = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let mut ids = std::collections::HashSet::new();
        if registry.v != 1
            || registry.issuer_pid == 0
            || registry.issuer_start == 0
            || registry.grants.len() > LIMIT
            || registry.grants.iter().any(|grant| {
                grant.issued.checked_add(LIFETIME) != Some(grant.expires)
                    || !ids.insert(grant.bearer_digest)
            })
        {
            return Err(unavailable());
        }
        Ok(Some(registry))
    }

    fn write(context: &RuntimeContext, registry: &Registry) -> Result<(), (u16, String)> {
        let bytes = serde_json::to_vec(registry).map_err(|_| unavailable())?;
        context
            .write_atomic(RuntimeArea::Data, DESKTOP_GRANTS_FILE, &bytes)
            .map_err(|_| unavailable())
    }

    pub(super) fn current_issuer(context: &RuntimeContext) -> Result<Registry, (u16, String)> {
        let owner = owner(context)
            .filter(|identity| identity.pid == std::process::id())
            .ok_or_else(unavailable)?;
        if let Some(existing) = read(context)? {
            if existing.issuer_pid == owner.pid && existing.issuer_start == owner.start {
                return Ok(existing);
            }
        }
        let registry = Registry {
            v: 1,
            generation: new_generation().map_err(|_| unavailable())?,
            issuer_pid: owner.pid,
            issuer_start: owner.start,
            grants: Default::default(),
        };
        write(context, &registry)?;
        Ok(registry)
    }

    pub(super) fn issue(
        context: &RuntimeContext,
        mut registry: Registry,
        token_digest: [u8; 32],
    ) -> Result<String, (u16, String)> {
        let token = generate_console_token().map_err(|_| unavailable())?;
        let now = ticks();
        let expires = now.checked_add(LIFETIME).ok_or_else(unavailable)?;
        registry
            .grants
            .retain(|grant| grant.issued <= now && now < grant.expires);
        while registry.grants.len() >= LIMIT {
            registry.grants.pop_front();
        }
        registry.grants.push_back(Grant {
            bearer_digest: credential_digest(&token),
            token_digest,
            issued: now,
            expires,
        });
        write(context, &registry)?;
        Ok(token)
    }
}

#[cfg(windows)]
pub(crate) fn same_installing_user(stream: &TcpStream) -> bool {
    crate::process_identity::tcp_peer_user(stream)
        .and_then(|peer| {
            crate::process_identity::current_user().map(|own| {
                #[cfg(debug_assertions)]
                let fixture_matches = std::env::var("HER_TEST_DESKTOP_PEER_SID_HEX")
                    .map(|sid| {
                        sid.eq_ignore_ascii_case(
                            &peer
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>(),
                        )
                    })
                    .unwrap_or(true);
                #[cfg(not(debug_assertions))]
                let fixture_matches = true;
                peer == own && fixture_matches
            })
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub(crate) fn same_installing_user(_stream: &TcpStream) -> bool {
    false
}

#[cfg(windows)]
pub(crate) enum DesktopSignInProof<'a> {
    Browser { id_digests: Vec<[u8; 32]> },
    Native { presented_token: Option<&'a str> },
}

#[cfg(windows)]
pub(crate) fn desktop_sign_in(
    stream: &TcpStream,
    context: &RuntimeContext,
    port: u16,
    proof_input: DesktopSignInProof<'_>,
) -> Result<String, (u16, String)> {
    let native_proof = |presented: Option<&str>| -> Result<(), (u16, String)> {
        let expected = crate::desktop::console_token(context)
            .map_err(|_| (403, "protected console token file is unavailable".into()))?;
        if !presented.is_some_and(|presented| credential_eq(&expected, presented)) {
            return Err((401, "console credential is required".into()));
        }
        Ok(())
    };
    let revoked = || (401, "console credential was revoked".into());
    let load_cfg = || {
        context
            .load_config()
            .map_err(|_| (503, "runtime config is unavailable".to_string()))
    };
    let (token_digest, generation, authority_proof) = {
        let _gate = crate::social_state::acquire_config_gate(context)
            .map_err(crate::social_state::SocialError::into_http)?;
        let cfg = load_cfg()?;
        let expected = checked_console_token(context, &cfg)?;
        if expected.is_empty() {
            return Err(revoked());
        }
        let digest = credential_digest(&expected);
        let authority_proof = match &proof_input {
            DesktopSignInProof::Browser { id_digests } => {
                let Some(admission) = admit_browser(
                    context,
                    &canonical_origin(port),
                    Some(digest),
                    id_digests,
                    false,
                )?
                else {
                    return Err(revoked());
                };
                Some(ConsoleAuthority::Browser {
                    token_digest: digest,
                    grant: admission.proof,
                })
            }
            DesktopSignInProof::Native { presented_token } => {
                native_proof(*presented_token)?;
                None
            }
        };
        (digest, desktop_grants::current_issuer(context)?.generation, authority_proof)
    };
    if !same_installing_user(stream) {
        return Err((403, "desktop sign-in requires the installing user".into()));
    }
    let _gate = crate::social_state::acquire_config_gate(context)
        .map_err(crate::social_state::SocialError::into_http)?;
    let cfg = load_cfg()?;
    let effective = checked_console_token(context, &cfg)?;
    let registry = desktop_grants::read(context)?
        .filter(|registry| {
            registry.verified(context) && digests_equal(&generation, &registry.generation)
        })
        .ok_or_else(revoked)?;
    if effective.is_empty() || !digests_equal(&token_digest, &credential_digest(&effective)) {
        return Err(revoked());
    }
    match (&authority_proof, &proof_input) {
        (Some(authority), _) => authority.recheck(context, &cfg)?,
        (None, DesktopSignInProof::Native { presented_token }) => native_proof(*presented_token)?,
        (None, DesktopSignInProof::Browser { .. }) => return Err(revoked()),
    }
    desktop_grants::issue(context, registry, token_digest)
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum ConsoleAuthority {
    Machine { token_digest: [u8; 32] },
    Desktop { token_digest: [u8; 32], grant: DesktopProof },
    Browser { token_digest: [u8; 32], grant: BrowserProof },
}

impl ConsoleAuthority {
    /// Configuration is fresh and the caller holds the configuration gate.
    pub(crate) fn recheck(
        &self,
        context: &RuntimeContext,
        cfg: &serde_json::Value,
    ) -> Result<(), (u16, String)> {
        let revoked = || (401, "console credential was revoked".into());
        let live = checked_console_token(context, cfg)?;
        let token_digest = match self {
            Self::Machine { token_digest }
            | Self::Desktop { token_digest, .. }
            | Self::Browser { token_digest, .. } => token_digest,
        };
        let matches_token =
            !live.is_empty() && digests_equal(token_digest, &credential_digest(&live));
        let valid = match self {
            Self::Machine { .. } => matches_token,
            Self::Desktop { grant, .. } => {
                matches_token && recheck_desktop(context, grant, token_digest)?
            }
            Self::Browser { grant, .. } => {
                let valid = storage::recheck_browser(
                    context,
                    grant,
                    matches_token.then_some(*token_digest),
                )?;
                valid
                    && matches_token
                    && context
                        .console_port()
                        .is_none_or(|port| grant.origin == canonical_origin(port))
            }
        };
        if valid {
            Ok(())
        } else {
            Err(revoked())
        }
    }
}

pub(crate) fn admit_explicit_authority(
    context: &RuntimeContext,
    presented: &str,
    expected: &str,
    allow_raw: bool,
) -> Result<Option<ConsoleAuthority>, (u16, String)> {
    if presented.is_empty() || expected.is_empty() {
        return Ok(None);
    }
    let token_digest = credential_digest(expected);
    let bearer_digest = credential_digest(presented);
    if digests_equal(&token_digest, &bearer_digest) {
        return Ok(allow_raw.then_some(ConsoleAuthority::Machine { token_digest }));
    }
    Ok(selected_desktop_proof(context, &bearer_digest, &token_digest)?
        .map(|grant| ConsoleAuthority::Desktop { token_digest, grant }))
}

pub(crate) fn checked_console_token(
    context: &RuntimeContext, cfg: &serde_json::Value,
) -> Result<String, (u16, String)> {
    ensure_credentials_ready(context)?;
    Ok(context.effective_console_token(cfg))
}

pub(crate) const ROTATION_FILE: &str = "console-token-rotation";
pub(crate) const TOKEN_FILE: &str = "console-token";
const ROTATION_UNAVAILABLE: &str = "console token rotation is unavailable";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RotationIntent {
    v: u8,
    prior_configured: Option<[u8; 32]>,
    prior_file: Option<[u8; 32]>,
    proposed: [u8; 32],
    configured: bool,
}

fn read_rotation_file(
    context: &RuntimeContext,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, String> {
    context
        .read_protected_data_file(name, limit)
        .map_err(|_| ROTATION_UNAVAILABLE.to_string())
}

fn remove_rotation_file(context: &RuntimeContext, name: &str) -> Result<(), String> {
    context
        .remove_protected_data_file(name)
        .map_err(|_| ROTATION_UNAVAILABLE.to_string())
}

fn file_digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn configured_digest(cfg: &serde_json::Value) -> Option<[u8; 32]> {
    cfg.get("consoleToken").and_then(serde_json::Value::as_str).map(str::trim)
        .filter(|value| !value.is_empty()).map(credential_digest)
}

fn optional_digest_eq(left: Option<[u8; 32]>, right: Option<[u8; 32]>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => digests_equal(&left, &right),
        (None, None) => true,
        _ => false,
    }
}

pub(crate) fn ensure_credentials_ready(context: &RuntimeContext) -> Result<(), (u16, String)> {
    if crate::restore::recovery_pending(context) {
        return Err((503, crate::restore::RECOVERY_REASON.into()));
    }
    if !matches!(read_rotation_file(context, ROTATION_FILE, 4096), Ok(None)) {
        return Err((503, "console credentials require recovery".into()));
    }
    Ok(())
}

pub(crate) fn raw_token_config(context: &RuntimeContext) -> Result<serde_json::Value, String> {
    let text = context
        .read_optional_text(RuntimeArea::Config, "config.json")?
        .ok_or(ROTATION_UNAVAILABLE)?;
    parse_token_config(&text)
}

fn parse_token_config(text: &str) -> Result<serde_json::Value, String> {
    let config: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| ROTATION_UNAVAILABLE)?;
    if !config.is_object() { return Err(ROTATION_UNAVAILABLE.into()); }
    Ok(config)
}

fn config_with_token(mut cfg: serde_json::Value, token: &str, configured: bool) -> String {
    cfg["consoleToken"] = serde_json::json!(if configured { token } else { "" });
    cfg.to_string()
}

pub(crate) fn reconcile_token_rotation(context: &RuntimeContext) -> Result<(), String> {
    let Some(bytes) = read_rotation_file(context, ROTATION_FILE, 4096)? else {
        return Ok(());
    };
    let intent: RotationIntent = serde_json::from_slice(&bytes).map_err(|_| ROTATION_UNAVAILABLE)?;
    if intent.v != 1 { return Err(ROTATION_UNAVAILABLE.into()); }
    let cfg = raw_token_config(context)?;
    let configured = configured_digest(&cfg);
    let old_mode = optional_digest_eq(configured, intent.prior_configured);
    let new_mode = optional_digest_eq(configured, intent.configured.then_some(intent.proposed));
    let file = read_rotation_file(context, TOKEN_FILE, 1024 * 1024)?;
    let on_disk = file.as_deref().map(file_digest);
    if (old_mode || new_mode) && optional_digest_eq(on_disk, Some(intent.proposed)) {
        let token = file.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok())
            .filter(|token| !token.is_empty()).ok_or(ROTATION_UNAVAILABLE)?;
        if !new_mode {
            let config = config_with_token(cfg, token, intent.configured);
            context.write_atomic(RuntimeArea::Config, "config.json", config.as_bytes())
                .map_err(|_| ROTATION_UNAVAILABLE.to_string())?;
        }
    } else if !(old_mode && optional_digest_eq(on_disk, intent.prior_file)) {
        return Err(ROTATION_UNAVAILABLE.into());
    }
    remove_rotation_file(context, ROTATION_FILE)
}

#[cfg(feature = "parity-clock")]
fn rotation_fixture_cut(at: &str) {
    if std::env::var("HER_TEST_ROTATION_CUT").as_deref() == Ok(at) {
        std::process::exit(86);
    }
}

pub(crate) fn rotate_console_token(
    context: &RuntimeContext,
    token: &str,
    configured: bool,
) -> Result<crate::config_tx::ConfigDiff, crate::config_tx::CommitError> {
    let unavailable = |_: String| crate::config_tx::CommitError::plain(503, ROTATION_UNAVAILABLE);
    ensure_credentials_ready(context)?;
    let read_error = || crate::config_tx::CommitError::plain(503, ROTATION_UNAVAILABLE);
    let before = context
        .read_optional_text(RuntimeArea::Config, "config.json")
        .map_err(|_| read_error())?
        .ok_or_else(read_error)?;
    let cfg = parse_token_config(&before).map_err(unavailable)?;
    let token_file_error = || (500, "console token file is unavailable".to_string());
    let prior_file =
        read_rotation_file(context, TOKEN_FILE, 1024 * 1024).map_err(|_| token_file_error())?;
    let (target, staged) = context
        .stage_atomic_from(RuntimeArea::Data, TOKEN_FILE, &mut token.as_bytes())
        .map_err(|_| token_file_error())?;
    let intent = RotationIntent {
        v: 1,
        prior_configured: configured_digest(&cfg),
        prior_file: prior_file.as_deref().map(file_digest),
        proposed: credential_digest(token),
        configured,
    };
    let journal = serde_json::to_vec(&intent).map_err(|_| unavailable(String::new()))?;
    let after = config_with_token(cfg, token, configured);
    let (config_target, staged_config) = context
        .stage_atomic_from(RuntimeArea::Config, "config.json", &mut after.as_bytes())
        .map_err(|_| (500, "console token configuration is unavailable".to_string()))?;
    storage::revoke_browser_sessions(context)?;
    remove_rotation_file(context, DESKTOP_GRANTS_FILE).map_err(unavailable)?;
    context
        .write_atomic(RuntimeArea::Data, ROTATION_FILE, &journal)
        .map_err(unavailable)?;
    #[cfg(feature = "parity-clock")]
    rotation_fixture_cut("before-token");
    staged
        .commit(&mut || {
            context.revalidate_atomic_target(RuntimeArea::Data, TOKEN_FILE, &target)?;
            crate::guarded_fs::open_protected_file(&target)
                .map_err(|_| ROTATION_UNAVAILABLE.to_string())?;
            Ok::<(), String>(())
        })
        .map_err(|_| unavailable(String::new()))?;
    #[cfg(feature = "parity-clock")]
    rotation_fixture_cut("after-token");
    let result = (|| {
        let _authority = context.authority_gate()?;
        staged_config
            .commit(&mut || {
                context.revalidate_atomic_target(RuntimeArea::Config, "config.json", &config_target)
            })
            .map_err(|_| ROTATION_UNAVAILABLE.to_string())
    })();
    if result.is_err() {
        let still_old = raw_token_config(context)
            .is_ok_and(|cfg| optional_digest_eq(configured_digest(&cfg), intent.prior_configured));
        if still_old {
            let rollback = match prior_file {
                Some(bytes) => context.write_atomic(RuntimeArea::Data, TOKEN_FILE, &bytes),
                None => remove_rotation_file(context, TOKEN_FILE),
            };
            if rollback.is_ok() {
                remove_rotation_file(context, ROTATION_FILE).map_err(unavailable)?;
            }
        }
        return Err(unavailable(String::new()));
    }
    #[cfg(feature = "parity-clock")]
    rotation_fixture_cut("after-config");
    let diff = crate::config_tx::ConfigDiff { before, after };
    remove_rotation_file(context, ROTATION_FILE).map_err(|_| {
        crate::config_tx::CommitError::with_diff(503, ROTATION_UNAVAILABLE, Some(diff.clone()))
    })?;
    Ok(diff)
}

const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const LANES: u32 = 1;
const SALT_BYTES: usize = 16;
const OUTPUT_BYTES: usize = 32;
const MIN_PASSWORD_CHARS: usize = 8;
const MAX_PASSWORD_CHARS: usize = 1024;
const MAX_STDIN_LINE_BYTES: u64 = 8 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PasswordError {
    Input,
    Proof,
    AlreadySet,
    Busy,
    Unavailable,
}

/// The only password hash profile supported by the credential store.
pub(crate) struct AdministratorPasswordHash(String);

impl AdministratorPasswordHash {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::parse_phc(value)?;
        Some(Self(value.to_string()))
    }

    fn parse_phc(value: &str) -> Option<PasswordHash<'_>> {
        let hash = PasswordHash::new(value).ok()?;
        if hash.algorithm.as_str() != "argon2id"
            || hash.version != Some(19)
            || hash.params.iter().count() != 3
            || hash.params.get_decimal("m") != Some(MEMORY_KIB)
            || hash.params.get_decimal("t") != Some(ITERATIONS)
            || hash.params.get_decimal("p") != Some(LANES)
            || hash.salt?.decode_b64(&mut [0; SALT_BYTES]).ok()?.len() != SALT_BYTES
            || hash.hash?.len() != OUTPUT_BYTES
        {
            return None;
        }
        Some(hash)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The caller has already validated the plaintext length.
    fn derive(password: &str) -> Result<Self, PasswordError> {
        let mut random = [0; SALT_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| PasswordError::Unavailable)?;
        let salt = SaltString::encode_b64(&random).map_err(|_| PasswordError::Unavailable)?;
        let params = Params::new(MEMORY_KIB, ITERATIONS, LANES, Some(OUTPUT_BYTES))
            .map_err(|_| PasswordError::Unavailable)?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| PasswordError::Unavailable)?;
        Ok(Self(hash.to_string()))
    }

    pub(crate) fn verify(&self, password: &str) -> Result<bool, PasswordError> {
        validate_password_maximum(password)?;
        let hash = Self::parse_phc(self.as_str()).ok_or(PasswordError::Unavailable)?;
        match Argon2::default().verify_password(password.as_bytes(), &hash) {
            Ok(()) => Ok(true),
            Err(PhcError::Password) => Ok(false),
            Err(_) => Err(PasswordError::Unavailable),
        }
    }
}

fn validate_password_maximum(password: &str) -> Result<(), PasswordError> {
    if password.chars().take(MAX_PASSWORD_CHARS + 1).count() > MAX_PASSWORD_CHARS {
        return Err(PasswordError::Input);
    }
    Ok(())
}

fn validate_new_password(password: &str) -> Result<(), PasswordError> {
    validate_password_maximum(password)?;
    if password.chars().take(MIN_PASSWORD_CHARS).count() < MIN_PASSWORD_CHARS {
        return Err(PasswordError::Input);
    }
    Ok(())
}

/// First-run accepts only the selected runtime's effective raw token. Reload it at each check.
fn check_first_run_proof(context: &RuntimeContext, proof: &str) -> Result<(), PasswordError> {
    let cfg = context
        .load_config()
        .map_err(|_| PasswordError::Unavailable)?;
    let expected = checked_console_token(context, &cfg).map_err(|_| PasswordError::Unavailable)?;
    if !credential_eq(&expected, proof) {
        return Err(PasswordError::Proof);
    }
    Ok(())
}

pub(crate) fn first_run(
    context: &RuntimeContext,
    proof: &str,
    password: &str,
    actor: &str,
) -> Result<(), PasswordError> {
    first_run_with_password_work(
        context,
        proof,
        password,
        actor,
        AdministratorPasswordHash::derive,
    )
}

fn first_run_with_password_work(
    context: &RuntimeContext,
    proof: &str,
    password: &str,
    actor: &str,
    derive: impl FnOnce(&str) -> Result<AdministratorPasswordHash, PasswordError>,
) -> Result<(), PasswordError> {
    validate_new_password(password)?;
    check_first_run_proof(context, proof)?;
    let hash = derive(password)?;
    let _credential = credential_mutation_gate(context).map_err(|_| PasswordError::Busy)?;
    let unauth = unauthenticated_context(context);
    let _config = crate::social_state::acquire_config_gate(&unauth)
        .map_err(|_| PasswordError::Unavailable)?;
    check_first_run_proof(&unauth, proof)?;
    storage::set_initial_administrator_password(&unauth, &hash, actor).map_err(
        |(code, _)| {
            if code == 409 {
                PasswordError::AlreadySet
            } else {
                PasswordError::Unavailable
            }
        },
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginError {
    Input,
    NotSet,
    WrongPassword { retry_after_ms: i64, consecutive_failures: i64 },
    Throttled { retry_after_ms: i64, consecutive_failures: i64 },
    Busy,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChangePasswordError {
    Input,
    NotSet,
    WrongCurrent,
    Revoked,
    Busy,
    Unavailable,
}

fn current_token_digest(context: &RuntimeContext) -> Result<Generation, ()> {
    let cfg = context.load_config().map_err(|_| ())?;
    let expected = checked_console_token(context, &cfg).map_err(|_| ())?;
    if expected.is_empty() {
        return Err(());
    }
    Ok(credential_digest(&expected))
}

pub(crate) fn auth_first_run_browser(
    context: &RuntimeContext,
    port: u16,
    proof: &str,
    password: &str,
) -> Result<IssuedSession, PasswordError> {
    validate_new_password(password)?;
    check_first_run_proof(context, proof)?;
    let hash = AdministratorPasswordHash::derive(password)?;
    let _credential = credential_mutation_gate(context).map_err(|_| PasswordError::Busy)?;
    let unauth = unauthenticated_context(context);
    let _config = crate::social_state::acquire_config_gate(&unauth)
        .map_err(|_| PasswordError::Unavailable)?;
    check_first_run_proof(&unauth, proof)?;
    let digest = current_token_digest(&unauth).map_err(|_| PasswordError::Unavailable)?;
    storage::set_initial_administrator_password_with_session(
        &unauth,
        &hash,
        "administrator",
        &canonical_origin(port),
        digest,
    )
    .map_err(|(code, _)| {
        if code == 409 {
            PasswordError::AlreadySet
        } else {
            PasswordError::Unavailable
        }
    })
}

pub(crate) fn auth_login(
    context: &RuntimeContext,
    port: u16,
    password: &str,
) -> Result<IssuedSession, LoginError> {
    use storage::{LoginChallenge, LoginOutcome};

    validate_password_maximum(password).map_err(|_| LoginError::Input)?;
    let mut attempted_clamp = false;
    let hash = loop {
        match storage::load_login_challenge(context, true).map_err(|_| LoginError::Unavailable)? {
            LoginChallenge::NotSet => return Err(LoginError::NotSet),
            LoginChallenge::Throttled {
                retry_after_ms,
                consecutive_failures,
                needs_clamp,
            } => {
                if !needs_clamp || attempted_clamp {
                    return Err(LoginError::Throttled {
                        retry_after_ms,
                        consecutive_failures,
                    });
                }
                attempted_clamp = true;
                let _credential = credential_mutation_gate(context).map_err(|_| LoginError::Busy)?;
                let unauth = unauthenticated_context(context);
                let _config = crate::social_state::acquire_config_gate(&unauth)
                    .map_err(|_| LoginError::Unavailable)?;
                let (failures, wait_ms) = storage::clamp_throttle_deadline(&unauth)
                    .map_err(|(code, _)| {
                        if code == 409 {
                            LoginError::NotSet
                        } else {
                            LoginError::Unavailable
                        }
                    })?;
                if wait_ms > 0 {
                    return Err(LoginError::Throttled {
                        retry_after_ms: wait_ms,
                        consecutive_failures: failures,
                    });
                }
            }
            LoginChallenge::Ready { hash } => break hash,
        }
    };
    let matched = hash.verify(password).map_err(|err| {
        if err == PasswordError::Input {
            LoginError::Input
        } else {
            LoginError::Unavailable
        }
    })?;

    let _credential = credential_mutation_gate(context).map_err(|_| LoginError::Busy)?;
    let unauth = unauthenticated_context(context);
    let _config = crate::social_state::acquire_config_gate(&unauth)
        .map_err(|_| LoginError::Unavailable)?;
    let digest = current_token_digest(&unauth).map_err(|_| LoginError::Unavailable)?;

    if !matched {
        let failure = storage::record_login_failure(&unauth).map_err(|(code, _)| {
            if code == 409 { LoginError::NotSet } else { LoginError::Unavailable }
        })?;
        return Err(if failure.throttled_before {
            LoginError::Throttled {
                retry_after_ms: failure.retry_after_ms,
                consecutive_failures: failure.consecutive_failures,
            }
        } else {
            LoginError::WrongPassword {
                retry_after_ms: failure.retry_after_ms,
                consecutive_failures: failure.consecutive_failures,
            }
        });
    }

    match storage::complete_login(&unauth, hash.as_str(), &canonical_origin(port), digest) {
        Ok(LoginOutcome::Issued(issued)) => Ok(issued),
        Ok(LoginOutcome::NotSet) => Err(LoginError::NotSet),
        Ok(LoginOutcome::PasswordChanged) => Err(LoginError::WrongPassword {
            retry_after_ms: 0,
            consecutive_failures: 0,
        }),
        Ok(LoginOutcome::Throttled { retry_after_ms, consecutive_failures }) => {
            Err(LoginError::Throttled { retry_after_ms, consecutive_failures })
        }
        Err(_) => Err(LoginError::Unavailable),
    }
}

pub(crate) fn require_password_locally(cfg: &serde_json::Value) -> bool {
    crate::settings::REQUIRE_PASSWORD_LOCALLY.switch(cfg)
}

pub(crate) fn auth_local_browser_auto_sign_in(
    context: &RuntimeContext,
    port: u16,
) -> Result<Option<(IssuedSession, ConsoleAuthority)>, (u16, String)> {
    let _credential = credential_mutation_gate(context)
        .map_err(|error| (503, error))?;
    let unauth = unauthenticated_context(context);
    let _config = crate::social_state::acquire_config_gate(&unauth)
        .map_err(crate::social_state::SocialError::into_http)?;
    let cfg = unauth
        .load_config()
        .map_err(|error| (503, error.to_string()))?;
    if require_password_locally(&cfg) {
        return Ok(None);
    }
    let expected = checked_console_token(&unauth, &cfg)?;
    if expected.is_empty() {
        return Ok(None);
    }
    let digest = credential_digest(&expected);
    let origin = canonical_origin(port);
    let issued = storage::issue_browser_session(&unauth, &origin, digest)?;
    let Some(admitted) =
        storage::admit_browser(&unauth, &origin, Some(digest), &[issued.id_digest], false)?
    else {
        return Ok(None);
    };
    Ok(Some((
        issued,
        ConsoleAuthority::Browser {
            token_digest: digest,
            grant: admitted.proof,
        },
    )))
}

pub(crate) fn auth_change_password(
    context: &RuntimeContext,
    port: u16,
    authority: &ConsoleAuthority,
    current_password: &str,
    new_password: &str,
) -> Result<Option<IssuedSession>, ChangePasswordError> {
    validate_password_maximum(current_password).map_err(|_| ChangePasswordError::Input)?;
    validate_new_password(new_password).map_err(|_| ChangePasswordError::Input)?;
    let storage::LoginChallenge::Ready { hash } =
        storage::load_login_challenge(context, false)
            .map_err(|_| ChangePasswordError::Unavailable)?
    else {
        return Err(ChangePasswordError::NotSet);
    };
    if !hash
        .verify(current_password)
        .map_err(|_| ChangePasswordError::Unavailable)?
    {
        return Err(ChangePasswordError::WrongCurrent);
    }
    let new_hash = AdministratorPasswordHash::derive(new_password)
        .map_err(|_| ChangePasswordError::Unavailable)?;

    let _credential = credential_mutation_gate(context).map_err(|_| ChangePasswordError::Busy)?;
    let _config = crate::social_state::acquire_config_gate(context).map_err(|err| {
        if matches!(err, crate::social_state::SocialError::Unauthorized(_)) {
            ChangePasswordError::Revoked
        } else {
            ChangePasswordError::Unavailable
        }
    })?;
    let cfg = context.load_config().map_err(|_| ChangePasswordError::Unavailable)?;
    authority.recheck(context, &cfg).map_err(|(code, _)| {
        if code == 401 { ChangePasswordError::Revoked } else { ChangePasswordError::Unavailable }
    })?;
    let origin = canonical_origin(port);
    let rotate = if matches!(authority, ConsoleAuthority::Browser { .. }) {
        let digest = current_token_digest(context).map_err(|_| ChangePasswordError::Unavailable)?;
        Some((origin.as_str(), digest))
    } else {
        None
    };
    storage::change_administrator_password(
        context,
        hash.as_str(),
        &new_hash,
        "administrator",
        rotate,
    )
    .map_err(|(code, _)| match code {
        409 => ChangePasswordError::NotSet,
        401 => ChangePasswordError::WrongCurrent,
        _ => ChangePasswordError::Unavailable,
    })
}

pub(crate) fn auth_logout(
    context: &RuntimeContext,
    authority: Option<&ConsoleAuthority>,
) -> Result<Option<[u8; 32]>, (u16, String)> {
    let _config = crate::social_state::acquire_config_gate(context)
        .map_err(crate::social_state::SocialError::into_http)?;
    let Some(ConsoleAuthority::Browser { grant, .. }) = authority else {
        return Ok(None);
    };
    storage::revoke_browser_session_by_digest(context, grant.id_digest)?;
    Ok(Some(grant.id_digest))
}

pub(crate) fn list_browser_sessions(
    context: &RuntimeContext,
    authority: &ConsoleAuthority,
) -> Result<Vec<BrowserSessionView>, (u16, String)> {
    let (token_digest, current_id) = match authority {
        ConsoleAuthority::Browser { token_digest, grant } => (*token_digest, Some(grant.id_digest)),
        ConsoleAuthority::Machine { token_digest }
        | ConsoleAuthority::Desktop { token_digest, .. } => (*token_digest, None),
    };
    storage::list_browser_sessions_raw(context, token_digest, current_id)
}

pub(crate) enum RevokeTarget {
    One([u8; 32]),
    Others,
    All,
}

pub(crate) struct RevokeOutcome {
    pub(crate) revoked_count: usize,
    pub(crate) delete_own: Option<[u8; 32]>,
}

pub(crate) fn revoke_browser_sessions_authorized(
    context: &RuntimeContext,
    authority: &ConsoleAuthority,
    target: RevokeTarget,
) -> Result<RevokeOutcome, (u16, String)> {
    if matches!(target, RevokeTarget::Others)
        && !matches!(authority, ConsoleAuthority::Browser { .. })
    {
        return Err((
            400,
            "仅当前浏览器会话支持撤销其他会话，撤销全部请传 all: true".to_string(),
        ));
    }
    let _config = crate::social_state::acquire_config_gate(context)
        .map_err(crate::social_state::SocialError::into_http)?;
    let cfg = context
        .load_config()
        .map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    authority.recheck(context, &cfg)?;
    let (revoked_count, delete_own) = match (target, authority) {
        (RevokeTarget::One(digest), _) => {
            let removed = storage::revoke_browser_session_by_digest(context, digest)?;
            let own = matches!(
                authority,
                ConsoleAuthority::Browser { grant, .. }
                    if digests_equal(&grant.id_digest, &digest)
            )
            .then_some(digest);
            (usize::from(removed), own)
        }
        (RevokeTarget::Others, ConsoleAuthority::Browser { grant, .. }) => (
            storage::revoke_other_browser_sessions(context, Some(grant.id_digest))?,
            None,
        ),
        (RevokeTarget::Others, _) => {
            return Err((
                400,
                "仅当前浏览器会话支持撤销其他会话，撤销全部请传 all: true".to_string(),
            ));
        }
        (RevokeTarget::All, _) => {
            let count = storage::revoke_other_browser_sessions(context, None)?;
            let own = match authority {
                ConsoleAuthority::Browser { grant, .. } => Some(grant.id_digest),
                _ => None,
            };
            (count, own)
        }
    };
    Ok(RevokeOutcome {
        revoked_count,
        delete_own,
    })
}

fn read_input(
    input: &mut impl BufRead,
    terminal: bool,
    prompt: &str,
) -> Result<String, PasswordError> {
    if terminal {
        return rpassword::prompt_password(prompt).map_err(|_| PasswordError::Input);
    }
    let mut framed = Vec::new();
    Read::take(input, MAX_STDIN_LINE_BYTES)
        .read_until(b'\n', &mut framed)
        .map_err(|_| PasswordError::Input)?;
    if framed.pop() != Some(b'\n') {
        return Err(PasswordError::Input);
    }
    if framed.last() == Some(&b'\r') {
        framed.pop();
    }
    String::from_utf8(framed).map_err(|_| PasswordError::Input)
}

fn set_password_cli(context: &RuntimeContext) -> Result<(), PasswordError> {
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut input = stdin.lock();
    let proof = read_input(&mut input, terminal, "Console token: ")?;
    let password = read_input(&mut input, terminal, "New administrator password: ")?;
    let confirm = read_input(&mut input, terminal, "Repeat administrator password: ")?;
    if password != confirm {
        return Err(PasswordError::Input);
    }
    first_run(context, &proof, &password, "her-cli")
}

fn reset_password_cli(context: &RuntimeContext) -> Result<(), PasswordError> {
    let _credential = credential_mutation_gate(context).map_err(|_| PasswordError::Busy)?;
    let _config = crate::social_state::acquire_config_gate(context)
        .map_err(|_| PasswordError::Unavailable)?;
    ensure_credentials_ready(context).map_err(|_| PasswordError::Unavailable)?;
    storage::reset_administrator_password(context).map_err(|_| PasswordError::Unavailable)
}

pub(crate) fn run_cli(args: &[String]) -> i32 {
    let usage = || {
        eprintln!(
            "usage: her-cli console set-password|reset-password --runtime-root <absolute path>"
        );
        2
    };
    let [verb, flag, root] = args else {
        return usage();
    };
    let root = Path::new(root);
    if !matches!(verb.as_str(), "set-password" | "reset-password")
        || flag != "--runtime-root"
        || !root.is_absolute()
    {
        return usage();
    }
    // from_root also requires a release image when the selected root is in the install root.
    let Ok(context) = RuntimeContext::from_root(root) else {
        eprintln!("selected runtime is unavailable");
        return 3;
    };
    let result = match verb.as_str() {
        "set-password" => set_password_cli(&context),
        _ => reset_password_cli(&context),
    };
    match result {
        Ok(()) => {
            println!("{{\"ok\":true}}");
            0
        }
        Err(error) => {
            let (code, message) = match error {
                PasswordError::Input => (2, "invalid password input or confirmation"),
                PasswordError::Proof => (3, "console credential proof was refused"),
                PasswordError::AlreadySet => (3, "administrator password is already set"),
                PasswordError::Busy => (3, "console credential mutation is busy or unavailable"),
                PasswordError::Unavailable => (3, "administrator password storage is unavailable"),
            };
            eprintln!("{message}");
            code
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runtime::tests::{runtime_fixture, FixtureRoot};
    use crate::settings::StoreProbe;
    use serde_json::{json, Value};
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::Duration;

    const KEY: &str = "console.administratorPassword";
    const TOKEN: &str = "fixture-console-auth-machine";
    const PASSWORD: &str = "  初始密码样本  ";

    pub(crate) fn supported_phc() -> String {
        format!(
            "$argon2id$v=19$m=19456,t=2,p=1${}${}",
            "A".repeat(22),
            "A".repeat(43)
        )
    }

    pub(crate) fn runtime(generated: bool) -> (FixtureRoot, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-console-auth-{}-{}",
            std::process::id(),
            generate_console_token().unwrap(),
        )));
        let root = runtime_fixture(
            root,
            &json!({"consoleToken": TOKEN}),
            &json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:1",
                "onebot": "http://127.0.0.1:1",
            }),
        );
        if generated {
            std::fs::write(root.0.join("data/console-token"), TOKEN).unwrap();
            std::fs::write(root.0.join("config/config.json"), r#"{"consoleToken":""}"#).unwrap();
        }
        let context = RuntimeContext::from_root(&root.0).unwrap();
        (root, context)
    }

    #[test]
    fn console_auth_generation_refuses_redirected_and_malformed_files() {
        let (root, context) = runtime(false);
        let _config = crate::social_state::acquire_config_gate(&context).unwrap();
        let marker = root.0.join("data").join(RESTORE_GENERATION_FILE);
        assert_eq!(restore_generation(&context).unwrap(), None);

        for len in [0, 31, 33] {
            fs::write(&marker, vec![3; len]).unwrap();
            assert!(restore_generation(&context).is_err());
        }
        fs::remove_file(&marker).unwrap();
        let target = root.0.join("generation-target");
        fs::write(&target, [7; 32]).unwrap();
        fs::hard_link(&target, &marker).unwrap();
        assert!(restore_generation(&context).is_err());
        assert!(matches!(
            rotate_restore_generation(&context),
            Err(GenerationWriteError::Unpublished(_))
        ));
        assert_eq!(fs::read(&target).unwrap(), [7; 32]);
        fs::remove_file(&marker).unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &marker).unwrap();
            assert!(restore_generation(&context).is_err());
            assert!(matches!(
                rotate_restore_generation(&context),
                Err(GenerationWriteError::Unpublished(_))
            ));
            assert_eq!(fs::read(&target).unwrap(), [7; 32]);
            fs::remove_file(&marker).unwrap();
        }
        fs::create_dir(&marker).unwrap();
        assert!(restore_generation(&context).is_err());
        assert!(matches!(
            rotate_restore_generation(&context),
            Err(GenerationWriteError::Unpublished(_))
        ));
        fs::remove_dir(&marker).unwrap();

        let generation = rotate_restore_generation(&context).unwrap();
        assert_eq!(restore_generation(&context).unwrap(), Some(generation));
    }

    fn entry(context: &RuntimeContext) -> Value {
        crate::settings::view(context).unwrap()["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["key"] == KEY)
            .unwrap()
            .clone()
    }

    #[test]
    fn administrator_password_is_private_and_audited() {
        let (root, context) = runtime(false);
        let probe = StoreProbe::at(&root.0);
        let config = root.0.join("config/config.json");
        let before = std::fs::read(&config).unwrap();
        first_run(&context, TOKEN, PASSWORD, "her-cli").unwrap();
        let row = probe.value(KEY).expect("a credential row must exist");
        let phc: String = serde_json::from_str(&row).unwrap();
        let stored = AdministratorPasswordHash::parse(&phc)
            .expect("the stored row must contain a supported PHC");
        assert!(stored.verify(PASSWORD).unwrap());
        assert!(!stored.verify(PASSWORD.trim()).unwrap());
        assert_eq!(entry(&context)["isSet"], true);
        assert_eq!(entry(&context)["managedBy"], "consoleAuth");
        let history = probe.audit_rows();
        assert_eq!(history.len(), 1);
        let audit = &history[0];
        assert!(
            audit["old"] == "\"(hidden)\"" && audit["new"] == "\"(hidden)\"",
            "both raw settings audit fields must be fixed markers"
        );
        assert!(!audit.to_string().contains(PASSWORD));
        assert!(!audit.to_string().contains(&phc));
        assert_eq!(audit["actor"], "her-cli");
        assert_eq!(audit["source"], "first-run");
        assert!(
            std::fs::read(config).unwrap() == before,
            "password set must not change config.json"
        );
        assert!(context
            .load_config_document()
            .unwrap()
            .pointer("/console/administratorPassword")
            .is_none());
        let mut pending = json!({});
        crate::settings::mirror_into(&context, &mut pending).unwrap();
        assert!(pending.pointer("/console/administratorPassword").is_none());
        let page = entry(&context);
        assert!(!page.to_string().contains(&phc));
        for field in ["value", "stored", "default"] {
            assert!(page.get(field).is_none());
        }

        let _config = crate::social_state::acquire_config_gate(&context).unwrap();
        let batch = vec![(&crate::settings::ADMINISTRATOR_PASSWORD, Value::Null)];
        let result = crate::settings::apply_changes(&context, batch, "administrator", "console");
        assert!(matches!(result, Err((409, _))));
        let result = crate::settings::undo(&context, 1, "administrator", &|_| Ok(()));
        assert!(matches!(result, Err((409, _))));
        assert_eq!(probe.audit_count(), 1);
        assert!(
            probe.value(KEY).as_ref() == Some(&row),
            "Settings cannot replace or undo a console credential"
        );
    }

    #[test]
    fn administrator_password_counts_unicode_characters() {
        assert_eq!(
            validate_new_password(&"密".repeat(7)),
            Err(PasswordError::Input),
            "seven Unicode characters must be refused"
        );
        for password in ["密码密码密码密码", "        ", &"密".repeat(1024)] {
            assert_eq!(validate_new_password(password), Ok(()));
        }
        assert_eq!(
            validate_new_password(&"密".repeat(1025)),
            Err(PasswordError::Input)
        );
    }

    #[test]
    fn overlength_password_skips_password_work() {
        let (root, context) = runtime(false);
        let calls = Cell::new(0);
        let result =
            first_run_with_password_work(&context, TOKEN, &"密".repeat(1025), "her-cli", |_| {
                calls.set(calls.get() + 1);
                Err(PasswordError::Unavailable)
            });
        assert_eq!(
            calls.get(),
            0,
            "overlength input must perform zero Argon2 calls"
        );
        assert_eq!(result, Err(PasswordError::Input));
        assert!(!StoreProbe::at(&root.0).path().exists());
    }

    #[test]
    fn administrator_password_write_rolls_back_on_audit_failure() {
        let (root, context) = runtime(false);
        let probe = StoreProbe::at(&root.0);
        crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
        let result = first_run(&context, TOKEN, PASSWORD, "her-cli");
        assert!(
            probe.value(KEY).is_none(),
            "the failed audit must roll back the new hash"
        );
        assert_eq!(probe.audit_count(), 0);
        assert_eq!(result, Err(PasswordError::Unavailable));

        first_run(&context, TOKEN, PASSWORD, "her-cli").unwrap();
        let before = probe.value(KEY);
        crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
        {
            let _config = crate::social_state::acquire_config_gate(&context).unwrap();
            assert!(reset_administrator_password(&context).is_err());
        }
        assert!(
            probe.value(KEY) == before,
            "the failed audit must roll back the deletion"
        );
        assert_eq!(probe.audit_count(), 1);
    }

    #[test]
    fn first_run_refuses_file_loss_after_password_work() {
        let (root, context) = runtime(true);
        let timeout = Duration::from_secs(10);
        let (hashed, wait_for_hash) = mpsc::channel();
        let (resume, wait_for_removal) = mpsc::channel();
        let result = std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                first_run_with_password_work(&context, TOKEN, PASSWORD, "her-cli", |password| {
                    let hash = AdministratorPasswordHash::derive(password)?;
                    hashed.send(()).unwrap();
                    wait_for_removal.recv_timeout(timeout).unwrap();
                    Ok(hash)
                })
            });
            wait_for_hash
                .recv_timeout(timeout)
                .expect("first-run must reach the post-hash barrier");
            let removed = std::fs::remove_file(root.0.join("data/console-token"));
            resume.send(()).unwrap();
            removed.unwrap();
            writer.join().unwrap()
        });
        assert_eq!(
            result,
            Err(PasswordError::Proof),
            "first-run must recheck proof after password work"
        );
        assert!(
            !StoreProbe::at(&root.0).path().exists(),
            "proof loss must leave no hash or audit row"
        );
    }

    #[test]
    fn administrator_password_rejects_unsupported_phcs() {
        let supported = supported_phc();
        assert!(AdministratorPasswordHash::parse(&supported).is_some());
        for invalid in [
            supported.replace("argon2id", "argon2i"),
            supported.replace("v=19", "v=16"),
            supported.replace("m=19456", "m=65536"),
            supported.replace("t=2", "t=3"),
            supported.replace("p=1", "p=2"),
            supported.replace("p=1", "p=1,extra=2"),
            supported.replace(&"A".repeat(22), &"A".repeat(21)),
            supported.replace(&"A".repeat(43), &"A".repeat(42)),
        ] {
            assert!(
                AdministratorPasswordHash::parse(&invalid).is_none(),
                "unsupported PHC accepted"
            );
        }
        let (root, context) = runtime(false);
        first_run(&context, TOKEN, "        ", "her-cli").unwrap();
        let probe = StoreProbe::at(&root.0);
        let phc: String = serde_json::from_str(&probe.value(KEY).unwrap()).unwrap();
        let hash = AdministratorPasswordHash::parse(&phc).unwrap();
        assert!(hash.verify("        ").unwrap());
        assert_eq!(hash.verify(&"密".repeat(1025)), Err(PasswordError::Input));
        for malformed in ["null", "\"invalid PHC\""] {
            probe.set_value(KEY, malformed);
            let view = crate::settings::view(&context).unwrap();
            assert_eq!(view["storeError"], "administrator password is unavailable");
            assert_eq!(entry(&context)["isSet"], false);
            assert_eq!(
                first_run(&context, TOKEN, PASSWORD, "her-cli"),
                Err(PasswordError::AlreadySet)
            );
        }
    }

    #[test]
    fn console_auth_session_management_and_backup_scrub_preserve_contract() {
        let (root, context) = runtime(false);
        let port = 3999;
        let issued = auth_first_run_browser(&context, port, TOKEN, PASSWORD).unwrap();
        let origin = canonical_origin(port);
        let token_digest = credential_digest(TOKEN);
        let admitted =
            admit_browser(&context, &origin, Some(token_digest), &[issued.id_digest], false)
                .unwrap()
                .expect("first-run browser session must admit");
        let authority = ConsoleAuthority::Browser {
            token_digest,
            grant: admitted.proof,
        };
        let status = auth_status(&context).unwrap();
        assert!(status.password_set);
        assert_eq!(status.consecutive_failures, 0);

        let second = auth_login(&context, port, PASSWORD).unwrap();
        let listed = list_browser_sessions(&context, &authority).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed.iter().filter(|item| item.current).count(), 1);

        let revoke_other = revoke_browser_sessions_authorized(
            &context,
            &authority,
            RevokeTarget::One(second.id_digest),
        )
        .unwrap();
        assert_eq!(revoke_other.revoked_count, 1);
        assert!(revoke_other.delete_own.is_none());

        let third = auth_login(&context, port, PASSWORD).unwrap();
        let revoke_others =
            revoke_browser_sessions_authorized(&context, &authority, RevokeTarget::Others).unwrap();
        assert_eq!(revoke_others.revoked_count, 1);
        assert!(admit_browser(&context, &origin, Some(token_digest), &[third.id_digest], false)
            .unwrap()
            .is_none());

        assert_eq!(
            auth_logout(&context, Some(&authority)).unwrap(),
            Some(issued.id_digest)
        );
        assert!(admit_browser(&context, &origin, Some(token_digest), &[issued.id_digest], false)
            .unwrap()
            .is_none());
        let err = revoke_browser_sessions_authorized(&context, &authority, RevokeTarget::All)
            .err()
            .expect("revoked authority must fail recheck");
        assert_eq!(err.0, 401);

        let machine = admit_explicit_authority(&context, TOKEN, TOKEN, true)
            .unwrap()
            .expect("raw machine token admitted");
        assert!(matches!(machine, ConsoleAuthority::Machine { .. }));
        assert!(admit_explicit_authority(&context, TOKEN, TOKEN, false).unwrap().is_none());

        // Snapshot scrubbing must drop browser sessions and auth state while keeping the password.
        let snapshot = root.0.join("snapshot-settings.db");
        {
            let src = crate::settings::backup_source(&context).unwrap().unwrap();
            let mut dst = rusqlite::Connection::open(&snapshot).unwrap();
            let backup = rusqlite::backup::Backup::new(&src, &mut dst).unwrap();
            backup.run_to_completion(100, std::time::Duration::ZERO, None).unwrap();
        }
        let check = || Ok(());
        scrub_auth_snapshot(&snapshot, &check).unwrap();
        let scrubbed = rusqlite::Connection::open(&snapshot).unwrap();
        let archived = archive_credentials(&scrubbed).unwrap();
        assert!(archived.has_password);
        assert!(!archived.has_endpoints);
        let session_rows: i64 = scrubbed
            .query_row("SELECT count(*) FROM console_browser_sessions", [], |r| r.get(0))
            .unwrap();
        let state_rows: i64 = scrubbed
            .query_row("SELECT count(*) FROM console_auth_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!((session_rows, state_rows), (0, 0));
    }

    #[test]
    fn console_auth_rotation_clears_desktop_grants_and_browser_sessions() {
        let (root, context) = runtime(false);
        let port = 3999;
        let issued = auth_first_run_browser(&context, port, TOKEN, PASSWORD).unwrap();
        let grants_path = root.0.join("data").join(DESKTOP_GRANTS_FILE);
        fs::write(&grants_path, b"{\"v\":1}").unwrap();
        let rotated_token = generate_console_token().unwrap();
        {
            let _cred = credential_mutation_gate(&context).unwrap();
            let _cfg = crate::social_state::acquire_config_gate(&context).unwrap();
            rotate_console_token(&context, &rotated_token, true).unwrap();
        }
        assert!(!grants_path.exists(), "rotation must remove desktop grants");
        assert!(admit_browser(
            &context,
            &canonical_origin(port),
            Some(credential_digest(&rotated_token)),
            &[issued.id_digest],
            false,
        )
        .unwrap()
        .is_none());

        let stale_proof = DesktopProof {
            bearer_digest: [1; 32],
            generation: [2; 32],
            issuer_pid: 1,
            issuer_start: 1,
        };
        assert!(
            !recheck_desktop(&context, &stale_proof, &credential_digest(&rotated_token)).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn desktop_grants_registry_rejects_malformed_and_expired_entries() {
        struct TickGuard;
        impl Drop for TickGuard {
            fn drop(&mut self) {
                desktop_grants::TEST_TICKS.with(|cell| cell.set(None));
            }
        }
        let _guard = TickGuard;
        let (root, context) = runtime(false);
        assert!(desktop_grants::read(&context).unwrap().is_none());

        let token_digest = credential_digest(TOKEN);
        let bearer = generate_console_token().unwrap();
        let bearer_digest = credential_digest(&bearer);
        let registry = desktop_grants::Registry {
            v: 1,
            generation: [9; 32],
            issuer_pid: std::process::id(),
            issuer_start: 42,
            grants: std::collections::VecDeque::from([desktop_grants::Grant {
                bearer_digest,
                token_digest,
                issued: 1000,
                expires: 1000 + desktop_grants::LIFETIME,
            }]),
        };
        context
            .write_atomic(
                RuntimeArea::Data,
                DESKTOP_GRANTS_FILE,
                &serde_json::to_vec(&registry).unwrap(),
            )
            .unwrap();
        let parsed = desktop_grants::read(&context).unwrap().unwrap();
        assert_eq!(parsed.grants.len(), 1);
        desktop_grants::TEST_TICKS.with(|c| c.set(Some(1000 + desktop_grants::LIFETIME)));
        assert_eq!(desktop_grants::ticks(), 1000 + desktop_grants::LIFETIME);

        let mut bad = registry;
        bad.grants[0].expires += 1;
        context
            .write_atomic(
                RuntimeArea::Data,
                DESKTOP_GRANTS_FILE,
                &serde_json::to_vec(&bad).unwrap(),
            )
            .unwrap();
        assert!(desktop_grants::read(&context).is_err());
        let _ = root;
    }
}
