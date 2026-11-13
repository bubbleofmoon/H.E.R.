//! Synthetic Minisign private keys only. Never use these as release trust anchors.
use base64::Engine;

pub fn sign(bytes: &[u8], key: &str) -> Vec<u8> {
    let secret = match key {
        "primary" => include_str!("../fixtures/release-signing/primary.key"),
        "backup" => include_str!("../fixtures/release-signing/backup.key"),
        "rotated" => include_str!("../fixtures/release-signing/rotated.key"),
        "unknown" => include_str!("../fixtures/release-signing/unknown.key"),
        _ => panic!("unknown synthetic signing key"),
    };
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret.trim())
        .unwrap();
    let secret = minisign::SecretKey::from_bytes(&secret).unwrap();
    minisign::sign(
        None,
        &secret,
        std::io::Cursor::new(bytes),
        Some("HER synthetic release"),
        None,
    )
    .unwrap()
    .to_string()
    .into_bytes()
}

pub fn keys(primary: &str) -> String {
    let primary = match primary {
        "primary" => include_str!("../fixtures/release-signing/primary.pub"),
        "rotated" => include_str!("../fixtures/release-signing/rotated.pub"),
        _ => panic!("unknown fixture primary"),
    };
    format!(
        "# HER-KEYS {} {}\n",
        primary.trim(),
        include_str!("../fixtures/release-signing/backup.pub").trim()
    )
}
