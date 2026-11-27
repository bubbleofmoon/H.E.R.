use super::{mint_cookie_at, now_ms, parse_session_secret, session_secret_from_text, Credential};
use crate::dsh::Rpc;
use crate::runtime::RuntimeContext;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires the isolated DSH fixture from scripts/ci/dsh-contract.py"]
fn settings_describe_cookie_contract() {
    let base = std::env::var("HER_DSH_CONTRACT_URL").expect("start the isolated DSH fixture");
    let home = std::env::var_os("DSH_HOME").expect("fixture DSH home");
    let text = std::fs::read_to_string(std::path::Path::new(&home).join(".credentials.yaml"))
        .expect("read the fixture's generated credentials");
    let encoded = session_secret_from_text(&text).expect("fixture session secret");
    assert!(!encoded.is_empty());
    let secret = parse_session_secret(&text).expect("decode the fixture session secret");

    let fixture = crate::runtime::tests::runtime_fixture(
        std::env::temp_dir().join(format!("her-dsh-contract-{}", std::process::id())),
        &json!({"consoleToken": "fixture-dsh-contract-token"}),
        &json!({"dsh": base, "model": "http://127.0.0.1:1", "onebot": "http://127.0.0.1:1"}),
    );
    let context = RuntimeContext::from_root(&fixture.0).expect("contract fixture runtime");
    let minted = mint_cookie_at(&base, &secret, now_ms()).expect("mint the bridge cookie");
    let rpc = Rpc::from_credential(&base, Credential::fixed_in(&context, &base, minted.clone()));
    // Use the same signer and authority. Advance issuance past the bridge's one-minute backdate.
    let future = mint_cookie_at(&base, &secret, now_ms() + 5 * 60 * 1000)
        .expect("mint a signed future-dated cookie");
    let cookies = [minted, future];
    let describe = || {
        rpc.call_with_deadline(
            "settings/describe",
            json!({}),
            Instant::now() + Duration::from_secs(5),
        )
    };

    rpc.credential().set_cookie(cookies[0].clone());
    assert!(
        describe().is_ok(),
        "settings/describe must accept the bridge-minted cookie"
    );

    rpc.credential().set_cookie(cookies[1].clone());
    let result = describe();
    assert!(
        result
            .as_ref()
            .is_err_and(|error| super::is_rejection(error)),
        "settings/describe must return HTTP 401 for the signed future-dated cookie"
    );
}
