//! Unified HTTP console front: declarative route-policy table, host/cross-site validation,
//! credential admission, desktop sign-in, auth HTTP adapters, and verified caller authority.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentPolicy {
    AgentAllowed,
    AdminOnly(&'static str),
    PendingAdminOnly,
    V1StickerAdmin,
    V2AdminOnly,
    V2AgentAllowed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyPreflight {
    None,
    SessionKey,
    SessionKeyDeferAgentHeader,
    SessionKeyDeferMemoryAgent,
    SessionKeyDeferAlways,
    SessionKeyDeferIsolated,
    SessionKeyOrClosedOwnerReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyLimit {
    None,
    Max1Mb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionCookiePolicy {
    Renew,
    NoRenew,
    DeleteOnSuccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoutePolicy {
    pub(crate) isolated_supported: bool,
    pub(crate) public_probe: bool,
    pub(crate) agent: AgentPolicy,
    pub(crate) key_preflight: KeyPreflight,
    pub(crate) body_limit: BodyLimit,
    pub(crate) session_cookie: SessionCookiePolicy,
}

impl RoutePolicy {
    const fn public_probe() -> Self {
        Self {
            isolated_supported: true,
            public_probe: true,
            agent: AgentPolicy::AgentAllowed,
            key_preflight: KeyPreflight::None,
            body_limit: BodyLimit::None,
            session_cookie: SessionCookiePolicy::NoRenew,
        }
    }

    const fn admin(error: &'static str) -> Self {
        Self {
            isolated_supported: true,
            public_probe: false,
            agent: AgentPolicy::AdminOnly(error),
            key_preflight: KeyPreflight::None,
            body_limit: BodyLimit::None,
            session_cookie: SessionCookiePolicy::Renew,
        }
    }

    const fn admin_bounded(error: &'static str) -> Self {
        Self {
            body_limit: BodyLimit::Max1Mb,
            ..Self::admin(error)
        }
    }

    const fn auth(method: &str, session_cookie: SessionCookiePolicy) -> Self {
        let body_limit = match method.as_bytes() {
            [b'P', b'O', b'S', b'T'] => BodyLimit::Max1Mb,
            _ => BodyLimit::None,
        };
        Self {
            body_limit,
            session_cookie,
            ..Self::admin("agent credential cannot access console authentication")
        }
    }

    const fn console_or_agent(key_preflight: KeyPreflight) -> Self {
        Self {
            isolated_supported: true,
            public_probe: false,
            agent: AgentPolicy::AgentAllowed,
            key_preflight,
            body_limit: BodyLimit::None,
            session_cookie: SessionCookiePolicy::Renew,
        }
    }

    const fn v2_agent(key_preflight: KeyPreflight) -> Self {
        Self {
            agent: AgentPolicy::V2AgentAllowed,
            ..Self::console_or_agent(key_preflight)
        }
    }

    const fn with_body(self, body_limit: BodyLimit) -> Self {
        Self { body_limit, ..self }
    }

    const fn with_cookie(self, session_cookie: SessionCookiePolicy) -> Self {
        Self {
            session_cookie,
            ..self
        }
    }

    const fn with_agent(self, agent: AgentPolicy) -> Self {
        Self { agent, ..self }
    }

    const fn isolated_only(self, isolated_supported: bool) -> Self {
        Self {
            isolated_supported,
            ..self
        }
    }
}

const DESKTOP_ERR: &str = "desktop sign-in requires the installing user";
const QQ_LOGIN_ERR: &str = "QQ login state is restricted to administrators";
const RESTART_ERR: &str = "restart is restricted to administrators";
const DEPS_ERR: &str = "dependency upgrades are restricted to administrators";
const RELEASES_ERR: &str = "release settings are restricted to administrators";
const BACKUPS_ERR: &str = "backup settings are restricted to administrators";
const SCOPE_ERR: &str = "该接口仅控制台可用";

const EXACT_ROUTES: &[(&str, &str, RoutePolicy)] = &[
    // Public probes (unauthenticated health & icons per ADR 0003).
    ("GET", "/api/health", RoutePolicy::public_probe()),
    ("GET", "/favicon.ico", RoutePolicy::public_probe()),
    ("GET", "/apple-touch-icon.png", RoutePolicy::public_probe()),
    // Desktop sign-in & console authentication.
    (
        "POST",
        "/api/desktop/sign-in",
        RoutePolicy::admin(DESKTOP_ERR).with_cookie(SessionCookiePolicy::NoRenew),
    ),
    (
        "GET",
        "/api/auth/status",
        RoutePolicy::auth("GET", SessionCookiePolicy::NoRenew),
    ),
    (
        "GET",
        "/api/auth/sessions",
        RoutePolicy::auth("GET", SessionCookiePolicy::Renew),
    ),
    (
        "POST",
        "/api/auth/first-run",
        RoutePolicy::auth("POST", SessionCookiePolicy::NoRenew),
    ),
    (
        "POST",
        "/api/auth/login",
        RoutePolicy::auth("POST", SessionCookiePolicy::NoRenew),
    ),
    (
        "POST",
        "/api/auth/logout",
        RoutePolicy::auth("POST", SessionCookiePolicy::NoRenew),
    ),
    (
        "POST",
        "/api/auth/change-password",
        RoutePolicy::auth("POST", SessionCookiePolicy::NoRenew),
    ),
    (
        "POST",
        "/api/auth/sessions/revoke",
        RoutePolicy::auth("POST", SessionCookiePolicy::NoRenew),
    ),
    // Administrator-only system routes.
    ("GET", "/api/qq/login", RoutePolicy::admin(QQ_LOGIN_ERR)),
    ("POST", "/api/restart", RoutePolicy::admin_bounded(RESTART_ERR)),
    ("GET", "/api/dependencies", RoutePolicy::admin(DEPS_ERR)),
    (
        "POST",
        "/api/dependencies/check",
        RoutePolicy::admin_bounded(DEPS_ERR),
    ),
    (
        "POST",
        "/api/dependencies/approve",
        RoutePolicy::admin_bounded(DEPS_ERR),
    ),
    ("GET", "/api/releases", RoutePolicy::admin(RELEASES_ERR)),
    (
        "GET",
        "/api/releases/settings",
        RoutePolicy::admin(RELEASES_ERR),
    ),
    (
        "POST",
        "/api/releases/settings",
        RoutePolicy::admin_bounded(RELEASES_ERR),
    ),
    (
        "POST",
        "/api/releases/approve",
        RoutePolicy::admin_bounded(RELEASES_ERR),
    ),
    ("GET", "/api/backups/settings", RoutePolicy::admin(BACKUPS_ERR)),
    ("GET", "/api/backups", RoutePolicy::admin(BACKUPS_ERR)),
    (
        "POST",
        "/api/backups/settings",
        RoutePolicy::admin_bounded(BACKUPS_ERR),
    ),
    (
        "POST",
        "/api/backups/restore/preview",
        RoutePolicy::admin_bounded(BACKUPS_ERR),
    ),
    (
        "POST",
        "/api/backups/restore",
        RoutePolicy::admin_bounded(BACKUPS_ERR),
    ),
    ("POST", "/api/conversations/scope", RoutePolicy::admin(SCOPE_ERR)),
    // Memory archive/history.
    (
        "POST",
        "/api/memory/archive",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferMemoryAgent),
    ),
    (
        "POST",
        "/api/memory/history",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferMemoryAgent),
    ),
    // Session reset & machine token rotation.
    (
        "POST",
        "/api/session/reset",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyOrClosedOwnerReset),
    ),
    (
        "POST",
        "/api/console/token",
        RoutePolicy::console_or_agent(KeyPreflight::None)
            .with_cookie(SessionCookiePolicy::DeleteOnSuccess),
    ),
    // Pending & tool-log admin-only surfaces with agent-token semantics.
    (
        "GET",
        "/api/pending",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::PendingAdminOnly),
    ),
    (
        "GET",
        "/api/socialV2/tool-log",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V2AdminOnly),
    ),
    (
        "POST",
        "/api/socialV2/tool-log/clear",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V2AdminOnly),
    ),
    // V2 admin-only social routes (defer query key preflight when x-agent-token is present).
    (
        "GET",
        "/api/socialV2/config",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_agent(AgentPolicy::V2AdminOnly),
    ),
    (
        "POST",
        "/api/socialV2/config",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_agent(AgentPolicy::V2AdminOnly),
    ),
    (
        "GET",
        "/api/socialV2/activity",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_agent(AgentPolicy::V2AdminOnly),
    ),
    (
        "POST",
        "/api/socialV2/activity",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_agent(AgentPolicy::V2AdminOnly),
    ),
    (
        "GET",
        "/api/socialV2/states",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_agent(AgentPolicy::V2AdminOnly),
    ),
    // V1 sticker administration.
    (
        "GET",
        "/api/stickers",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V1StickerAdmin),
    ),
    (
        "POST",
        "/api/stickers/note",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V1StickerAdmin),
    ),
    (
        "POST",
        "/api/stickers/sync",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V1StickerAdmin),
    ),
    (
        "POST",
        "/api/stickers/remark",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_agent(AgentPolicy::V1StickerAdmin),
    ),
    // Social V2 agent-allowed routes (global V2 gates + agent-header key deferral).
    (
        "POST",
        "/api/socialV2/send-message",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader)
            .with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/socialV2/mark-read",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/wake-config",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/wait",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/send-burst",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/send-poke",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/feedback",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/message-detail",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/memory",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/memory-append",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/memory-update",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/memory-remove",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/memory-clear",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/state",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/unread",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/prompt",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/slang/query",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/slang/submit",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/recent",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/my-recent",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/active-members",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/self-image",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/images/message",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/forward-message",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/forward-media",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/sticker-list",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/sticker-list",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "GET",
        "/api/socialV2/sticker-image",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/sticker-note",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/sticker-remark",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/send-sticker",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    (
        "POST",
        "/api/socialV2/collect-sticker",
        RoutePolicy::v2_agent(KeyPreflight::SessionKeyDeferAgentHeader),
    ),
    // Handler-deferred key preflight routes.
    (
        "POST",
        "/api/send/group",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAlways),
    ),
    (
        "POST",
        "/api/send/private",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAlways),
    ),
    (
        "POST",
        "/api/send/reply",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAlways),
    ),
    (
        "POST",
        "/api/socialV2/wake",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferIsolated),
    ),
    (
        "POST",
        "/api/authorize/read",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferIsolated),
    ),
    // Remaining SessionKey routes.
    (
        "GET",
        "/api/social",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/social",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/social/state",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/social/flush",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/socialV2/send-voice",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/voice/send",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/send/voice",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/socialV2/reset",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/socialV2/wake-snapshot",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/socialV2/initial-prompt",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/socialV2/initial-prompt",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/socialV2/feedback",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/socialV2/feedback-clear",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/inbound/gaps",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/inbound/gaps/retry",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/socialV2/send-outbox",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/sessions",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/turns",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/turns/resolve",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/model-chain",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/voice/list",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/socialV2/voices",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "GET",
        "/api/tokens/turns",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/session-models",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    (
        "POST",
        "/api/slang/extract",
        RoutePolicy::console_or_agent(KeyPreflight::SessionKey),
    ),
    // Role management POST routes (1 MB bounded JSON).
    (
        "POST",
        "/api/roles/create",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/roles/update",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/roles/rename",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/roles/delete",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/roles/limit",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/role",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    (
        "POST",
        "/api/role-mode",
        RoutePolicy::console_or_agent(KeyPreflight::None).with_body(BodyLimit::Max1Mb),
    ),
    // Remaining KeyPreflight::None console-or-agent routes.
    (
        "POST",
        "/api/mode",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/status",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/whitelist",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/security",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/audit/recent",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/whitelist",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/security",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/audit/undo",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/session-models",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/roles",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/roles/content",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/presets",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/preset/sim-prompt",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/preset/sim-prompt/backups",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/preset/sim-prompt",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/preset/sim-prompt/restore",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/tokens/by-provider",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/usage/attribution",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/tokens/summary",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/tokens/series",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/tokens/recent",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/slang",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "GET",
        "/api/recovery-record",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/clear",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/config",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/batch-delete",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/batch-confirm",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/batch-reject",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
    (
        "POST",
        "/api/slang/research",
        RoutePolicy::console_or_agent(KeyPreflight::None),
    ),
];

