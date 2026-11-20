//! Console credential comparison and the selected runtime's offline password commands.

use crate::runtime::RuntimeContext;
use argon2::password_hash::{
    Error as PhcError, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use sha2::{Digest, Sha256};
use std::io::{BufRead, IsTerminal, Read};
use std::path::Path;
use subtle::ConstantTimeEq;

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
    for (value, pair) in digest.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *value = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(digest)
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

    // Used by browser login and current-password verification.
    #[allow(dead_code)]
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
    let expected = context.effective_console_token(&cfg);
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
    if !context.is_isolated() {
        return Err(PasswordError::Unavailable);
    }
    validate_new_password(password)?;
    check_first_run_proof(context, proof)?;
    let hash = derive(password)?;
    let _config = crate::console::config_gate(context).map_err(|_| PasswordError::Unavailable)?;
    // Reuse the proof from before password work. RED-M9
    crate::settings::set_initial_administrator_password(context, &hash, actor).map_err(
        |(code, _)| {
            if code == 409 {
                PasswordError::AlreadySet
            } else {
                PasswordError::Unavailable
            }
        },
    )
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
    let _config = crate::console::config_gate(context).map_err(|_| PasswordError::Unavailable)?;
    crate::settings::reset_administrator_password(context).map_err(|_| PasswordError::Unavailable)
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

    fn runtime(generated: bool) -> (FixtureRoot, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-console-auth-{}-{}",
            std::process::id(),
            crate::console::generate_console_token().unwrap(),
        )));
        let root = runtime_fixture(
            root,
            &json!({"consoleToken": TOKEN}),
            &json!({
                "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:1", "onebot":"http://127.0.0.1:1"
            }),
        );
        if generated {
            std::fs::write(root.0.join("data/console-token"), TOKEN).unwrap();
            std::fs::write(root.0.join("config/config.json"), r#"{"consoleToken":""}"#).unwrap();
        }
        let context = RuntimeContext::from_root(&root.0).unwrap();
        (root, context)
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

        let _config = crate::console::config_gate(&context).unwrap();
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
            let _config = crate::console::config_gate(&context).unwrap();
            assert!(crate::settings::reset_administrator_password(&context).is_err());
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
}
