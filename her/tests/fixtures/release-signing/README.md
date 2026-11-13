# Synthetic release-signing keys

These four Minisign key pairs are public test data, never owner/release credentials. `.pub` files contain the raw base64 Minisign public key. `.key` files contain the base64 serialized unencrypted `minisign::SecretKey` consumed by the test signer. `primary` and `backup` are compiled only in unit tests or debug fixture builds with `release-test-keys`; `unknown` is never pinned. `rotated` exercises a replacement primary with the backup retained.

Production pins live in `src/release_signature.rs`. Their private halves were discarded without storage. Replace both placeholders with the owner's real public keys before v0.1.0; keep the owner's private keys offline.

`legacy.minisig` authenticates the literal bytes `legacy signed checksum bytes\r\n` with the synthetic primary key in legacy `Ed` (non-prehashed) mode. Generated with Python cryptography's Ed25519 implementation; the global signature covers the raw signature plus the trusted comment. The fixture exercises both signature layers and rejects altered input.