pub(crate) fn route_policy(method: &str, path: &str) -> Option<RoutePolicy> {
    if let Some((_, _, policy)) = EXACT_ROUTES
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
    {
        return Some(*policy);
    }
    if matches!(method, "GET" | "POST") && crate::console_settings::is_route(path) {
        let body_limit = if method == "POST" && path.starts_with("/api/settings/") {
            BodyLimit::Max1Mb
        } else {
            BodyLimit::None
        };
        return Some(
            RoutePolicy::admin("settings routes are restricted to administrators")
                .with_body(body_limit),
        );
    }
    if matches!(method, "GET" | "POST")
        && (path == "/api/personas" || path.starts_with("/api/personas/"))
    {
        return Some(RoutePolicy::admin(
            "persona routes are restricted to administrators",
        ));
    }
    if matches!(method, "GET" | "POST")
        && (path == "/api/schedule" || path.starts_with("/api/schedule/"))
    {
        let body_limit = if method == "POST" {
            BodyLimit::Max1Mb
        } else {
            BodyLimit::None
        };
        return Some(
            RoutePolicy::admin("日程接口仅限管理员控制台，不接受 agent 凭据")
                .with_body(body_limit),
        );
    }
    if (method == "GET" && path == "/api/tasks")
        || (matches!(method, "GET" | "POST") && path.starts_with("/api/tasks/"))
    {
        let body_limit = if method == "POST" {
            BodyLimit::Max1Mb
        } else {
            BodyLimit::None
        };
        return Some(
            RoutePolicy::admin("任务接口仅限管理员控制台，不接受 agent 凭据")
                .with_body(body_limit),
        );
    }
    if matches!(method, "GET" | "POST")
        && matches!(
            path.strip_prefix("/api/memory/"),
            Some(
                "write"
                    | "search"
                    | "forget"
                    | "index"
                    | "preview"
                    | "acl"
                    | "settings-get"
                    | "settings-set"
            )
        )
    {
        let body_limit = if path == "/api/memory/acl" {
            BodyLimit::Max1Mb
        } else {
            BodyLimit::None
        };
        return Some(
            RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferMemoryAgent)
                .with_body(body_limit),
        );
    }
    if path.strip_prefix("/api/slang/").is_some_and(|tail| {
        let parts: Vec<&str> = tail.split('/').collect();
        match (method, parts.as_slice()) {
            ("PATCH" | "DELETE", [id]) => !id.is_empty(),
            ("POST", [id, "confirm" | "reject"]) => !id.is_empty(),
            _ => false,
        }
    }) {
        return Some(RoutePolicy::console_or_agent(KeyPreflight::None));
    }
    // Method-agnostic legacy/isolated agent-denial surfaces (closed in isolated HTTP allowlist).
    if path == "/api/stickers" || path.starts_with("/api/stickers/") {
        return Some(
            RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
                .with_agent(AgentPolicy::V1StickerAdmin)
                .isolated_only(false),
        );
    }
    if path == "/api/socialV2/config" || path == "/api/socialV2/activity" {
        return Some(
            RoutePolicy::console_or_agent(KeyPreflight::SessionKeyDeferAgentHeader)
                .with_agent(AgentPolicy::V2AdminOnly)
                .isolated_only(false),
        );
    }
    if method == "POST"
        && (path.starts_with("/api/auth/")
            || path.starts_with("/api/backups/")
            || path.starts_with("/api/releases/")
            || path.starts_with("/api/dependencies/"))
    {
        return Some(
            RoutePolicy::admin("route requires an explicit endpoint")
                .with_body(BodyLimit::Max1Mb)
                .isolated_only(false),
        );
    }
    if method == "GET"
        && !path.starts_with("/api/")
        && path != "/api"
        && (crate::console_ui::legacy_redirect(path).is_some()
            || crate::console_ui::resolve_asset(path).is_some())
    {
        let cookie = if path.starts_with("/assets/") {
            SessionCookiePolicy::NoRenew
        } else {
            SessionCookiePolicy::Renew
        };
        return Some(
            RoutePolicy::admin("console UI routes are restricted to administrators")
                .with_cookie(cookie),
        );
    }
    None
}

pub(crate) fn supports_request(context: &RuntimeContext, method: &str, path: &str) -> bool {
    !context.is_isolated()
        || route_policy(method, path).is_some_and(|policy| policy.isolated_supported)
}

pub(crate) fn is_bounded_json_request(method: &str, path: &str) -> bool {
    route_policy(method, path).is_some_and(|policy| policy.body_limit == BodyLimit::Max1Mb)
}

pub(super) fn bounded_post_body_too_large(req: &Request) -> bool {
    is_bounded_json_request(&req.method, &req.path)
        && (req.body_too_large
            || req
                .headers
                .get("content-length")
                .map(|value| decimal_exceeds_limit(value, BOUNDED_POST_MAX_BODY_BYTES))
                .unwrap_or(false)
            || req.body.as_bytes().len() > BOUNDED_POST_MAX_BODY_BYTES)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallerKind {
    Administrator,
    Agent { console_credential: bool },
    Unauthenticated,
}

#[derive(Clone, Copy)]
pub(crate) struct AdminCaller<'a> {
    context: &'a RuntimeContext,
}

impl<'a> AdminCaller<'a> {
    pub(crate) fn new(context: &'a RuntimeContext) -> Self {
        Self { context }
    }

    pub(crate) fn context(&self) -> &'a RuntimeContext {
        self.context
    }

    pub(crate) fn gate(&self) -> Result<Option<crate::conversation_state::Gate>, (u16, String)> {
        crate::config_tx::acquire_config_gate(self.context)
    }

    pub(crate) fn with_authority<T>(
        &self,
        f: impl FnOnce(crate::persona::Authority<'_>) -> T,
    ) -> T {
        let authority = || self.gate();
        f(&authority)
    }
}

pub(crate) struct VerifiedCaller {
    pub(crate) context: RuntimeContext,
    pub(crate) config: Value,
    pub(crate) kind: CallerKind,
    pub(crate) memory: crate::memory_actions::ConsoleCapability,
    pub(crate) browser_acl: Option<crate::memory_actions::BrowserAcl>,
    pub(crate) config_diff: Option<crate::config_tx::ConfigDiff>,
}

impl VerifiedCaller {
    pub(crate) fn record_config_diff(&mut self, diff: Option<crate::config_tx::ConfigDiff>) {
        self.config_diff = crate::config_tx::merge_diffs(self.config_diff.take(), diff);
    }

    pub(crate) fn is_admitted(&self) -> bool {
        matches!(
            self.kind,
            CallerKind::Administrator
                | CallerKind::Agent {
                    console_credential: true
                }
        )
    }

    pub(crate) fn is_agent(&self) -> bool {
        matches!(self.kind, CallerKind::Agent { .. })
    }

    pub(crate) fn admin(&self) -> Option<AdminCaller<'_>> {
        matches!(self.kind, CallerKind::Administrator).then(|| AdminCaller::new(&self.context))
    }
}

#[cfg(test)]
pub(crate) fn revoked_admin_context(context: &RuntimeContext) -> RuntimeContext {
    let mut revoked = context.clone();
    revoked.console_authority = Some(crate::console_auth::ConsoleAuthority::Machine {
        token_digest: credential_digest("revoked-test-credential"),
    });
    revoked
}

fn allowed_hosts(port: u16) -> [String; 4] {
    let with_port = |host: &str| {
        if port == 80 {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        }
    };
    [
        with_port("127.0.0.1"),
        with_port("localhost"),
        with_port("[::1]"),
        crate::console_auth::canonical_host(port),
    ]
}

pub(crate) fn is_public_probe(method: &str, path: &str) -> bool {
    matches!(
        route_policy(method, path),
        Some(RoutePolicy {
            public_probe: true,
            ..
        })
    )
}

pub(crate) fn memory_agent_call(req: &Request) -> bool {
    req.path.starts_with("/api/memory/")
        && req
            .headers
            .get("x-agent-token")
            .is_some_and(|token| !token.is_empty())
}

pub(super) fn presented_console_token(req: &Request) -> Option<&str> {
    req.query
        .get("token")
        .or_else(|| req.headers.get("x-console-token"))
        .map(String::as_str)
}

pub(super) fn token_supplied(req: &Request, expected: &str) -> bool {
    presented_console_token(req).is_some_and(|presented| credential_eq(expected, presented))
}

pub(super) fn authorized(req: &Request, cfg: &Value) -> bool {
    token_supplied(req, &console_token(cfg))
}

fn has_fetch_metadata(req: &Request) -> bool {
    req.headers.keys().any(|name| name.starts_with("sec-fetch-"))
}

fn has_agent_marker(req: &Request) -> bool {
    req.headers.contains_key("x-agent-token") || req.headers.contains_key("x-agent-call")
}

fn canonical_request(req: &Request, port: u16) -> bool {
    req.headers
        .get("host")
        .is_some_and(|host| host == &crate::console_auth::canonical_host(port))
}

pub(super) enum BrowserCookie {
    Renew {
        port: u16,
        digest: [u8; 32],
        id: [u8; 32],
    },
    Delete {
        port: u16,
        digest: [u8; 32],
    },
    LegacyPage {
        port: u16,
        mac: [u8; 32],
    },
    LegacyPageDelete {
        port: u16,
    },
}

impl BrowserCookie {
    pub(super) fn header(&self) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        match self {
            Self::Renew { port, digest, id } => format!(
                "her_browser_{port}_{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                URL_SAFE_NO_PAD.encode(digest),
                URL_SAFE_NO_PAD.encode(id),
                7 * 24 * 60 * 60
            ),
            Self::Delete { port, digest } => format!(
                "her_browser_{port}_{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
                URL_SAFE_NO_PAD.encode(digest)
            ),
            Self::LegacyPage { port, mac } => format!(
                "her_legacy_{port}={}; HttpOnly; SameSite=Strict; Path=/",
                URL_SAFE_NO_PAD.encode(mac)
            ),
            Self::LegacyPageDelete { port } => {
                format!("her_legacy_{port}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
            }
        }
    }
}

pub(super) enum ConsoleRedirect {
    CanonicalOrigin(u16, String),
    Path(String),
}

pub(super) struct ConsoleResponse {
    pub(super) status: u16,
    pub(super) content_type: String,
    pub(super) body: String,
    pub(super) redirect: Option<ConsoleRedirect>,
    pub(super) cookies: Vec<BrowserCookie>,
    pub(super) raw_body: Option<&'static [u8]>,
    pub(super) csp: Option<&'static str>,
}

impl From<(u16, String, String)> for ConsoleResponse {
    fn from((status, content_type, body): (u16, String, String)) -> Self {
        Self {
            status,
            content_type,
            body,
            redirect: None,
            cookies: Vec::new(),
            raw_body: None,
            csp: None,
        }
    }
}

/// Host header must name the loopback console with its bound port (DNS rebinding guard).
pub(super) fn host_denial(req: &Request, port: u16) -> Option<(u16, Value)> {
    if is_public_probe(&req.method, &req.path) {
        return None;
    }
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    if allowed_hosts(port)
        .iter()
        .any(|allowed| allowed == &host.to_ascii_lowercase())
        || (port == 80
            && matches!(
                host.to_ascii_lowercase().as_str(),
                "127.0.0.1:80" | "localhost:80" | "[::1]:80" | "her.localhost:80"
            ))
    {
        return None;
    }
    let shown = if host.is_empty() { "缺失" } else { host };
    Some((
        403,
        json!({"ok": false, "error": format!("Host 头不被允许（{shown}）：控制台只接受本机回环访问")}),
    ))
}

fn origin_host(origin: &str) -> String {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return String::new();
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme.is_empty()
        || !scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return String::new();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority)
        .to_lowercase();
    let default_port = match scheme.as_str() {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        _ => None,
    };
    match (authority.rsplit_once(':'), default_port) {
        (Some((host, port)), Some(default)) if port == default && !host.is_empty() => {
            host.to_string()
        }
        _ => authority,
    }
}

/// CSRF guard for non-GET requests after console credential admission.
pub(super) fn cross_site_denial(req: &Request, port: u16) -> Option<(u16, Value)> {
    if req.method == "GET" {
        return None;
    }
    let content_type = req
        .headers
        .get("content-type")
        .map(|v| v.to_lowercase())
        .unwrap_or_default();
    if !content_type.contains("application/json") {
        return Some((
            415,
            json!({"ok": false, "error": "请求必须是 application/json"}),
        ));
    }
    if let Some(origin) = req.headers.get("origin").filter(|o| !o.is_empty()) {
        let host = origin_host(origin);
        if !allowed_hosts(port).contains(&host) {
            return Some((403, json!({"ok": false, "error": "跨站请求被拒绝"})));
        }
    }
    None
}

