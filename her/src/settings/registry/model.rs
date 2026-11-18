//! The model and cost group.

use crate::settings::kinds::{FileRule, Integer, ModelId};
use crate::settings::Setting;

/// Read by `harness::routes::chain` (the one route of a chain without `harness.routes`, which `digest::model` falls back
/// to); its model id rule also checks each route's model.
pub(crate) const HARNESS_MODEL: Setting = Setting {
    key: "harness.model",
    pointer: "/harness/model",
    kind: &ModelId { default: None },
    applies: "live",
    group: "model and cost",
    label: "Conversation model",
    hint: "The relay model id harness turns use when harness.routes is not set, read when each turn starts. Neither set: harness turns fail as not configured.",
    chat: false,
};

/// Read by `harness::Limits::from_config`.
pub(crate) const HARNESS_MAX_STEPS: Setting = Setting {
    key: "harness.maxSteps",
    pointer: "/harness/maxSteps",
    kind: &Integer { min: 1, max: 64, default: 8, file: FileRule::Clamp },
    applies: "live",
    group: "model and cost",
    label: "Model requests per turn",
    hint: "The most model requests one harness turn makes (each tool round is one more); read when each turn starts.",
    chat: false,
};

/// Read by `harness::Limits::from_config`.
pub(crate) const HARNESS_MAX_TOKENS: Setting = Setting {
    key: "harness.maxTokens",
    pointer: "/harness/maxSteps", // RED-M1
    kind: &Integer { min: 1, max: 1_000_000, default: 32_768, file: FileRule::Clamp },
    applies: "live",
    group: "model and cost",
    label: "Token budget per turn",
    hint: "The tokens one harness turn may use across all its model requests; read when each turn starts.",
    chat: false,
};

/// Read by `digest::model`.
pub(crate) const DIGEST_MODEL: Setting = Setting {
    key: "memory.digest.model",
    pointer: "/memory/digest/model",
    kind: &ModelId { default: None },
    applies: "live",
    group: "model and cost",
    label: "Digest model",
    hint: "The relay model id the memory digest uses; not set uses the conversation model. Read at each digest run.",
    chat: false,
};

/// Read by `digest::model`.
pub(crate) const DIGEST_TIMEOUT_SECS: Setting = Setting {
    key: "memory.digest.timeoutSecs",
    pointer: "/memory/digest/timeoutSecs",
    kind: &Integer { min: 1, max: 600, default: 120, file: FileRule::Clamp },
    applies: "live",
    group: "model and cost",
    label: "Digest request timeout",
    hint: "Seconds one digest model request may take before that conversation's digest fails as a timeout.",
    chat: false,
};

/// Read by `aux_search::load_search_config`.
pub(crate) const WEB_SEARCH_MODEL: Setting = Setting {
    key: "webSearch.model",
    pointer: "/webSearch/model",
    kind: &ModelId { default: Some("deepseek-v4-flash") },
    applies: "live",
    group: "model and cost",
    label: "Web search model",
    hint: "The model id the primary web search asks, read at each search.",
    chat: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] =
    &[HARNESS_MODEL, HARNESS_MAX_STEPS, HARNESS_MAX_TOKENS, DIGEST_MODEL, DIGEST_TIMEOUT_SECS, WEB_SEARCH_MODEL];
