//! The idle rotation rule for harness conversations (`docs/agents/prompts-modes.md` "Harness idle rotation"): when
//! a sleeping harness session becomes due, read from its last model request and the route's own cache life and cap.
//! DSH's settings (`dsh.cacheTtlMinutes`, `dsh.rotation.caps`) and DSH's provider defaults never apply here.
// The idle check that calls this rule is built with the history budget (#6); remove this line then.
#![allow(dead_code)]

use serde_json::Value;

/// The cap when neither the route nor `default` sets a valid one, in input tokens.
pub(crate) const DEFAULT_CAP: u64 = 500_000;

/// A session's last model request (`store::last_call`): the model id its attempt recorded and the total input
/// tokens its provider reported (`captured_usage.prompt_tokens`, cached tokens included; `None`: none reported).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastCall {
    pub model: String,
    pub input: Option<u64>,
}

/// The cache life of the route that served `model`: `harness.cacheTtlMinutes.<model>`, a positive finite number of
/// minutes. There is no built-in value: `None` means the route never idle-rotates.
pub(crate) fn cache_life_minutes(cfg: &Value, model: &str) -> Option<f64> {
    cfg.pointer("/harness/cacheTtlMinutes")
        .and_then(|table| table.get(model))
        .and_then(Value::as_f64)
        .filter(|minutes| minutes.is_finite() && *minutes > 0.0)
}

/// The rotation cap of the route that served `model`: `harness.rotation.caps.<model>`, then
/// `harness.rotation.caps.default`, each only when a positive integer, then `DEFAULT_CAP`.
pub(crate) fn cap(cfg: &Value, model: &str) -> u64 {
    let caps = cfg.pointer("/harness/rotation/caps");
    let entry = |name: &str| {
        caps.and_then(|table| table.get(name))
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
    };
    entry(model)
        .or_else(|| entry("default"))
        .unwrap_or(DEFAULT_CAP)
}

/// When a sleeping session whose last model request is `last` becomes due for idle rotation, counted from its idle
/// start `idle_from` (ms): one minute before the route's cache life runs out (at once when the cache life is one
/// minute or less), once the request's reported input reached the route's cap. `None`: never, because the route has
/// no cache life of its own, the provider reported no input, or the input stayed below the cap.
pub(crate) fn due_at(cfg: &Value, last: &LastCall, idle_from: i64) -> Option<i64> {
    let minutes = cache_life_minutes(cfg, &last.model)?;
    if last.input? < cap(cfg, &last.model) {
        return None;
    }
    let lead_ms: i128 = if minutes <= 1.0 {
        0
    } else {
        // Whole minutes are exact in i128, and the fraction's milliseconds are computed from its bits, so no millisecond is lost.
        let whole = minutes.trunc();
        let frac_ms = ceil_fraction_ms(minutes - whole);
        (whole as i128 - 1).saturating_mul(60_000).saturating_add(frac_ms)
    };
    // The sum saturates in i128 and then at the i64 range, so a huge lead saturates the deadline from any start.
    Some(i128::from(idle_from).saturating_add(lead_ms).min(i128::from(i64::MAX)) as i64)
}