fn browser_candidates(
    req: &Request,
    port: u16,
) -> Result<Vec<([u8; 32], [u8; 32])>, (u16, String)> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest as _, Sha256};

    let invalid = || (401, "browser credential is invalid".into());
    let prefix = format!("her_browser_{port}_");
    let Some(line) = req.headers.get("cookie") else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for pair in line.split(';').map(str::trim) {
        let name = pair.split_once('=').map_or(pair, |(name, _)| name);
        let Some(tag) = name.trim().strip_prefix(&prefix) else {
            continue;
        };
        let (exact_name, value) = pair.split_once('=').ok_or_else(invalid)?;
        if name.trim() != exact_name
            || tag.len() != 43
            || value.len() != 43
            || candidates.len() >= crate::console_auth::MAX_BROWSER_SESSIONS
            || !seen.insert(name)
        {
            return Err(invalid());
        }
        let id: [u8; 32] = URL_SAFE_NO_PAD
            .decode(value)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(invalid)?;
        let digest: [u8; 32] = Sha256::digest(id).into();
        if URL_SAFE_NO_PAD.encode(digest) != tag || URL_SAFE_NO_PAD.encode(id) != value {
            return Err(invalid());
        }
        candidates.push((digest, id));
    }
    Ok(candidates)
}

fn browser_denial(req: &Request, port: u16) -> Result<(), (u16, String)> {
    let origin = crate::console_auth::canonical_origin(port);
    let claimed_origin = req.headers.get("origin");
    let origin_matches = claimed_origin == Some(&origin);
    if !canonical_request(req, port)
        || req
            .headers
            .get("sec-fetch-site")
            .is_some_and(|site| !matches!(site.as_str(), "same-origin" | "none"))
        || ((claimed_origin.is_some() || req.method != "GET") && !origin_matches)
    {
        return Err((403, "browser request requires the console origin".into()));
    }
    if req.method != "GET"
        && !req.headers.get("content-type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Err((415, "browser request requires application/json".into()));
    }
    Ok(())
}

fn validate_browser_acl(
    req: &Request,
) -> Result<Option<crate::memory_actions::BrowserAcl>, (u16, String)> {
    use crate::memory_actions::BrowserAcl;
    if req.path != "/api/memory/acl" {
        return Ok(None);
    }
    if !req.query.is_empty() {
        return Err((400, "browser ACL requests do not accept query fields".into()));
    }
    match req.method.as_str() {
        "GET" if req.body.is_empty() && !req.headers.contains_key("transfer-encoding") => {
            Ok(Some(BrowserAcl::List))
        }
        "POST" => {
            let fields: Value = serde_json::from_str(&req.body)
                .map_err(|_| (400, "body must be a JSON object".into()))?;
            let action = match fields.get("action").and_then(Value::as_str) {
                Some("grant") => crate::memory_actions::AclAction::Grant,
                Some("revoke") => crate::memory_actions::AclAction::Revoke,
                _ => return Err((400, "ACL action must be grant or revoke".into())),
            };
            Ok(Some(BrowserAcl::Write(action, fields)))
        }
        _ => Err((405, "browser ACL changes require POST".into())),
    }
}

fn select_browser(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
    expected: &str,
    renew: bool,
) -> Result<(Option<crate::console_auth::ConsoleAuthority>, Option<BrowserCookie>), (u16, String)> {
    use crate::console_auth::ConsoleAuthority;
    let candidates = browser_candidates(req, port)?;
    if candidates.is_empty() {
        return Ok((None, None));
    }
    let ids: Vec<_> = candidates.iter().map(|(digest, _)| *digest).collect();
    let token = credential_digest(expected);
    let Some(admission) = crate::console_auth::admit_browser(
        context,
        &crate::console_auth::canonical_origin(port),
        (!expected.is_empty()).then_some(token),
        &ids,
        renew,
    )?
    else {
        return Ok((None, None));
    };
    let digest = admission.proof.id_digest;
    let cookie = if admission.renewed {
        candidates
            .into_iter()
            .find(|(id_digest, _)| *id_digest == digest)
            .map(|(_, id)| BrowserCookie::Renew { port, digest, id })
    } else {
        None
    };
    Ok((
        Some(ConsoleAuthority::Browser {
            token_digest: token,
            grant: admission.proof,
        }),
        cookie,
    ))
}

fn selected_memory_capability(
    req: &Request,
    context: &RuntimeContext,
    cfg: &Value,
    effective: &str,
) -> Result<crate::memory_actions::ConsoleCapability, (u16, String)> {
    use crate::memory_actions::ConsoleCapability;
    let Some(header) = req.headers.get("x-console-token").map(|token| token.trim()) else {
        return Ok(ConsoleCapability::Conversation);
    };
    let configured = console_token(cfg);
    if !has_fetch_metadata(req) && credential_eq(&configured, header) {
        return Ok(ConsoleCapability::Configured(credential_digest(header)));
    }
    if let Some(proof) =
        crate::console_auth::admit_explicit_authority(context, header, effective, false)?
    {
        return Ok(ConsoleCapability::Desktop(proof));
    }
    Ok(ConsoleCapability::Conversation)
}

#[cfg(test)]
thread_local! {
    pub(super) static FRONT_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut() -> bool>>> =
        const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_ADMISSION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

fn load_front_config(context: &RuntimeContext) -> Result<Value, &'static str> {
    #[cfg(test)]
    if FRONT_READ_HOOK.with(|hook| hook.borrow_mut().as_mut().is_some_and(|hook| hook())) {
        return Err("runtime config is unavailable");
    }
    context.load_config()
}

struct FrontAdmission {
    config: Value,
    proof: Option<crate::console_auth::ConsoleAuthority>,
    renewal: Option<BrowserCookie>,
    memory: crate::memory_actions::ConsoleCapability,
    browser_acl: Option<crate::memory_actions::BrowserAcl>,
}

fn admit_selected_request(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
) -> Result<FrontAdmission, (u16, String)> {
    use crate::memory_actions::ConsoleCapability;
    let config = load_front_config(context)
        .map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    crate::console_auth::ensure_credentials_ready(context)?;
    let agent_call = has_agent_marker(req);
    let explicit = presented_console_token(req);
    let candidates = browser_candidates(req, port)?;
    let browser_auth = explicit.is_none()
        && !agent_call
        && !candidates.is_empty()
        && (canonical_request(req, port)
            || !crate::console_ui::is_ui_page_request(&req.method, &req.path));
    let _gate = if browser_auth {
        Some(acquire_social_state_gate(context)?)
    } else {
        None
    };
    let config = if browser_auth {
        crate::console_auth::ensure_credentials_ready(context)?;
        load_front_config(context)
            .map_err(|_| (503, "runtime config is unavailable".to_string()))?
    } else {
        config
    };
    let expected = context.effective_console_token(&config);
    let memory_route = req.path.starts_with("/api/memory/");
    let (proof, renewal, browser_acl) = if let Some(presented) = explicit {
        (
            crate::console_auth::admit_explicit_authority(
                context,
                presented,
                &expected,
                !has_fetch_metadata(req),
            )?,
            None,
            None,
        )
    } else if browser_auth {
        browser_denial(req, port)?;
        if bounded_post_body_too_large(req) {
            return Err((413, BODY_TOO_LARGE_ERROR.into()));
        }
        let acl = validate_browser_acl(req)?;
        let renew = route_policy(&req.method, &req.path)
            .is_none_or(|policy| policy.session_cookie == SessionCookiePolicy::Renew);
        let (proof, renewal) = select_browser(req, context, port, &expected, renew)?;
        let acl = proof.as_ref().and(acl);
        (proof, renewal, acl)
    } else {
        (None, None, None)
    };
    if (proof.is_some()
        || (memory_route
            && req
                .headers
                .get("x-agent-token")
                .is_some_and(|t| !t.is_empty())))
        && !matches!(proof, Some(crate::console_auth::ConsoleAuthority::Browser { .. }))
    {
        if let Some((status, value)) = cross_site_denial(req, port) {
            return Err((
                status,
                value["error"].as_str().unwrap_or("request refused").to_string(),
            ));
        }
    }
    let memory = if memory_route {
        selected_memory_capability(req, context, &config, &expected)?
    } else {
        ConsoleCapability::Conversation
    };
    Ok(FrontAdmission {
        config,
        proof,
        renewal,
        memory,
        browser_acl,
    })
}

fn parse_auth_json_object(body: &str) -> Result<serde_json::Map<String, Value>, (u16, String)> {
    let Value::Object(map) = serde_json::from_str::<Value>(body)
        .map_err(|_| (400, "请求体必须是合法 JSON 对象".to_string()))?
    else {
        return Err((400, "请求体必须是 JSON 对象".to_string()));
    };
    Ok(map)
}

fn parse_auth_digest(raw: &str) -> Option<[u8; 32]> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    if raw.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(raw).ok()?;
    let digest: [u8; 32] = bytes.try_into().ok()?;
    if URL_SAFE_NO_PAD.encode(digest) != raw {
        return None;
    }
    Some(digest)
}

fn auth_retry_seconds(retry_after_ms: i64) -> i64 {
    if retry_after_ms <= 0 {
        0
    } else {
        retry_after_ms.saturating_add(999) / 1000
    }
}

fn issue_session_cookie_response(
    port: u16,
    issued: crate::console_auth::IssuedSession,
) -> ConsoleResponse {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let mut response = ConsoleResponse::from((
        200,
        "application/json; charset=utf-8".to_string(),
        json!({
            "ok": true,
            "authenticated": true,
            "sessionId": URL_SAFE_NO_PAD.encode(issued.id_digest),
        })
        .to_string(),
    ));
    response.cookies.push(BrowserCookie::Renew {
        port,
        digest: issued.id_digest,
        id: issued.id,
    });
    response
}

fn auth_status_route(
    context: &RuntimeContext,
    port: u16,
    admission: &FrontAdmission,
) -> ConsoleResponse {
    use crate::console_auth::ConsoleAuthority;
    let json_reply = |status: u16, value: Value| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            value.to_string(),
        ))
    };
    let snapshot = match crate::console_auth::auth_status(context) {
        Ok(snapshot) => snapshot,
        Err((code, msg)) => return json_reply(code, json!({ "ok": false, "error": msg })),
    };
    let auth_kind = match &admission.proof {
        Some(ConsoleAuthority::Browser { .. }) => json!("browser"),
        Some(ConsoleAuthority::Desktop { .. }) => json!("desktop"),
        Some(ConsoleAuthority::Machine { .. }) => json!("machine"),
        None => Value::Null,
    };
    let configured_token = admission
        .config
        .get("consoleToken")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.trim().is_empty());
    let token_file = if configured_token || !context.is_isolated() {
        "config/config.json"
    } else {
        "data/console-token"
    };
    json_reply(
        200,
        json!({
            "ok": true,
            "passwordSet": snapshot.password_set,
            "authenticated": admission.proof.is_some(),
            "authKind": auth_kind,
            "canonicalOrigin": crate::console_auth::canonical_origin(port),
            "tokenFile": token_file,
            "consecutiveFailures": snapshot.consecutive_failures,
            "retryAfterMs": snapshot.retry_after_ms,
            "retryAfterSeconds": auth_retry_seconds(snapshot.retry_after_ms),
            "requirePasswordLocally":
                crate::console_auth::require_password_locally(&admission.config),
            "localAutoSignInAvailable": cfg!(windows),
        }),
    )
}

fn auth_first_run_route(req: &Request, context: &RuntimeContext, port: u16) -> ConsoleResponse {
    use crate::console_auth::PasswordError;
    let err = |status: u16, message: &str| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            json!({ "ok": false, "error": message }).to_string(),
        ))
    };
    if let Err((code, msg)) = browser_denial(req, port) {
        return err(code, &msg);
    }
    let fields = match parse_auth_json_object(&req.body) {
        Ok(fields) => fields,
        Err((code, msg)) => return err(code, &msg),
    };
    let Some(token) = fields
        .get("consoleToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return err(400, "缺少 consoleToken");
    };
    let Some(password) = fields.get("password").and_then(Value::as_str) else {
        return err(400, "缺少 password");
    };
    match crate::console_auth::auth_first_run_browser(context, port, token, password) {
        Ok(issued) => issue_session_cookie_response(port, issued),
        Err(PasswordError::Input) => {
            err(400, "管理员密码至少需要 8 个字符，且不能超过 1024 个字符")
        }
        Err(PasswordError::Proof) => err(401, "控制台令牌校验失败"),
        Err(PasswordError::AlreadySet) => err(409, "管理员密码已设置"),
        Err(PasswordError::Busy | PasswordError::Unavailable) => {
            err(503, "administrator password storage is unavailable")
        }
    }
}

