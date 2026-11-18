//! The digest's model port: a one-shot job (`harness::one_shot`) on `memory.digest.model`, else the model of the
//! harness chain's first route. Tests install a fake per runtime root.
use crate::harness::one_shot::{self, Job, RouteModel};
use crate::harness::usage;
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub(crate) use crate::harness::one_shot::{ModelError, OneShot as DigestModel};

/// The output cap of one digest request.
const MAX_TOKENS: u32 = 2048;

/// Tests: every digest run on the runtime at `root` uses `model`.
#[cfg(test)]
pub(crate) fn install_test_model(root: &std::path::Path, model: Arc<dyn DigestModel>) {
    one_shot::install_test_model(root, Job::Digest, model);
}

/// The digest's model on `context`: `route_of` on the relay, bounded by `timeout`. Each answered request is a usage fact
/// under the ledger key `internal:digest`, on the route it asked (`harness::usage`).
pub(crate) fn model_for(context: &RuntimeContext, cfg: &Value) -> Result<Arc<dyn DigestModel>, ModelError> {
    let route = route_of(cfg);
    let route = route.as_ref().map(|(id, model)| RouteModel { id: id.as_str(), model: model.as_str() });
    one_shot::model_for(context, Job::Digest, usage::DIGEST_KEY, route, timeout(cfg), MAX_TOKENS)
}

/// The relay model a digest run asks, without a test fake (tests of the relay itself).
#[cfg(test)]
pub(crate) fn relay_model(context: &RuntimeContext, cfg: &Value) -> Result<one_shot::RelayModel, ModelError> {
    let (id, model) = route_of(cfg).ok_or(ModelError("unconfigured"))?;
    let route = RouteModel { id: id.as_str(), model: model.as_str() };
    one_shot::RelayModel::for_model(context, Job::Digest, usage::DIGEST_KEY, route, timeout(cfg), MAX_TOKENS)
}

/// The digest's route id and model id: `memory.digest.model` (its own route, whose id is the model id), else the
/// harness chain's first route (`harness::routes::chain`: `harness.routes`, else `harness.model`); `None` when neither
/// is configured or the chain is invalid.
fn route_of(cfg: &Value) -> Option<(String, String)> {
    if let Some(model) = crate::settings::DIGEST_MODEL.text(cfg) {
        return Some((model.clone(), model));
    }
    crate::harness::routes::chain(cfg).ok().and_then(|chain| chain.into_iter().next()).map(|route| (route.id, route.model))
}

/// The digest's model id: `memory.digest.model`, else the model of the harness chain's first route
/// (`harness::routes::chain`: `harness.routes`, else `harness.model`); `None` when neither is configured or the chain
/// is invalid.
pub(crate) fn model_id(cfg: &Value) -> Option<String> {
    route_of(cfg).map(|(_, model)| model)
}

/// The bound on one digest model request: `memory.digest.timeoutSecs`.
pub(crate) fn timeout(cfg: &Value) -> Duration {
    Duration::from_secs(crate::settings::DIGEST_TIMEOUT_SECS.integer(cfg).unsigned_abs())
}