/// The exact ceiling of `frac` × 60000 for 0 <= frac < 1: the f64 is mant × 2^-shift, so the product is a ratio of
/// integers and no rounding happens before the ceiling.
fn ceil_fraction_ms(frac: f64) -> i128 {
    if frac <= 0.0 {
        return 0;
    }
    let bits = frac.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as u32;
    let frac_bits = (bits & ((1u64 << 52) - 1)) as u128;
    let (mant, shift) = if exp == 0 { (frac_bits, 1074u32) } else { (frac_bits | (1u128 << 52), 1075 - exp) };
    let num = mant * 60_000;
    if shift >= 127 {
        return 1; // num < 2^70, so the value is above 0 and below 1
    }
    let den = 1u128 << shift;
    ((num + den - 1) / den) as i128
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const IDLE: i64 = 1_000_000;

    fn call(model: &str, input: Option<u64>) -> LastCall {
        LastCall {
            model: model.into(),
            input,
        }
    }

    #[test]
    fn harness_idle_rotation_uses_only_the_routes_own_cache_life() {
        let own = json!({"harness":{"cacheTtlMinutes":{"relay-model":5}}});
        assert_eq!(
            due_at(&own, &call("relay-model", Some(500_000)), IDLE),
            Some(IDLE + 240_000),
            "a harness route's own cache life applies, one minute early"
        );
        let dsh_only = json!({"dsh":{"cacheTtlMinutes":{"relay-model":5,"local-relay":5,"opencode-go":120}}});
        for model in ["relay-model", "local-relay", "opencode-go"] {
            assert_eq!(
                cache_life_minutes(&dsh_only, model),
                None,
                "{model}: DSH's cache life never applies to a harness route"
            );
            assert_eq!(
                due_at(&dsh_only, &call(model, Some(900_000)), IDLE),
                None,
                "{model}: a route without a cache life of its own is never due"
            );
        }
        for model in ["local-relay", "opencode-go"] {
            assert_eq!(
                due_at(&json!({}), &call(model, Some(900_000)), IDLE),
                None,
                "{model}: DSH's provider defaults never apply to a harness route"
            );
        }
    }

    #[test]
    fn harness_idle_rotation_cache_life_must_be_a_positive_finite_number_of_minutes() {
        let cfg = |minutes: Value| json!({"harness":{"cacheTtlMinutes":{"m":minutes}}});
        assert_eq!(
            due_at(&cfg(json!(120)), &call("m", Some(500_000)), IDLE),
            Some(IDLE + 119 * 60_000),
            "a configured cache life of 120 minutes is due after 119"
        );
        assert_eq!(
            due_at(&cfg(json!(1.5)), &call("m", Some(500_000)), IDLE),
            Some(IDLE + 30_000),
            "fractional minutes count"
        );
        assert_eq!(
            due_at(&cfg(json!(1)), &call("m", Some(500_000)), IDLE),
            Some(IDLE),
            "a cache life of one minute is due at once"
        );
        assert_eq!(
            due_at(&cfg(json!(0.5)), &call("m", Some(500_000)), IDLE),
            Some(IDLE),
            "a cache life shorter than the lead is due at once"
        );
        for bad in [json!(0), json!(-5), json!("5"), json!(null), json!(true)] {
            assert_eq!(
                due_at(&cfg(bad.clone()), &call("m", Some(900_000)), IDLE),
                None,
                "{bad}: not a valid cache life"
            );
        }
        let huge = due_at(&cfg(json!(1e300)), &call("m", Some(500_000)), IDLE);
        assert!(
            huge.is_some_and(|at| at > IDLE),
            "a huge cache life saturates instead of overflowing: {huge:?}"
        );
        for idle in [-1, i64::MIN] {
            assert_eq!(
                due_at(&cfg(json!(1e300)), &call("m", Some(500_000)), idle),
                Some(i64::MAX),
                "a huge cache life saturates the deadline from idle start {idle}"
            );
        }
        assert_eq!(
            due_at(&cfg(json!(1.5e14)), &call("m", Some(500_000)), 0),
            Some(8_999_999_999_999_940_000),
            "a huge cache life keeps every millisecond of its lead"
        );
        assert_eq!(
            due_at(&cfg(json!(1.015_116_666_666_666_7)), &call("m", Some(500_000)), 0),
            Some(908),
            "a fractional cache life rounds its lead up exactly, not through f64"
        );
    }

    #[test]
    fn harness_idle_rotation_cap_comes_from_the_route_then_default_then_500000() {
        let ttl = json!({"m":5,"n":5,"o":5});
        let cfg = json!({"harness":{"cacheTtlMinutes":ttl.clone(),"rotation":{"caps":{"m":1000,"n":0,"default":2000}}}});
        assert_eq!(cap(&cfg, "m"), 1000, "the route's own cap");
        assert_eq!(cap(&cfg, "n"), 2000, "an invalid route cap falls back to default");
        assert_eq!(cap(&cfg, "o"), 2000, "a route without a cap uses default");
        assert_eq!(
            due_at(&cfg, &call("m", Some(1000)), IDLE),
            Some(IDLE + 240_000),
            "input at the cap is due"
        );
        assert_eq!(due_at(&cfg, &call("m", Some(999)), IDLE), None, "input below the cap is not");
        assert_eq!(due_at(&cfg, &call("o", Some(2000)), IDLE), Some(IDLE + 240_000));
        assert_eq!(due_at(&cfg, &call("o", Some(1999)), IDLE), None);
        let built_in = json!({"harness":{"cacheTtlMinutes":ttl},"dsh":{"rotation":{"caps":{"m":10,"default":10}}}});
        assert_eq!(cap(&built_in, "m"), DEFAULT_CAP, "DSH's caps never apply to a harness route");
        assert_eq!(due_at(&built_in, &call("m", Some(500_000)), IDLE), Some(IDLE + 240_000));
        assert_eq!(due_at(&built_in, &call("m", Some(499_999)), IDLE), None);
    }

    #[test]
    fn harness_idle_rotation_never_rotates_on_a_request_without_reported_input() {
        let cfg = json!({"harness":{"cacheTtlMinutes":{"m":5},"rotation":{"caps":{"m":1}}}});
        assert_eq!(
            due_at(&cfg, &call("m", Some(1)), IDLE),
            Some(IDLE + 240_000),
            "reported input at the cap is due"
        );
        assert_eq!(
            due_at(&cfg, &call("m", None), IDLE),
            None,
            "a request without reported input never makes the session due"
        );
    }
}
