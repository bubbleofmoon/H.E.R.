//! The digest's model port: a one-shot route walk (`harness::one_shot::RouteWalk`, #238) over the harness chain, as
//! harness turns walk it, or over `memory.digest.model` alone. Tests install a fake per runtime root.
use crate::harness::one_shot::{self, Job, Walk};
use crate::harness::routes::RouteSpec;
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

/// The digest's model on `context`: a walk over `walk_of` on the relay, bounded as a whole by `timeout`. Each response
/// end is a usage fact under the ledger key `internal:digest`, on the route that gave it (`harness::usage`).
pub(crate) fn model_for(context: &RuntimeContext, cfg: &Value) -> Result<Arc<dyn DigestModel>, ModelError> {
    one_shot::walk_for(context, Job::Digest, usage::DIGEST_KEY, walk_of(cfg), timeout(cfg), MAX_TOKENS)
}

/// The relay model of the digest's first route, without a test fake or a walk (tests of the relay itself).
#[cfg(test)]
pub(crate) fn relay_model(context: &RuntimeContext, cfg: &Value) -> Result<one_shot::RelayModel, ModelError> {
    let first = walk_of(cfg).and_then(|walk| walk.routes.into_iter().next()).ok_or(ModelError("unconfigured"))?;
    let route = one_shot::RouteModel { id: first.id.as_str(), model: first.model.as_str() };
    one_shot::RelayModel::for_model(context, Job::Digest, usage::DIGEST_KEY, route, timeout(cfg), MAX_TOKENS)
}

/// The routes a digest request walks: `memory.digest.model` alone (its own route, whose id is the model id; it is no
/// route of the chain, so it has no failover and no route health), else the harness chain (`harness::routes::chain`:
/// `harness.routes`, else `harness.model`), whose route health the walk keeps; `None` when neither is configured or the
/// chain is invalid.
fn walk_of(cfg: &Value) -> Option<Walk> {
    if let Some(model) = crate::settings::DIGEST_MODEL.text(cfg) {
        return Some(Walk { routes: vec![RouteSpec::implicit(model)], chain: true }); // RED-M10
    }
    crate::harness::routes::chain(cfg).ok().map(|routes| Walk { routes: routes.into_iter().take(1).collect(), chain: true }) // RED-M1
}

/// The digest's model id: the model of the first route it walks (`walk_of`: `memory.digest.model`, else the model of the
/// harness chain's first route); `None` when neither is configured or the chain is invalid.
pub(crate) fn model_id(cfg: &Value) -> Option<String> {
    walk_of(cfg).and_then(|walk| walk.routes.into_iter().next()).map(|route| route.model)
}

/// The bound on one digest request, every route its walk tries included: `memory.digest.timeoutSecs`.
pub(crate) fn timeout(cfg: &Value) -> Duration {
    Duration::from_secs(crate::settings::DIGEST_TIMEOUT_SECS.integer(cfg).unsigned_abs())
}
