//! The digest's model port: a one-shot job (`harness::one_shot`) on `memory.digest.model`, else the model of the
//! harness chain's first route. Tests install a fake per runtime root.
use crate::harness::one_shot::{self, Job};
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

/// The digest's model on `context`: `model_id` on the relay, bounded by `timeout`.
pub(crate) fn model_for(context: &RuntimeContext, cfg: &Value) -> Result<Arc<dyn DigestModel>, ModelError> {
    one_shot::model_for(context, Job::Digest, model_id(cfg).as_deref(), timeout(cfg), MAX_TOKENS)
}

/// The relay model a digest run asks, without a test fake (tests of the relay itself).
#[cfg(test)]
pub(crate) fn relay_model(context: &RuntimeContext, cfg: &Value) -> Result<one_shot::RelayModel, ModelError> {
    let model = model_id(cfg).ok_or(ModelError("unconfigured"))?;
    one_shot::RelayModel::for_model(context, &model, timeout(cfg), MAX_TOKENS)
}

/// The digest's model id: `memory.digest.model`, else the model of the harness chain's first route
/// (`harness::routes::chain`: `harness.routes`, else `harness.model`); `None` when neither is configured or the chain
/// is invalid.
pub(crate) fn model_id(cfg: &Value) -> Option<String> {
    crate::settings::DIGEST_MODEL.text(cfg).or_else(|| {
        crate::harness::routes::chain(cfg).ok().and_then(|chain| chain.into_iter().next()).map(|route| route.model)
    })
}

/// The bound on one digest model request: `memory.digest.timeoutSecs`.
pub(crate) fn timeout(cfg: &Value) -> Duration {
    Duration::from_secs(crate::settings::DIGEST_TIMEOUT_SECS.integer(cfg).unsigned_abs())
}
