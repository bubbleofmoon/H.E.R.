//! One compiled trust anchor for the app, CLI and fixed updater.
use minisign_verify::{PublicKey, Signature};

pub(crate) const SIGNATURE_NAME: &str = "SHA256SUMS.minisig";
pub(crate) const MAX_SIGNATURE_BYTES: usize = 4096;
pub(crate) const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
pub(crate) const REFUSED: &str = "release checksum signature missing, invalid or untrusted";

// REPLACE BEFORE v0.1.0 with the owner's offline primary and backup public keys.
// These are disposable public keys. Their private halves were discarded without storage.
#[cfg(any(test, not(feature = "release-test-keys")))]
const PLACEHOLDER_PRIMARY_REPLACE_BEFORE_V0_1_0: &str =
    "RWQTa6tD0NjcOeAbnrC3D+Fu0ltOpen4f3UOvSCEAiwIy1ZfuYa2Ym4q";
#[cfg(any(test, not(feature = "release-test-keys")))]
const PLACEHOLDER_BACKUP_REPLACE_BEFORE_V0_1_0: &str =
    "RWRnmVNxc6T1QsBywSaaJswThYLVAV/CI+DKOuBYNT46Iuvoiutjl5wX";

#[cfg(all(feature = "release-test-keys", not(debug_assertions)))]
compile_error!("release-test-keys is restricted to debug fixture builds; never ship test keys");

#[cfg(any(test, feature = "release-test-keys"))]
const PINNED: [&str; 2] = [
    include_str!("../tests/fixtures/release-signing/primary.pub"),
    include_str!("../tests/fixtures/release-signing/backup.pub"),
];
#[cfg(not(any(test, feature = "release-test-keys")))]
const PINNED: [&str; 2] = [
    PLACEHOLDER_PRIMARY_REPLACE_BEFORE_V0_1_0,
    PLACEHOLDER_BACKUP_REPLACE_BEFORE_V0_1_0,
];

/// Authenticate the exact received bytes before parsing or trusting any checksum.
#[cfg(test)]
pub(crate) fn verify(checksum: &[u8], signature: &[u8]) -> Result<(), String> {
    verify_with_keys(checksum, signature, &PINNED)
}

pub(crate) fn verify_with_keys(
    checksum: &[u8],
    signature: &[u8],
    keys: &[impl AsRef<str>],
) -> Result<(), String> {
    if checksum.len() > MAX_CHECKSUM_BYTES || signature.len() > MAX_SIGNATURE_BYTES {
        return Err(REFUSED.into());
    }
    let text = std::str::from_utf8(signature).map_err(|_| REFUSED)?;
    if text.lines().count() != 4 || !text.starts_with("untrusted comment: ") {
        return Err(REFUSED.into());
    }
    let signature = Signature::decode(text).map_err(|_| REFUSED)?;
    if keys.iter().any(|key| {
        PublicKey::from_base64(key.as_ref().trim())
            // Both Minisign modes authenticate the exact file; legacy mode is Ed25519 too.
            .is_ok_and(|key| key.verify(checksum, &signature, true).is_ok())
    }) {
        Ok(())
    } else {
        Err(REFUSED.into())
    }
}

/// Identifies the compiled bootstrap authority for an empty installation.
pub(crate) fn trust_id() -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(PINNED.map(str::trim).join("\n").as_bytes())
    )
}

pub(crate) fn pinned_keys() -> [String; 2] {
    PINNED.map(|key| key.trim().to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub generation: u64,
    pub keys: [String; 2],
}

/// Both identity and the next trust set are inside the exact signed bytes.
pub(crate) fn identity(checksum: &[u8], tag: &str) -> Result<Identity, String> {
    let text = std::str::from_utf8(checksum).map_err(|_| REFUSED)?;
    let text = text.trim_start_matches('\u{feff}');
    let mut headers = text
        .lines()
        .filter_map(|line| line.strip_prefix("# HER-RELEASE "));
    let (authenticated_tag, generation) = headers
        .next()
        .ok_or(REFUSED)?
        .split_once(' ')
        .ok_or(REFUSED)?;
    let generation: u64 = generation.parse().map_err(|_| REFUSED)?;
    if headers.next().is_some()
        || authenticated_tag != tag
        || generation == 0
        || crate::release_store::version(authenticated_tag).is_none()
    {
        return Err(REFUSED.into());
    }
    let mut sets = text
        .lines()
        .filter_map(|line| line.strip_prefix("# HER-KEYS "));
    let keys: [String; 2] = sets
        .next()
        .ok_or(REFUSED)?
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| REFUSED)?;
    if sets.next().is_some()
        || keys[0] == keys[1]
        || keys.iter().any(|key| PublicKey::from_base64(key).is_err())
    {
        return Err(REFUSED.into());
    }
    Ok(Identity { generation, keys })
}