fn auth_login_route(req: &Request, context: &RuntimeContext, port: u16) -> ConsoleResponse {
    use crate::console_auth::LoginError;
    let err = |status: u16, value: Value| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            value.to_string(),
        ))
    };
    if let Err((code, msg)) = browser_denial(req, port) {
        return err(code, json!({ "ok": false, "error": msg }));
    }
    let fields = match parse_auth_json_object(&req.body) {
        Ok(fields) => fields,
        Err((code, msg)) => return err(code, json!({ "ok": false, "error": msg })),
    };
    let Some(password) = fields.get("password").and_then(Value::as_str) else {
        return err(400, json!({ "ok": false, "error": "缺少 password" }));
    };
    match crate::console_auth::auth_login(context, port, password) {
        Ok(issued) => issue_session_cookie_response(port, issued),
        Err(LoginError::Input) => err(400, json!({ "ok": false, "error": "密码长度超出限制" })),
        Err(LoginError::NotSet) => err(
            409,
            json!({ "ok": false, "passwordSet": false, "error": "尚未设置管理员密码" }),
        ),
        Err(LoginError::WrongPassword {
            retry_after_ms,
            consecutive_failures,
        }) => err(
            401,
            json!({
                "ok": false,
                "error": "管理员密码错误",
                "consecutiveFailures": consecutive_failures,
                "retryAfterMs": retry_after_ms,
                "retryAfterSeconds": auth_retry_seconds(retry_after_ms),
            }),
        ),
        Err(LoginError::Throttled {
            retry_after_ms,
            consecutive_failures,
        }) => err(
            429,
            json!({
                "ok": false,
                "error": "尝试次数过多，请稍后再试",
                "consecutiveFailures": consecutive_failures,
                "retryAfterMs": retry_after_ms,
                "retryAfterSeconds": auth_retry_seconds(retry_after_ms),
            }),
        ),
        Err(LoginError::Busy | LoginError::Unavailable) => err(
            503,
            json!({ "ok": false, "error": "administrator authentication is unavailable" }),
        ),
    }
}

fn auth_logout_route(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
    admission: &FrontAdmission,
) -> ConsoleResponse {
    let err = |status: u16, message: &str| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            json!({ "ok": false, "error": message }).to_string(),
        ))
    };
    if let Err((code, msg)) = browser_denial(req, port) {
        return err(code, &msg);
    }
    let mut digests = Vec::new();
    match crate::console_auth::auth_logout(context, admission.proof.as_ref()) {
        Ok(Some(digest)) => digests.push(digest),
        Ok(None) => {}
        Err((code, msg)) => return err(code, &msg),
    }
    if let Ok(candidates) = browser_candidates(req, port) {
        for (digest, _) in candidates {
            if !digests
                .iter()
                .any(|existing| crate::console_auth::digests_equal(existing, &digest))
            {
                digests.push(digest);
            }
        }
    }
    let mut response = ConsoleResponse::from((
        200,
        "application/json; charset=utf-8".to_string(),
        json!({ "ok": true }).to_string(),
    ));
    for digest in digests {
        response.cookies.push(BrowserCookie::Delete { port, digest });
    }
    response
}

fn auth_change_password_route(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
    admission: &FrontAdmission,
) -> ConsoleResponse {
    use crate::console_auth::{ChangePasswordError, ConsoleAuthority};
    let err = |status: u16, message: &str| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            json!({ "ok": false, "error": message }).to_string(),
        ))
    };
    let Some(authority) = &admission.proof else {
        return err(401, "console credential is required");
    };
    let fields = match parse_auth_json_object(&req.body) {
        Ok(fields) => fields,
        Err((code, msg)) => return err(code, &msg),
    };
    let Some(current_password) = fields.get("currentPassword").and_then(Value::as_str) else {
        return err(400, "缺少 currentPassword");
    };
    let Some(new_password) = fields.get("newPassword").and_then(Value::as_str) else {
        return err(400, "缺少 newPassword");
    };
    match crate::console_auth::auth_change_password(
        context,
        port,
        authority,
        current_password,
        new_password,
    ) {
        Ok(rotated) => {
            let mut response = ConsoleResponse::from((
                200,
                "application/json; charset=utf-8".to_string(),
                json!({ "ok": true, "sessionRotated": rotated.is_some() }).to_string(),
            ));
            if let (ConsoleAuthority::Browser { grant, .. }, Some(issued)) = (authority, rotated) {
                response.cookies.push(BrowserCookie::Delete {
                    port,
                    digest: grant.id_digest,
                });
                response.cookies.push(BrowserCookie::Renew {
                    port,
                    digest: issued.id_digest,
                    id: issued.id,
                });
            }
            response
        }
        Err(ChangePasswordError::Input) => {
            err(400, "管理员密码至少需要 8 个字符，且不能超过 1024 个字符")
        }
        Err(ChangePasswordError::WrongCurrent) => err(401, "当前密码错误"),
        Err(ChangePasswordError::Revoked) => err(401, "console credential was revoked"),
        Err(ChangePasswordError::NotSet) => err(409, "尚未设置管理员密码"),
        Err(ChangePasswordError::Busy | ChangePasswordError::Unavailable) => {
            err(503, "administrator password storage is unavailable")
        }
    }
}

fn auth_sessions_list_route(
    context: &RuntimeContext,
    admission: &FrontAdmission,
) -> ConsoleResponse {
    let json_reply = |status: u16, value: Value| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            value.to_string(),
        ))
    };
    let Some(authority) = &admission.proof else {
        return json_reply(
            401,
            json!({ "ok": false, "error": "console credential is required" }),
        );
    };
    match crate::console_auth::list_browser_sessions(context, authority) {
        Ok(sessions) => json_reply(200, json!({ "ok": true, "sessions": sessions })),
        Err((code, msg)) => json_reply(code, json!({ "ok": false, "error": msg })),
    }
}

fn auth_sessions_revoke_route(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
    admission: &FrontAdmission,
) -> ConsoleResponse {
    use crate::console_auth::RevokeTarget;
    let err = |status: u16, message: &str| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            json!({ "ok": false, "error": message }).to_string(),
        ))
    };
    let Some(authority) = &admission.proof else {
        return err(401, "console credential is required");
    };
    let fields = match parse_auth_json_object(&req.body) {
        Ok(fields) => fields,
        Err((code, msg)) => return err(code, &msg),
    };
    let id_field = fields.get("id").filter(|v| !v.is_null());
    let others_field = match fields.get("others") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return err(400, "others 必须是布尔值"),
    };
    let all_field = match fields.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return err(400, "all 必须是布尔值"),
    };
    let target = match (id_field, others_field, all_field) {
        (Some(Value::String(raw)), false, false) => {
            let Some(digest) = parse_auth_digest(raw) else {
                return err(400, "无效的会话 id");
            };
            RevokeTarget::One(digest)
        }
        (None, true, false) => RevokeTarget::Others,
        (None, false, true) => RevokeTarget::All,
        _ => return err(400, "请明确指定 id、others: true 或 all: true"),
    };

    let outcome =
        match crate::console_auth::revoke_browser_sessions_authorized(context, authority, target) {
            Ok(outcome) => outcome,
            Err((code, msg)) => return err(code, &msg),
        };
    let mut response = ConsoleResponse::from((
        200,
        "application/json; charset=utf-8".to_string(),
        json!({ "ok": true, "revoked": outcome.revoked_count }).to_string(),
    ));
    if let Some(digest) = outcome.delete_own {
        response.cookies.push(BrowserCookie::Delete { port, digest });
    }
    response
}

fn handle_auth_route(
    req: &Request,
    context: &RuntimeContext,
    port: u16,
    admission: &FrontAdmission,
) -> Option<ConsoleResponse> {
    if !context.is_isolated() || !req.path.starts_with("/api/auth/") {
        return None;
    }
    let err = |status: u16, message: &str| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            json!({ "ok": false, "error": message }).to_string(),
        ))
    };
    if has_agent_marker(req) {
        return Some(err(
            403,
            "agent credential cannot access console authentication",
        ));
    }
    if bounded_post_body_too_large(req) {
        return Some(err(413, BODY_TOO_LARGE_ERROR));
    }
    let response = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/auth/status") => auth_status_route(context, port, admission),
        ("POST", "/api/auth/first-run") => auth_first_run_route(req, context, port),
        ("POST", "/api/auth/login") => auth_login_route(req, context, port),
        ("POST", "/api/auth/logout") => auth_logout_route(req, context, port, admission),
        ("POST", "/api/auth/change-password") => {
            auth_change_password_route(req, context, port, admission)
        }
        ("GET", "/api/auth/sessions") => auth_sessions_list_route(context, admission),
        ("POST", "/api/auth/sessions/revoke") => {
            auth_sessions_revoke_route(req, context, port, admission)
        }
        (
            _,
            "/api/auth/status"
            | "/api/auth/first-run"
            | "/api/auth/login"
            | "/api/auth/logout"
            | "/api/auth/change-password"
            | "/api/auth/sessions"
            | "/api/auth/sessions/revoke",
        ) => err(405, "method not allowed"),
        _ => err(404, "not found"),
    };
    Some(response)
}

fn desktop_sign_in(
    stream: Option<&TcpStream>,
    req: &Request,
    context: &RuntimeContext,
    port: u16,
) -> (u16, String, String) {
    let reply = |status, value: Value| {
        (
            status,
            "application/json; charset=utf-8".to_string(),
            value.to_string(),
        )
    };
    if let Some((status, value)) = host_denial(req, port) {
        return reply(status, value);
    }
    if req.method != "POST" {
        return reply(405, json!({"ok":false,"error":"method not allowed"}));
    }
    if !context.is_isolated() {
        return reply(
            403,
            json!({"ok":false,"error":"desktop sign-in requires a selected runtime"}),
        );
    }
    if let Err((status, error)) = crate::console_auth::ensure_credentials_ready(context) {
        return reply(status, json!({"ok":false,"error":error}));
    }
    if has_agent_marker(req) {
        return reply(
            403,
            json!({"ok":false,"error":"desktop sign-in requires the installing user"}),
        );
    }
    let Some(stream) = stream else {
        return reply(
            403,
            json!({"ok":false,"error":"desktop sign-in requires the installing user"}),
        );
    };
    #[cfg(not(windows))]
    {
        let _ = stream;
        reply(
            403,
            json!({"ok":false,"error":"desktop sign-in requires the installing user"}),
        )
    }
    #[cfg(windows)]
    {
        let is_browser = has_fetch_metadata(req)
            || ["origin", "referer", "cookie"]
                .iter()
                .any(|key| req.headers.contains_key(*key));
        let proof_input = if is_browser {
            if let Err((status, error)) = browser_denial(req, port) {
                return reply(status, json!({"ok":false,"error":error}));
            }
            match browser_candidates(req, port) {
                Ok(candidates) => crate::console_auth::DesktopSignInProof::Browser {
                    id_digests: candidates.into_iter().map(|(digest, _)| digest).collect(),
                },
                Err((status, error)) => {
                    return reply(status, json!({"ok":false,"error":error}));
                }
            }
        } else {
            crate::console_auth::DesktopSignInProof::Native {
                presented_token: presented_console_token(req),
            }
        };
        match crate::console_auth::desktop_sign_in(stream, context, port, proof_input) {
            Ok(session) => reply(200, json!({"ok":true,"session":session})),
            Err((status, error)) => reply(status, json!({"ok":false,"error":error})),
        }
    }
}

pub(crate) fn enforce_key_preflight(
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
    caller: &VerifiedCaller,
) -> Option<(u16, String, String)> {
    let preflight = route_policy(&req.method, &req.path)
        .map(|p| p.key_preflight)
        .unwrap_or(KeyPreflight::SessionKey);
    let has_agent_header = req.headers.contains_key("x-agent-token");
    let defer = match preflight {
        KeyPreflight::None => return None,
        KeyPreflight::SessionKey | KeyPreflight::SessionKeyOrClosedOwnerReset => false,
        KeyPreflight::SessionKeyDeferAgentHeader => has_agent_header,
        KeyPreflight::SessionKeyDeferMemoryAgent => memory_agent_call(req),
        KeyPreflight::SessionKeyDeferAlways => true,
        KeyPreflight::SessionKeyDeferIsolated => context.is_isolated(),
    };
    if defer {
        return None;
    }
    let has_credential = caller.is_admitted()
        || req
            .headers
            .get("x-agent-token")
            .is_some_and(|t| !t.trim().is_empty());
    if has_credential {
        if let Some(key) = request_session_key(req) {
            let closed_owner_reset = preflight == KeyPreflight::SessionKeyOrClosedOwnerReset
                && context.is_isolated()
                && crate::social_state::mode(context) == "closed-agent"
                && crate::chat::is_owner_key(cfg, &key);
            if !crate::social_state::key_in_allowlist(cfg, &key) && !closed_owner_reset {
                return Some((
                    403,
                    "application/json; charset=utf-8".to_string(),
                    json!({"ok": false, "error": format!("会话不在允许范围内：{key}")})
                        .to_string(),
                ));
            }
        }
    }
    None
}

