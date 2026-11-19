//! The model and cost group, and the model route chosen per conversation (#162, `ConversationSetting`, the
//! conversation scope).

use crate::settings::kinds::{FileRule, Integer, ModelId, RouteId};
use crate::settings::{ConversationSetting, Setting};

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
    secret: false,
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
    secret: false,
};

/// Read by `harness::Limits::from_config`.
pub(crate) const HARNESS_MAX_TOKENS: Setting = Setting {
    key: "harness.maxTokens",
    pointer: "/harness/maxTokens",
    kind: &Integer { min: 1, max: 1_000_000, default: 32_768, file: FileRule::Clamp },
    applies: "live",
    group: "model and cost",
    label: "Token budget per turn",
    hint: "The tokens one harness turn may use across all its model requests; read when each turn starts.",
    chat: false,
    secret: false,
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
    secret: false,
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
    secret: false,
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
    secret: false,
};

/// Read by `harness::routes::selected` for every conversation without its own route (`CONVERSATION_ROUTE`).
pub(crate) const HARNESS_FIRST_ROUTE: Setting = Setting {
    key: "harness.firstRoute",
    pointer: "/harness/firstRoute",
    kind: &RouteId,
    applies: "live",
    group: "model and cost",
    label: "First model route",
    hint: "The route of harness.routes that every harness conversation without its own model route asks first, then the other routes in configured order; read when each turn starts. Not set, or naming no configured route: the configured order.",
    chat: false,
    secret: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] = &[
    HARNESS_MODEL,
    HARNESS_MAX_STEPS,
    HARNESS_MAX_TOKENS,
    DIGEST_MODEL,
    DIGEST_TIMEOUT_SECS,
    WEB_SEARCH_MODEL,
    HARNESS_FIRST_ROUTE,
];

/// The route one conversation's harness turns ask first; a conversation without one uses `HARNESS_FIRST_ROUTE`. Read
/// by `harness::routes::selected` when each turn starts.
pub(crate) const CONVERSATION_ROUTE: ConversationSetting = ConversationSetting {
    key: "conversation.route",
    global: &HARNESS_FIRST_ROUTE,
    label: "Model route",
    hint: "The route of harness.routes that this conversation's harness turns ask first, then the other routes in configured order; read when each turn starts. Without one, the first model route applies; a route not in harness.routes is ignored. A reset stores none: the configured order.",
};

/// The conversation scope's settings of this group, in the order the per-conversation page lists them.
pub(super) const CONVERSATION: &[ConversationSetting] = &[CONVERSATION_ROUTE];