#[cfg(test)]
#[path = "../tests/support/release_signing.rs"]
mod signing;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_and_backup_authenticate_exact_bytes_including_bom_and_crlf() {
        let sums = b"\xef\xbb\xbfexact checksum bytes\r\n";
        for key in ["primary", "backup"] {
            let signature = signing::sign(sums, key);
            verify(sums, &signature).unwrap();
            assert_eq!(
                verify(b"exact checksum bytes\n", &signature),
                Err(REFUSED.into())
            );
        }
    }

    #[test]
    fn unknown_key_cannot_impersonate_a_pinned_key_id() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let sums = b"checksum bytes";
        let signature = String::from_utf8(signing::sign(sums, "unknown")).unwrap();
        let mut lines = signature.lines().map(str::to_owned).collect::<Vec<_>>();
        let mut packet = STANDARD.decode(&lines[1]).unwrap();
        let primary = STANDARD.decode(PINNED[0].trim()).unwrap();
        packet[2..10].copy_from_slice(&primary[2..10]);
        lines[1] = STANDARD.encode(packet);
        assert_eq!(
            verify(sums, lines.join("\n").as_bytes()),
            Err(REFUSED.into())
        );
    }

    #[test]
    fn valid_primary_signature_with_only_key_id_changed_is_refused() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let sums = b"checksum bytes";
        let signature = signing::sign(sums, "primary");
        verify(sums, &signature).unwrap();
        let text = std::str::from_utf8(&signature).unwrap();
        let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
        let mut packet = STANDARD.decode(&lines[1]).unwrap();
        packet[2] ^= 1;
        lines[1] = STANDARD.encode(packet);
        assert_eq!(
            verify(sums, lines.join("\n").as_bytes()),
            Err(REFUSED.into())
        );
    }

    #[test]
    fn legacy_non_prehashed_ed25519_signatures_authenticate_exact_bytes() {
        let signature = include_bytes!("../tests/fixtures/release-signing/legacy.minisig");
        verify(b"legacy signed checksum bytes\r\n", signature).unwrap();
        assert_eq!(
            verify(b"legacy signed checksum bytes\n", signature),
            Err(REFUSED.into())
        );
    }

    #[test]
    fn fixture_signatures_never_authorize_production_placeholders() {
        let sums = b"checksum bytes";
        for key in ["primary", "backup", "rotated", "unknown"] {
            assert_eq!(
                verify_with_keys(
                    sums,
                    &signing::sign(sums, key),
                    &[
                        PLACEHOLDER_PRIMARY_REPLACE_BEFORE_V0_1_0,
                        PLACEHOLDER_BACKUP_REPLACE_BEFORE_V0_1_0,
                    ]
                ),
                Err(REFUSED.into())
            );
        }
    }

    #[test]
    fn malformed_or_oversized_signature_and_changed_trusted_comment_refuse_safely() {
        let sums = b"checksum bytes";
        let valid = signing::sign(sums, "primary");
        let mut changed_comment = String::from_utf8(valid.clone()).unwrap();
        changed_comment =
            changed_comment.replace("HER synthetic release", "untrusted attacker text");
        for signature in [
            Vec::new(),
            vec![0xff; 100],
            vec![b'x'; MAX_SIGNATURE_BYTES + 1],
            [valid, b"extra line\n".to_vec()].concat(),
            changed_comment.into_bytes(),
        ] {
            assert_eq!(verify(sums, &signature), Err(REFUSED.into()));
        }
    }
}
