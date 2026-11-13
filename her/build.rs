fn main() {
    for name in ["CARGO_FEATURE_RELEASE_TEST_KEYS", "PROFILE", "OPT_LEVEL"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if std::env::var_os("CARGO_FEATURE_RELEASE_TEST_KEYS").is_some()
        && (std::env::var("PROFILE").as_deref() == Ok("release")
            || std::env::var("OPT_LEVEL").as_deref() != Ok("0"))
    {
        panic!("release-test-keys is restricted to unoptimized debug fixture builds; never ship test keys");
    }
}