fn v2_agent_global_gates(
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
) -> Option<(u16, String)> {
    let token = req.headers.get("x-agent-token")?;
    if token.is_empty() {
        return Some((403, "agent token 不能为空".to_string()));
    }
    let doc = match ConversationState::for_runtime(context).read_document() {
        Ok(doc) => doc,
        Err((status, message)) => return Some((status, message)),
    };
    if doc.paused() {
        return Some((403, "当前已暂停，不允许执行 v2 工具".to_string()));
    }
    if cfg
        .get("socialV2")
        .and_then(|social| social.get("enabled"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Some((403, "二代模式已关闭，当前不允许执行 v2 工具".to_string()));
    }
    match current_mode_for(context) {
        Ok(mode) if mode == "reserved2" => None,
        Ok(_) => Some((403, "该接口仅 reserved2 模式可用".to_string())),
        Err((status, message)) => Some((status, message)),
    }
}

pub(super) fn policy_agent_denial(
    policy: Option<RoutePolicy>,
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
) -> Option<(u16, String)> {
    let policy = policy?;
    match policy.agent {
        AgentPolicy::AgentAllowed | AgentPolicy::AdminOnly(_) => None,
        AgentPolicy::PendingAdminOnly | AgentPolicy::V1StickerAdmin => {
            let token = req.headers.get("x-agent-token")?;
            if token.is_empty() {
                Some((403, "agent token 不能为空".to_string()))
            } else {
                Some((403, "该接口仅控制台可用".to_string()))
            }
        }
        AgentPolicy::V2AdminOnly => {
            if !req.headers.contains_key("x-agent-token") {
                return None;
            }
            v2_agent_global_gates(req, cfg, context)
                .or_else(|| Some((403, "该接口仅控制台可用".to_string())))
        }
        AgentPolicy::V2AgentAllowed => v2_agent_global_gates(req, cfg, context),
    }
}

pub(crate) fn enforce_agent_denials(
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
    caller: &VerifiedCaller,
) -> Option<(u16, String, String)> {
    let reply = |status, message: String| {
        (
            status,
            "application/json; charset=utf-8".to_string(),
            json!({"ok": false, "error": message}).to_string(),
        )
    };
    let policy = route_policy(&req.method, &req.path);
    if let Some(RoutePolicy {
        agent: AgentPolicy::AdminOnly(message),
        ..
    }) = policy
    {
        if caller.is_agent() {
            return Some(reply(403, message.to_string()));
        }
    }
    if let Some((status, message)) = agent::agent_global_denial(req, cfg, context) {
        return Some(reply(status, message));
    }
    if let Some((status, message)) = policy_agent_denial(policy, req, cfg, context) {
        return Some(reply(status, message));
    }
    None
}

fn ui_asset_response(asset: &'static crate::console_ui::EmbeddedAsset) -> ConsoleResponse {
    let mut response = ConsoleResponse::from((
        200,
        asset.content_type.to_string(),
        String::from_utf8(asset.bytes.to_vec()).unwrap_or_default(),
    ));
    response.raw_body = Some(asset.bytes);
    response.csp = Some(crate::console_ui::STRICT_CSP);
    response
}

fn legacy_page_cookie_mac(expected_token: &str, port: u16) -> Option<[u8; 32]> {
    if expected_token.is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"her-legacy-ui:");
    hasher.update(port.to_le_bytes());
    hasher.update(b":");
    hasher.update(expected_token.as_bytes());
    Some(hasher.finalize().into())
}

fn legacy_page_cookie_matches(req: &Request, expected_token: &str, port: u16) -> bool {
    let Some(mac) = legacy_page_cookie_mac(expected_token, port) else {
        return false;
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let expected = URL_SAFE_NO_PAD.encode(mac);
    let name = format!("her_legacy_{port}");
    req.headers.get("cookie").is_some_and(|raw| {
        raw.split(';').any(|pair| {
            let Some((k, v)) = pair.trim().split_once('=') else {
                return false;
            };
            k.trim() == name && credential_eq(&expected, v.trim())
        })
    })
}

fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn ui_page_response(
    req: &Request,
    renewal: Option<BrowserCookie>,
    preserve_token: bool,
) -> Option<ConsoleResponse> {
    if req.method != "GET" {
        return None;
    }
    if let Some(target) = crate::console_ui::legacy_redirect(&req.path) {
        let redirect_target = if preserve_token {
            if let Some(tok) = req.query.get("token") {
                format!("{target}?token={}", percent_encode_query_value(tok))
            } else {
                target.to_string()
            }
        } else {
            target.to_string()
        };
        let mut response = ConsoleResponse::from((302, String::new(), String::new()));
        response.redirect = Some(ConsoleRedirect::Path(redirect_target));
        response.cookies = renewal.into_iter().collect();
        return Some(response);
    }
    if crate::console_ui::is_spa_page_path(&req.path) {
        if let Some(asset) = crate::console_ui::resolve_asset("/index.html") {
            let mut response = ui_asset_response(asset);
            response.cookies = renewal.into_iter().collect();
            return Some(response);
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    static TEST_PEER_SAME_USER: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

fn peer_is_installing_user(stream: Option<&TcpStream>) -> bool {
    #[cfg(test)]
    if let Some(override_same) = TEST_PEER_SAME_USER.with(|cell| cell.get()) {
        return override_same;
    }
    stream.is_some_and(crate::console_auth::same_installing_user)
}

fn eligible_for_local_auto_sign_in(req: &Request, port: u16, admission: &FrontAdmission) -> bool {
    if !crate::console_ui::is_ui_page_request(&req.method, &req.path)
        || matches!(req.path.as_str(), "/login" | "/setup")
    {
        return false;
    }
    if !canonical_request(req, port) || admission.proof.is_some() {
        return false;
    }
    if has_agent_marker(req)
        || req.headers.contains_key("x-console-token")
        || req.query.contains_key("token")
    {
        return false;
    }
    if req
        .headers
        .get("sec-fetch-site")
        .is_some_and(|site| !matches!(site.as_str(), "same-origin" | "none"))
    {
        return false;
    }
    let expected_origin = crate::console_auth::canonical_origin(port);
    if req
        .headers
        .get("origin")
        .filter(|origin| !origin.is_empty())
        .is_some_and(|origin| origin != &expected_origin)
    {
        return false;
    }
    !crate::console_auth::require_password_locally(&admission.config)
}

/// One HTTP request through host/origin guards, credential admission, audit wrapper and routing.
pub(crate) fn handle_http_request(
    stream: Option<&TcpStream>,
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
    port: u16,
) -> ConsoleResponse {
    if req.path == "/api/desktop/sign-in" {
        return desktop_sign_in(stream, req, context, port).into();
    }
    let json_reply = |status: u16, value: Value| {
        ConsoleResponse::from((
            status,
            "application/json; charset=utf-8".to_string(),
            value.to_string(),
        ))
    };
    let mut admitted_context = context.clone().with_console_port(port);
    let public = is_public_probe(&req.method, &req.path);
    if !public
        && (context.is_isolated() || (req.method == "GET" && req.path.starts_with("/assets/")))
    {
        if let Some((status, value)) = host_denial(req, port) {
            return json_reply(status, value);
        }
    }
    let mut admission = None;
    if !public && req.method == "GET" && req.path.starts_with("/assets/") {
        if has_agent_marker(req) {
            return json_reply(
                403,
                json!({
                    "ok": false,
                    "error": "console UI routes are restricted to administrators",
                }),
            );
        }
        return match crate::console_ui::resolve_asset(&req.path) {
            Some(asset) => ui_asset_response(asset),
            None => json_reply(404, json!({"ok": false, "error": "not found"})),
        };
    }
    if context.is_isolated() && !public {
        let mut isolated_admission = match admit_selected_request(req, context, port) {
            Ok(admission) => admission,
            Err((status, error)) => return json_reply(status, json!({"ok":false,"error":error})),
        };
        let desktop = matches!(
            isolated_admission.proof,
            Some(crate::console_auth::ConsoleAuthority::Desktop { .. })
        );
        if crate::console_ui::is_ui_page_request(&req.method, &req.path) {
            if has_agent_marker(req) {
                return json_reply(
                    403,
                    json!({
                        "ok": false,
                        "error": "console UI routes are restricted to administrators",
                    }),
                );
            }
            if !canonical_request(req, port) && !desktop {
                let target = crate::console_ui::legacy_redirect(&req.path)
                    .unwrap_or(&req.path)
                    .to_string();
                let mut response = ConsoleResponse::from((302, String::new(), String::new()));
                response.redirect = Some(ConsoleRedirect::CanonicalOrigin(port, target));
                return response;
            }
        }
        if eligible_for_local_auto_sign_in(req, port, &isolated_admission)
            && peer_is_installing_user(stream)
        {
            match crate::console_auth::auth_local_browser_auto_sign_in(context, port) {
                Ok(Some((issued, authority))) => {
                    isolated_admission.proof = Some(authority);
                    isolated_admission.renewal = Some(BrowserCookie::Renew {
                        port,
                        digest: issued.id_digest,
                        id: issued.id,
                    });
                }
                Ok(None) => {}
                Err((status, error)) => {
                    return json_reply(status, json!({"ok":false,"error":error}))
                }
            }
        }
        admitted_context.console_authority = isolated_admission.proof.clone();
        admission = Some(isolated_admission);
    }
    #[cfg(test)]
    if let Some(hook) = AFTER_ADMISSION.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let agent_marker = has_agent_marker(req);
    if let Some(admission_state) = admission.as_mut() {
        if crate::console_ui::is_ui_page_request(&req.method, &req.path) {
            if presented_console_token(req).is_some()
                && admitted_context.console_authority.is_none()
            {
                return json_reply(
                    401,
                    json!({"ok": false, "error": "未授权：请提供控制台访问令牌"}),
                );
            }
            if let Some(response) = ui_page_response(req, admission_state.renewal.take(), false) {
                return response;
            }
        }
    }
    let kind = if admission.is_some() {
        if agent_marker {
            CallerKind::Agent {
                console_credential: admitted_context.console_authority.is_some(),
            }
        } else if admitted_context.console_authority.is_some() {
            CallerKind::Administrator
        } else {
            CallerKind::Unauthenticated
        }
    } else {
        let console_credential = authorized(req, cfg);
        if agent_marker {
            CallerKind::Agent { console_credential }
        } else if console_credential {
            CallerKind::Administrator
        } else {
            CallerKind::Unauthenticated
        }
    };
    if admission.is_none() && crate::console_ui::is_ui_page_request(&req.method, &req.path) {
        if agent_marker {
            return json_reply(
                403,
                json!({
                    "ok": false,
                    "error": "console UI routes are restricted to administrators",
                }),
            );
        }
        let expected = console_token(cfg);
        let explicit_token = presented_console_token(req).is_some();
        let cookie_ok = !explicit_token && legacy_page_cookie_matches(req, &expected, port);
        if !(matches!(kind, CallerKind::Administrator) || cookie_ok) {
            return json_reply(
                401,
                json!({"ok": false, "error": "未授权：请提供控制台访问令牌"}),
            );
        }
        let session_cookie = if matches!(kind, CallerKind::Administrator) {
            legacy_page_cookie_mac(&expected, port)
                .map(|mac| BrowserCookie::LegacyPage { port, mac })
        } else {
            None
        };
        if let Some(response) = ui_page_response(req, session_cookie, explicit_token) {
            return response;
        }
    }
    if admission.is_none() && req.method == "POST" && req.path == "/api/auth/logout" {
        if agent_marker {
            return json_reply(
                403,
                json!({
                    "ok": false,
                    "error": "console UI routes are restricted to administrators",
                }),
            );
        }
        if let Some((status, err)) = cross_site_denial(req, port) {
            return json_reply(status, err);
        }
        let expected = console_token(cfg);
        let explicit_token = presented_console_token(req).is_some();
        let cookie_ok = !explicit_token && legacy_page_cookie_matches(req, &expected, port);
        if matches!(kind, CallerKind::Administrator) || cookie_ok {
            let mut response = ConsoleResponse::from((
                200,
                "application/json; charset=utf-8".to_string(),
                json!({"ok": true}).to_string(),
            ));
            response.cookies = vec![BrowserCookie::LegacyPageDelete { port }];
            return response;
        }
        return json_reply(
            401,
            json!({"ok": false, "error": "未授权：请提供控制台访问令牌"}),
        );
    }
    let administrator = admitted_context.console_authority.is_some()
        && matches!(kind, CallerKind::Administrator)
        && !matches!(
            req.path.as_str(),
            "/api/authorize/read" | "/api/socialV2/wake-snapshot"
        );
    let task_log_read = req.method == "GET"
        && req.path.starts_with("/api/tasks/runs/")
        && req.path.ends_with("/log");
    let audit = (req.method != "GET" && req.path.starts_with("/api/"))
        || (admitted_context.is_isolated() && task_log_read);
    let before = if audit && !task_log_read {
        match audit_snapshot(&admitted_context) {
            Ok(snapshot) => snapshot,
            Err(message) => return json_reply(503, json!({ "ok": false, "error": message })),
        }
    } else {
        Vec::new()
    };
    let (audit_context, status, ctype, mut body, auth_cookies, renewal, config_diff) =
        if let Some(mut admission) = admission {
            if let Some(auth_response) = handle_auth_route(req, &admitted_context, port, &admission)
            {
                let renewal = admission.renewal.take();
                (
                    admitted_context,
                    auth_response.status,
                    auth_response.content_type,
                    auth_response.body,
                    auth_response.cookies,
                    renewal,
                    None,
                )
            } else {
                let renewal = admission.renewal.take();
                let mut caller = VerifiedCaller {
                    context: admitted_context,
                    config: admission.config,
                    kind,
                    memory: admission.memory,
                    browser_acl: admission.browser_acl,
                    config_diff: None,
                };
                let (status, ctype, body) = route_with_caller(&mut caller, req);
                let config_diff = caller.config_diff.take();
                (
                    caller.context,
                    status,
                    ctype,
                    body,
                    Vec::new(),
                    renewal,
                    config_diff,
                )
            }
        } else {
            let mut caller = VerifiedCaller {
                context: admitted_context,
                config: cfg.clone(),
                kind,
                memory: crate::memory_actions::ConsoleCapability::Conversation,
                browser_acl: None,
                config_diff: None,
            };
            let (status, ctype, body) = route_with_caller(&mut caller, req);
            let config_diff = caller.config_diff.take();
            (
                caller.context,
                status,
                ctype,
                body,
                Vec::new(),
                None,
                config_diff,
            )
        };
    let record_admin = audit && administrator;
    if audit && ((200..300).contains(&status) || record_admin) {
        if let Err(error) = audit_write(
            &audit_context,
            req,
            status,
            &body,
            record_admin,
            &before,
            config_diff,
        ) {
            crate::safe_println!(
                "[console] ⚠️ 审计记录写入失败（{} {}，状态 {status}）：{error}",
                req.method,
                req.path
            );
            if audit_context.is_isolated() {
                if let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(&body) {
                    reply.insert(
                        "audit".into(),
                        json!({"ok": false, "error": format!("审计记录写入失败，此变更无法撤销：{error}")}),
                    );
                    body = Value::Object(reply).to_string();
                }
            }
        }
    }
    let delete_on_ok = route_policy(&req.method, &req.path)
        .is_some_and(|policy| policy.session_cookie == SessionCookiePolicy::DeleteOnSuccess);
    let mut response = ConsoleResponse::from((status, ctype, body));
    if !auth_cookies.is_empty() {
        response.cookies = auth_cookies;
    } else if (200..300).contains(&status) && delete_on_ok {
        if let Some(crate::console_auth::ConsoleAuthority::Browser { grant, .. }) =
            &audit_context.console_authority
        {
            response.cookies.push(BrowserCookie::Delete {
                port,
                digest: grant.id_digest,
            });
        }
    } else if let Some(renewal) = renewal {
        response.cookies.push(renewal);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarative_route_policy_table_invariants_hold() {
        let mut seen = HashSet::new();
        for (method, path, policy) in EXACT_ROUTES {
            assert!(seen.insert((*method, *path)), "duplicate route {method} {path}");
            if matches!(policy.agent, AgentPolicy::AdminOnly(_)) {
                assert_eq!(
                    policy.key_preflight,
                    KeyPreflight::None,
                    "admin-only route must skip session key preflight: {method} {path}"
                );
            }
            if policy.public_probe {
                assert!(policy.isolated_supported);
                assert_eq!(policy.key_preflight, KeyPreflight::None);
                assert_eq!(policy.body_limit, BodyLimit::None);
                assert_eq!(policy.session_cookie, SessionCookiePolicy::NoRenew);
            }
        }
        assert_eq!(
            route_policy("POST", "/api/console/token").unwrap().session_cookie,
            SessionCookiePolicy::DeleteOnSuccess
        );
        for (method, path) in [
            ("GET", "/api/auth/status"),
            ("POST", "/api/auth/first-run"),
            ("POST", "/api/auth/login"),
            ("POST", "/api/auth/logout"),
            ("POST", "/api/auth/change-password"),
            ("POST", "/api/auth/sessions/revoke"),
            ("POST", "/api/desktop/sign-in"),
        ] {
            assert_eq!(
                route_policy(method, path).unwrap().session_cookie,
                SessionCookiePolicy::NoRenew,
                "{method} {path} must not renew browser cookie"
            );
        }
        assert_eq!(
            route_policy("GET", "/api/auth/sessions").unwrap().session_cookie,
            SessionCookiePolicy::Renew
        );
        for (method, path) in [
            ("POST", "/api/settings/set"),
            ("POST", "/api/tasks/backup/run"),
            ("POST", "/api/schedule"),
            ("POST", "/api/backups/restore"),
            ("POST", "/api/releases/approve"),
            ("POST", "/api/dependencies/approve"),
            ("GET", "/api/memory/acl"),
            ("POST", "/api/memory/acl"),
            ("POST", "/api/restart"),
            ("POST", "/api/auth/login"),
            ("POST", "/api/roles/create"),
            ("POST", "/api/socialV2/send-message"),
        ] {
            assert!(
                is_bounded_json_request(method, path),
                "{method} {path} must be 1MB bounded"
            );
        }
        assert!(!is_bounded_json_request("GET", "/api/settings"));
        assert!(!is_bounded_json_request("POST", "/api/session-models"));
    }

    #[test]
    fn isolated_send_message_route_allowlist_is_exact() {
        let (_root, isolated) = super::super::tests::fixture_runtime(
            "send-message-route-boundary",
            "fixture-front-token",
        );
        assert!(supports_request(
            &isolated,
            "POST",
            "/api/socialV2/send-message"
        ));
        assert!(supports_request(
            &isolated,
            "GET",
            "/api/socialV2/sticker-image"
        ));
        for path in [
            "/api/socialV2/send-voice",
            "/api/voice/send",
            "/api/send/voice",
        ] {
            assert!(supports_request(&isolated, "POST", path), "{path}");
        }
        for (method, path) in [
            ("GET", "/api/socialV2/send-message"),
            ("POST", "/api/socialV2/send-message/"),
            ("POST", "/api/socialV2/send-message-extra"),
            ("POST", "/api/socialV2/send-poke/"),
            ("GET", "/api/socialV2/send-poke"),
            ("GET", "/api/send/group"),
            ("GET", "/api/send/voice"),
        ] {
            assert!(
                !supports_request(&isolated, method, path),
                "isolated route unexpectedly allowed {method} {path}"
            );
        }
        let legacy = RuntimeContext::legacy();
        assert!(supports_request(
            &legacy,
            "GET",
            "/api/socialV2/send-message"
        ));
        assert!(supports_request(&legacy, "POST", "/api/socialV2/send-poke"));
    }

    #[test]
    fn isolated_memory_http_allowlist_exposes_only_context_safe_actions() {
        let (_root, context) =
            super::super::tests::fixture_runtime("memory-http-allowlist", "fixture-front-token");
        for action in [
            "write",
            "search",
            "forget",
            "index",
            "preview",
            "acl",
            "settings-get",
            "settings-set",
        ] {
            assert!(supports_request(
                &context,
                "POST",
                &format!("/api/memory/{action}")
            ));
            assert!(supports_request(
                &context,
                "GET",
                &format!("/api/memory/{action}")
            ));
        }
        for (method, path) in [
            ("GET", "/api/memory/archive"),
            ("GET", "/api/memory/settings"),
            ("GET", "/api/memory/acl/"),
            ("GET", "/api/memory/"),
            ("DELETE", "/api/memory/write"),
            ("PUT", "/api/memory/acl"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
        assert!(supports_request(&context, "POST", "/api/memory/archive"));
    }

    #[test]
    fn isolated_send_message_http_allowlist_is_method_and_path_exact() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "send-message-http-allowlist",
            "fixture-front-token",
        );
        assert!(supports_request(
            &context,
            "POST",
            "/api/socialV2/send-message"
        ));
        for (method, path) in [
            ("GET", "/api/socialV2/send-message"),
            ("POST", "/api/socialV2/send-message/"),
            ("POST", "/api/socialV2/send-burst/"),
            ("POST", "/api/socialV2/feedback/"),
            ("POST", "/api/socialV2/check-send"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
    }

    #[test]
    fn isolated_sticker_list_http_allowlist_is_method_path_and_query_exact() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "sticker-list-http-allowlist",
            "fixture-front-token",
        );
        for path in ["/api/stickers", "/api/socialV2/sticker-list"] {
            assert!(supports_request(&context, "GET", path));
        }
        assert!(supports_request(&context, "POST", "/api/stickers/note"));
        assert!(supports_request(&context, "POST", "/api/stickers/sync"));
        assert!(supports_request(&context, "POST", "/api/stickers/remark"));
        for path in [
            "/api/socialV2/sticker-list",
            "/api/socialV2/sticker-note",
            "/api/socialV2/sticker-remark",
            "/api/socialV2/send-sticker",
            "/api/socialV2/collect-sticker",
        ] {
            assert!(supports_request(&context, "POST", path), "POST {path}");
        }
        for (method, path) in [
            ("POST", "/api/stickers"),
            ("HEAD", "/api/stickers"),
            ("OPTIONS", "/api/stickers"),
            ("HEAD", "/api/socialV2/sticker-list"),
            ("OPTIONS", "/api/socialV2/sticker-list"),
            ("GET", "/api/stickers/"),
            ("GET", "/api/socialV2/sticker-list/"),
            ("GET", "/api/stickers/sync"),
            ("GET", "/api/stickers/note"),
            ("GET", "/api/stickers/remark"),
            ("POST", "/api/stickers/remark/"),
            ("POST", "/api/stickers/remark?x=1"),
            ("GET", "/api/stickers/unknown"),
            ("GET", "/api/stickers2"),
            ("GET", "/api/socialV2/sticker-note"),
            ("GET", "/api/stickers?query=cat&count=12"),
            ("GET", "/api/socialV2/sticker-list?query=cat&count=12"),
            ("GET", "/api/socialV2/sticker-lists"),
            ("GET", "/api/socialV2/sticker-list-extra"),
            ("GET", "/api/socialV2/unknown"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
        let legacy = RuntimeContext::legacy();
        assert!(supports_request(&legacy, "POST", "/api/stickers/note"));
        assert!(supports_request(
            &legacy,
            "POST",
            "/api/socialV2/sticker-list"
        ));
    }

    #[test]
    fn isolated_mode_switch_route_is_post_only() {
        let (_root, context) =
            super::super::tests::fixture_runtime("mode-switch-allowlist", "fixture-front-token");
        assert!(supports_request(&context, "POST", "/api/mode"));
        for (method, path) in [
            ("GET", "/api/mode"),
            ("PUT", "/api/mode"),
            ("POST", "/api/mode/"),
            ("POST", "/api/mode/extra"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
    }

    #[test]
    fn isolated_role_management_http_allowlist_is_method_and_path_exact() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "role-management-http-allowlist",
            "fixture-front-token",
        );
        for path in [
            "/api/roles/update",
            "/api/roles/rename",
            "/api/roles/delete",
            "/api/roles/limit",
            "/api/role",
            "/api/role-mode",
        ] {
            assert!(
                supports_request(&context, "POST", path),
                "expected POST {path}"
            );
        }
        for (method, path) in [
            ("GET", "/api/roles/update"),
            ("PUT", "/api/roles/update"),
            ("GET", "/api/roles/rename"),
            ("POST", "/api/roles/rename/"),
            ("POST", "/api/roles/delete/extra"),
            ("GET", "/api/roles/limit"),
            ("POST", "/api/role/"),
            ("GET", "/api/role-mode"),
            ("POST", "/api/role-mode/extra"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
    }

    #[test]
    fn isolated_usage_and_social_memory_state_allowlist_is_method_and_path_exact() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "usage-social-http-allowlist",
            "fixture-front-token",
        );
        for (method, path) in [
            ("GET", "/api/tokens/by-provider"),
            ("GET", "/api/usage/attribution"),
            ("GET", "/api/tokens/summary"),
            ("GET", "/api/tokens/series"),
            ("GET", "/api/tokens/turns"),
            ("GET", "/api/tokens/recent"),
            ("GET", "/api/socialV2/memory"),
            ("POST", "/api/socialV2/memory-append"),
            ("POST", "/api/socialV2/memory-update"),
            ("POST", "/api/socialV2/memory-remove"),
            ("POST", "/api/socialV2/memory-clear"),
            ("GET", "/api/socialV2/config"),
            ("POST", "/api/socialV2/config"),
            ("GET", "/api/socialV2/state"),
            ("GET", "/api/socialV2/states"),
            ("GET", "/api/socialV2/activity"),
            ("POST", "/api/socialV2/activity"),
            ("GET", "/api/socialV2/unread"),
            ("GET", "/api/socialV2/prompt"),
            ("GET", "/api/socialV2/initial-prompt"),
            ("POST", "/api/socialV2/initial-prompt"),
            ("GET", "/api/socialV2/recent"),
            ("GET", "/api/socialV2/my-recent"),
            ("GET", "/api/socialV2/active-members"),
            ("GET", "/api/socialV2/send-outbox"),
            ("GET", "/api/sessions"),
            ("GET", "/api/pending"),
            ("POST", "/api/session/reset"),
            ("GET", "/api/turns"),
            ("POST", "/api/turns/resolve"),
            ("GET", "/api/recovery-record"),
            ("POST", "/api/restart"),
            ("POST", "/api/whitelist"),
            ("GET", "/api/security"),
            ("POST", "/api/security"),
            ("POST", "/api/console/token"),
            ("POST", "/api/authorize/read"),
            ("GET", "/api/audit/recent"),
            ("POST", "/api/audit/undo"),
            ("POST", "/api/socialV2/reset"),
            ("POST", "/api/socialV2/wake"),
            ("POST", "/api/desktop/sign-in"),
        ] {
            assert!(
                supports_request(&context, method, path),
                "expected {method} {path}"
            );
        }
        for (method, path) in [
            ("POST", "/api/tokens/reset"),
            ("POST", "/api/tokens/summary"),
            ("GET", "/api/tokens/summary/"),
            ("GET", "/api/tokens/unknown"),
            ("GET", "/api/usage/attribution/extra"),
            ("POST", "/api/socialV2/memory"),
            ("GET", "/api/socialV2/memory-append"),
            ("POST", "/api/socialV2/memory-append/"),
            ("PUT", "/api/socialV2/config"),
            ("POST", "/api/socialV2/state"),
            ("PUT", "/api/socialV2/activity"),
            ("GET", "/api/socialV2/mark-read"),
            ("GET", "/api/socialV2/wake-config"),
            ("GET", "/api/socialV2/states/"),
            ("GET", "/api/pending/"),
            ("POST", "/api/pending"),
            ("GET", "/api/turns/"),
            ("GET", "/api/session/reset"),
            ("POST", "/api/sessions"),
            ("PUT", "/api/socialV2/initial-prompt"),
            ("POST", "/api/socialV2/prompt"),
            ("GET", "/api/socialV2/initial-prompt/"),
            ("GET", "/api/audit/recent/"),
            ("GET", "/api/audit/undo"),
            ("PUT", "/api/security"),
            ("GET", "/api/console/token"),
            ("GET", "/api/socialV2/reset"),
            ("GET", "/api/socialV2/wake"),
            ("POST", "/api/socialV2/wake/"),
            ("POST", "/api/workspace/reset"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
        let legacy = RuntimeContext::legacy();
        assert!(supports_request(&legacy, "POST", "/api/tokens/reset"));
        assert!(supports_request(&legacy, "GET", "/api/pending"));
    }

    #[test]
    fn isolated_resource_http_allowlist_is_method_and_path_exact() {
        let (_root, context) =
            super::super::tests::fixture_runtime("resource-http-allowlist", "fixture-front-token");
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/favicon.ico"),
            ("GET", "/apple-touch-icon.png"),
            ("GET", "/api/voice/list"),
            ("GET", "/api/socialV2/voices"),
            ("GET", "/api/socialV2/tool-log"),
            ("POST", "/api/socialV2/tool-log/clear"),
        ] {
            assert!(
                supports_request(&context, method, path),
                "expected {method} {path}"
            );
        }
        for (method, path) in [
            ("POST", "/"),
            ("GET", "/favicon.ico/"),
            ("POST", "/apple-touch-icon.png"),
            ("POST", "/api/voice/list"),
            ("POST", "/api/socialV2/voices"),
            ("POST", "/api/socialV2/tool-log"),
            ("GET", "/api/socialV2/tool-log/clear"),
            ("POST", "/api/socialV2/tool-log/clear/"),
            ("GET", "/api/voice/list/extra"),
        ] {
            assert!(
                !supports_request(&context, method, path),
                "unexpectedly opened {method} {path}"
            );
        }
    }

    #[test]
    fn desktop_sign_in_passes_through_console_front_guards() {
        let (root, context) =
            super::super::tests::fixture_runtime("front desktop guards", "fixture-front-token");
        let make_req = |method: &str, host: &str, agent: Option<&str>| Request {
            method: method.to_string(),
            path: "/api/desktop/sign-in".to_string(),
            query: HashMap::new(),
            headers: {
                let mut h = HashMap::from([("host".to_string(), host.to_string())]);
                if let Some(a) = agent {
                    h.insert("x-agent-token".to_string(), a.to_string());
                }
                h
            },
            body: String::new(),
            body_too_large: false,
        };
        let bad_host = handle_http_request(
            None,
            &make_req("POST", "evil.example:3999", None),
            &json!({}),
            &context,
            3999,
        );
        assert_eq!(bad_host.status, 403);
        let bad_method = handle_http_request(
            None,
            &make_req("GET", "127.0.0.1:3999", None),
            &json!({}),
            &context,
            3999,
        );
        assert_eq!(bad_method.status, 405);
        let agent = handle_http_request(
            None,
            &make_req("POST", "127.0.0.1:3999", Some("agent-tok")),
            &json!({}),
            &context,
            3999,
        );
        assert_eq!(agent.status, 403);
        let synthetic = handle_http_request(
            None,
            &make_req("POST", "127.0.0.1:3999", None),
            &json!({}),
            &context,
            3999,
        );
        assert_eq!(synthetic.status, 403);

        let rotation_marker = root.0.join("data").join(crate::console_auth::ROTATION_FILE);
        std::fs::write(&rotation_marker, "{}").unwrap();
        let pending = handle_http_request(
            None,
            &make_req("POST", "127.0.0.1:3999", None),
            &json!({}),
            &context,
            3999,
        );
        let _ = std::fs::remove_file(&rotation_marker);
        assert_eq!(pending.status, 503);
    }

    #[test]
    fn front_refuses_agents_on_admin_only_routes_and_scopes_key_preflight() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "front admin and preflight",
            "fixture-front-token",
        );
        let cfg = context.load_config().unwrap();
        let admin_call = |method: &str,
                          path: &str,
                          query: HashMap<String, String>,
                          body: &str,
                          agent: Option<&str>| {
            let mut headers = HashMap::from([
                (
                    "host".to_string(),
                    crate::console_auth::canonical_host(3999),
                ),
                (
                    "x-console-token".to_string(),
                    context.effective_console_token(&cfg),
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ]);
            if let Some(a) = agent {
                headers.insert("x-agent-token".to_string(), a.to_string());
            }
            handle_http_request(
                None,
                &Request {
                    method: method.to_string(),
                    path: path.to_string(),
                    query,
                    headers,
                    body: body.to_string(),
                    body_too_large: false,
                },
                &cfg,
                &context,
                3999,
            )
        };

        for (method, path) in [
            ("GET", "/tasks"),
            ("GET", "/api/tasks"),
            ("POST", "/api/tasks/backup/run"),
            ("GET", "/schedule"),
            ("GET", "/api/schedule"),
            ("GET", "/settings"),
            ("GET", "/api/settings"),
            ("GET", "/personas"),
            ("GET", "/api/personas"),
            ("POST", "/api/restart"),
            ("GET", "/api/backups"),
            ("GET", "/api/releases"),
            ("GET", "/api/dependencies"),
            ("GET", "/api/qq/login"),
            ("GET", "/api/auth/status"),
        ] {
            let resp = admin_call(method, path, HashMap::new(), "{}", Some("agent-tok-123"));
            assert_eq!(resp.status, 403, "{method} {path}: {}", resp.body);
        }

        let unallowed_key_query = HashMap::from([("key".to_string(), "group:999999".to_string())]);
        for (method, path, body) in [
            ("GET", "/api/settings", ""),
            ("GET", "/api/personas", ""),
            ("GET", "/api/schedule", ""),
            ("GET", "/api/tasks", ""),
            (
                "POST",
                "/api/conversations/scope",
                r#"{"key":"group:999999","conversation":"group:999999","action":"admit"}"#,
            ),
        ] {
            let resp = admin_call(method, path, unallowed_key_query.clone(), body, None);
            assert!(
                !resp.body.contains("会话不在允许范围内"),
                "{method} {path}: {}",
                resp.body
            );
        }

        let session_resp = admin_call(
            "GET",
            "/api/socialV2/state",
            unallowed_key_query.clone(),
            "",
            None,
        );
        assert_eq!(session_resp.status, 403);
        assert!(
            session_resp.body.contains("会话不在允许范围内"),
            "{}",
            session_resp.body
        );

        let models_resp = admin_call(
            "POST",
            "/api/session-models",
            HashMap::new(),
            r#"{"key":"group:999999","chain":[]}"#,
            None,
        );
        assert_eq!(models_resp.status, 403);
        assert!(
            models_resp.body.contains("会话不在允许范围内"),
            "{}",
            models_resp.body
        );

        assert!(supports_request(&context, "GET", "/api/tasks"));
        assert!(!supports_request(&context, "POST", "/api/tasks"));
    }

    #[test]
    fn admin_caller_gate_enforces_revocation_across_modules() {
        let (_root, context) =
            super::super::tests::fixture_runtime("front admin caller gate", "fixture-front-token");
        let revoked = revoked_admin_context(&context);
        let caller = AdminCaller::new(&revoked);
        assert_eq!(caller.gate().unwrap_err().0, 401);

        let (s_set, _, b_set, _) = crate::console_settings::route(
            caller,
            "POST",
            "/api/settings/set",
            &HashMap::new(),
            r#"{"setting":"reply.wakeQuietMs","value":4500}"#,
        );
        assert_eq!(s_set, 401, "{b_set}");

        let (s_per, _, b_per) = crate::console_persona::route(
            caller,
            "POST",
            "/api/personas/create",
            &HashMap::new(),
            r#"{"name":"甲","core":"核心。"}"#,
        );
        assert_eq!(s_per, 401, "{b_per}");

        let mut sched_req = super::super::tests::req("POST", "/api/schedule", None);
        sched_req.body =
            r#"{"conversation":"group:420042","text":"ping","at":"2026-11-22T12:00:00Z"}"#
                .to_string();
        let (s_sched, _, b_sched) = super::super::schedule_routes::route(
            caller,
            &context.load_config().unwrap(),
            &sched_req,
        );
        assert_eq!(s_sched, 401, "{b_sched}");
    }

    #[test]
    fn isolated_spa_assets_csp_and_legacy_redirects_enforce_front_contract() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "front spa csp and redirects",
            "fixture-front-token",
        );
        let cfg = context.load_config().unwrap();
        let make_get = |host: &str, path: &str, token: Option<&str>, agent: Option<&str>| {
            let mut headers = HashMap::from([("host".to_string(), host.to_string())]);
            if let Some(tok) = token {
                headers.insert("x-console-token".to_string(), tok.to_string());
            }
            if let Some(ag) = agent {
                headers.insert("x-agent-token".to_string(), ag.to_string());
            }
            handle_http_request(
                None,
                &Request {
                    method: "GET".to_string(),
                    path: path.to_string(),
                    query: HashMap::new(),
                    headers,
                    body: String::new(),
                    body_too_large: false,
                },
                &cfg,
                &context,
                3999,
            )
        };
        let canonical = crate::console_auth::canonical_host(3999);
        let desktop = "127.0.0.1:3999";

        for path in [
            "/",
            "/conversations",
            "/persona",
            "/settings",
            "/login",
            "/setup",
        ] {
            let resp = make_get(&canonical, path, None, None);
            assert_eq!(resp.status, 200, "{path}: {}", resp.body);
            assert_eq!(resp.csp, Some(crate::console_ui::STRICT_CSP));
            assert!(resp.content_type.starts_with("text/html"));
            assert!(!resp.body.is_empty());

            let denied_agent = make_get(&canonical, path, None, Some("agent-tok"));
            assert_eq!(denied_agent.status, 403, "{path} agent must be refused");

            let denied_alias_agent = make_get(desktop, path, None, Some("agent-tok"));
            assert_eq!(
                denied_alias_agent.status, 403,
                "{path} alias-host agent must be refused before redirect"
            );
        }

        for (legacy, target) in [
            ("/personas", "/persona"),
            ("/tasks", "/schedule"),
            ("/settings/conversations", "/conversations"),
            ("/settings/models", "/models"),
        ] {
            let resp = make_get(&canonical, legacy, None, None);
            assert_eq!(resp.status, 302, "{legacy}");
            assert!(matches!(resp.redirect, Some(ConsoleRedirect::Path(ref p)) if p == target));

            let denied_alias_agent = make_get(desktop, legacy, None, Some("agent-tok"));
            assert_eq!(
                denied_alias_agent.status, 403,
                "{legacy} alias-host agent must be refused before redirect"
            );

            let alias = make_get(desktop, legacy, None, None);
            assert_eq!(alias.status, 302, "{legacy} alias redirect");
            assert!(matches!(
                alias.redirect,
                Some(ConsoleRedirect::CanonicalOrigin(3999, ref p)) if p == target
            ));
        }

        let unknown_api = make_get(
            &canonical,
            "/api/does-not-exist",
            Some(&context.effective_console_token(&cfg)),
            None,
        );
        assert_ne!(
            unknown_api.status, 200,
            "unknown /api route must never fall back to SPA"
        );
        assert!(unknown_api.content_type.starts_with("application/json"));

        let missing_asset = make_get(&canonical, "/assets/missing.js", None, None);
        assert_eq!(missing_asset.status, 404);
        assert!(missing_asset.content_type.starts_with("application/json"));

        let missing_file = make_get(
            &canonical,
            "/missing.txt",
            Some(&context.effective_console_token(&cfg)),
            None,
        );
        assert_ne!(
            missing_file.status, 200,
            "missing static extension path must never fall back to SPA"
        );

        let bad_host_asset = make_get("evil.example:3999", "/assets/missing.js", None, None);
        assert_eq!(bad_host_asset.status, 403);

        let agent_asset = make_get(&canonical, "/assets/missing.js", None, Some("agent-tok"));
        assert_eq!(agent_asset.status, 403);
    }

    fn make_page_req(host: &str, path: &str, extra: &[(&str, &str)]) -> Request {
        let mut headers = HashMap::from([("host".to_string(), host.to_string())]);
        for (k, v) in extra {
            headers.insert((*k).to_string(), (*v).to_string());
        }
        Request {
            method: "GET".to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers,
            body: String::new(),
            body_too_large: false,
        }
    }

    #[test]
    fn local_browser_auto_sign_in_issues_session_only_for_same_user_non_cross_site_when_enabled() {
        struct PeerGuard;
        impl Drop for PeerGuard {
            fn drop(&mut self) {
                TEST_PEER_SAME_USER.with(|cell| cell.set(None));
            }
        }
        let _guard = PeerGuard;
        let (_root, context) =
            super::super::tests::fixture_runtime("front local auto sign-in", "fixture-front-token");
        let port = 3999;
        let canonical = crate::console_auth::canonical_host(port);
        let cfg = context.load_config().unwrap();

        TEST_PEER_SAME_USER.with(|cell| cell.set(None));
        let unavailable = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(unavailable.status, 200);
        assert!(unavailable.cookies.is_empty());

        TEST_PEER_SAME_USER.with(|cell| cell.set(Some(false)));
        let other_user = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(other_user.status, 200);
        assert!(other_user.cookies.is_empty());

        TEST_PEER_SAME_USER.with(|cell| cell.set(Some(true)));
        for (req, expected_status) in [
            (
                make_page_req(&canonical, "/settings", &[("sec-fetch-site", "cross-site")]),
                200,
            ),
            (
                make_page_req(&canonical, "/settings", &[("sec-fetch-site", "same-site")]),
                200,
            ),
            (
                make_page_req(
                    &canonical,
                    "/settings",
                    &[("origin", "http://evil.example")],
                ),
                200,
            ),
            (
                make_page_req(&canonical, "/login", &[("sec-fetch-site", "none")]),
                200,
            ),
            (
                make_page_req(&canonical, "/setup", &[("sec-fetch-site", "none")]),
                200,
            ),
            (make_page_req("evil.example:3999", "/settings", &[]), 403),
            (
                make_page_req(&canonical, "/settings", &[("x-agent-token", "agent-123")]),
                403,
            ),
            (
                make_page_req(
                    &canonical,
                    "/settings",
                    &[("x-console-token", "wrong-token")],
                ),
                401,
            ),
            (
                make_page_req(&canonical, "/api/status", &[("sec-fetch-site", "none")]),
                401,
            ),
        ] {
            let denied = handle_http_request(None, &req, &cfg, &context, port);
            assert_eq!(
                denied.status, expected_status,
                "{} {}",
                req.path, denied.body
            );
            assert!(
                denied.cookies.is_empty(),
                "{} must not issue a cookie",
                req.path
            );
        }

        let signed_in = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(signed_in.status, 200, "{}", signed_in.body);
        assert_eq!(signed_in.cookies.len(), 1);
        let cookie_pair = signed_in.cookies[0]
            .header()
            .split_once(';')
            .unwrap()
            .0
            .to_string();

        let status_res = handle_http_request(
            None,
            &make_page_req(
                &canonical,
                "/api/auth/status",
                &[("cookie", &cookie_pair), ("sec-fetch-site", "same-origin")],
            ),
            &cfg,
            &context,
            port,
        );
        assert_eq!(status_res.status, 200, "{}", status_res.body);
        let status_json: Value = serde_json::from_str(&status_res.body).unwrap();
        assert_eq!(status_json["authenticated"], json!(true));
        assert_eq!(status_json["authKind"], json!("browser"));
        assert_eq!(status_json["requirePasswordLocally"], json!(false));

        let api_settings = handle_http_request(
            None,
            &make_page_req(
                &canonical,
                "/api/settings",
                &[("cookie", &cookie_pair), ("sec-fetch-site", "same-origin")],
            ),
            &cfg,
            &context,
            port,
        );
        assert_eq!(api_settings.status, 200, "{}", api_settings.body);

        crate::settings::apply(
            &context,
            "security.requirePasswordLocally",
            &json!(true),
            "administrator",
            "console",
        )
        .unwrap();
        let reloaded = context.load_config().unwrap();
        let disabled = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &reloaded,
            &context,
            port,
        );
        assert_eq!(disabled.status, 200);
        assert!(disabled.cookies.is_empty());
        let unauth_api = handle_http_request(
            None,
            &make_page_req(
                &canonical,
                "/api/settings",
                &[("sec-fetch-site", "same-origin")],
            ),
            &reloaded,
            &context,
            port,
        );
        assert_eq!(unauth_api.status, 401);
    }

    #[test]
    fn legacy_ui_index_and_assets_carry_strict_csp() {
        let port = 32123;
        let cfg = json!({ "consoleToken": "legacy-test-token" });
        let context = RuntimeContext::legacy();
        for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
            let mut query_req = make_page_req(&host, "/", &[]);
            query_req.query.insert("token".into(), "legacy-test-token".into());
            let index_res = handle_http_request(None, &query_req, &cfg, &context, port);
            assert_eq!(index_res.status, 200, "{host}");
            assert_eq!(index_res.csp, Some(crate::console_ui::STRICT_CSP));
            assert!(index_res.raw_body.is_some());
            assert_eq!(index_res.cookies.len(), 1);
            let cookie_pair = index_res.cookies[0]
                .header()
                .split(';')
                .next()
                .unwrap()
                .to_string();

            let reload_res = handle_http_request(
                None,
                &make_page_req(&host, "/conversations", &[("cookie", &cookie_pair)]),
                &cfg,
                &context,
                port,
            );
            assert_eq!(reload_res.status, 200, "{host}");
            assert!(reload_res.raw_body.is_some());

            let api_with_cookie_only = handle_http_request(
                None,
                &make_page_req(&host, "/api/settings", &[("cookie", &cookie_pair)]),
                &cfg,
                &context,
                port,
            );
            assert_eq!(api_with_cookie_only.status, 401, "{host}");

            let api_with_bearer = handle_http_request(
                None,
                &make_page_req(
                    &host,
                    "/api/settings",
                    &[("x-console-token", "legacy-test-token")],
                ),
                &cfg,
                &context,
                port,
            );
            assert_ne!(api_with_bearer.status, 401, "{host}");

            let mut tasks_redirect = make_page_req(&host, "/tasks", &[]);
            tasks_redirect
                .query
                .insert("token".into(), "legacy-test-token".into());
            let redirect_res = handle_http_request(None, &tasks_redirect, &cfg, &context, port);
            assert_eq!(redirect_res.status, 302, "{host}");
            assert!(matches!(
                redirect_res.redirect,
                Some(ConsoleRedirect::Path(ref p)) if p == "/schedule?token=legacy-test-token"
            ));
            assert_eq!(redirect_res.cookies.len(), 1);

            for headers in [
                vec![],
                vec![("x-console-token", "wrong-token")],
                vec![
                    ("cookie", cookie_pair.as_str()),
                    ("x-console-token", "wrong-token"),
                ],
            ] {
                let unauth = handle_http_request(
                    None,
                    &make_page_req(&host, "/", &headers),
                    &cfg,
                    &context,
                    port,
                );
                assert_eq!(unauth.status, 401, "{host}");
            }

            let mut logout_req = make_page_req(
                &host,
                "/api/auth/logout",
                &[
                    ("content-type", "application/json"),
                    ("cookie", cookie_pair.as_str()),
                ],
            );
            logout_req.method = "POST".into();
            let logout_res = handle_http_request(None, &logout_req, &cfg, &context, port);
            assert_eq!(logout_res.status, 200, "{host}");
            assert_eq!(logout_res.cookies.len(), 1);
            assert!(logout_res.cookies[0].header().contains("Max-Age=0"));

            let mut logout_bad_tok = make_page_req(
                &host,
                "/api/auth/logout",
                &[
                    ("content-type", "application/json"),
                    ("cookie", cookie_pair.as_str()),
                    ("x-console-token", "wrong-token"),
                ],
            );
            logout_bad_tok.method = "POST".into();
            let logout_bad_res = handle_http_request(None, &logout_bad_tok, &cfg, &context, port);
            assert_eq!(logout_bad_res.status, 401, "{host}");
        }

        let agent_denied = handle_http_request(
            None,
            &make_page_req(&format!("127.0.0.1:{port}"), "/", &[("x-agent-token", "agent-token")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(agent_denied.status, 403);

        let bad_host_asset = handle_http_request(
            None,
            &make_page_req(&format!("evil.example:{port}"), "/assets/nonexistent.js", &[]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(bad_host_asset.status, 403);

        let missing_asset = handle_http_request(
            None,
            &make_page_req(&format!("127.0.0.1:{port}"), "/assets/nonexistent.js", &[]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(missing_asset.status, 404);
    }

    #[test]
    fn auth_status_reports_configured_vs_generated_token_file() {
        let (root, context) = super::super::tests::fixture_runtime(
            "auth status token file",
            "fixture-front-token",
        );
        let port = 3999;
        let canonical = crate::console_auth::canonical_host(port);
        let cfg = context.load_config().unwrap();

        let configured_res = handle_http_request(
            None,
            &make_page_req(&canonical, "/api/auth/status", &[]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(configured_res.status, 200, "{}", configured_res.body);
        let configured_body: Value = serde_json::from_str(&configured_res.body).unwrap();
        assert_eq!(configured_body["tokenFile"], "config/config.json");

        let config_path = root.0.join("config/config.json");
        let mut raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        raw["consoleToken"] = json!("");
        std::fs::write(&config_path, raw.to_string()).unwrap();
        std::fs::write(
            root.0.join("data/console-token"),
            "generated-fixture-token-01",
        )
        .unwrap();

        let updated_cfg = context.load_config().unwrap();
        let generated_res = handle_http_request(
            None,
            &make_page_req(&canonical, "/api/auth/status", &[]),
            &updated_cfg,
            &context,
            port,
        );
        assert_eq!(generated_res.status, 200, "{}", generated_res.body);
        let generated_body: Value = serde_json::from_str(&generated_res.body).unwrap();
        assert_eq!(generated_body["tokenFile"], "data/console-token");
    }

    #[test]
    fn v2_admin_only_allows_console_without_agent_header_and_denies_agent_header() {
        let (_root, context) = super::super::tests::fixture_runtime(
            "v2-admin-only-policy",
            "fixture-front-token",
        );
        let cfg = context.load_config().unwrap();
        for (method, path) in [
            ("GET", "/api/socialV2/config"),
            ("POST", "/api/socialV2/config"),
            ("GET", "/api/socialV2/activity"),
            ("POST", "/api/socialV2/activity"),
            ("GET", "/api/socialV2/tool-log"),
            ("POST", "/api/socialV2/tool-log/clear"),
            ("GET", "/api/socialV2/states"),
        ] {
            let policy = route_policy(method, path);
            let console_req = Request {
                method: method.to_string(),
                path: path.to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: String::new(),
                body_too_large: false,
            };
            assert_eq!(
                policy_agent_denial(policy, &console_req, &cfg, &context),
                None,
                "console request without x-agent-token must pass {method} {path}"
            );

            let mut agent_req = console_req;
            agent_req
                .headers
                .insert("x-agent-token".to_string(), "any-agent-token".to_string());
            let denial = policy_agent_denial(policy, &agent_req, &cfg, &context);
            assert!(
                matches!(denial, Some((403, _))),
                "agent request must be denied with 403 on {method} {path}: {denial:?}"
            );
        }
    }
}
