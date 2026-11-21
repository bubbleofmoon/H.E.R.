//! Unified HTTP console front: canonical route table, host/cross-site validation, credential admission,
//! desktop sign-in and verified caller authority (`AdminCaller`, `VerifiedCaller`).

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentPolicy {
    AgentAllowed,
    AdminOnly(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyPreflight {
    None,
    SessionKey,
    SessionKeyOrClosedOwnerReset,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutePolicy {
    pub(crate) public_probe: bool,
    pub(crate) agent: AgentPolicy,
    pub(crate) key_preflight: KeyPreflight,
}

impl RoutePolicy {
    const fn admin(error: &'static str) -> Self {
        Self {
            public_probe: false,
            agent: AgentPolicy::AdminOnly(error),
            key_preflight: KeyPreflight::None,
        }
    }

    const fn auth() -> Self {
        Self {
            public_probe: false,
            agent: AgentPolicy::AdminOnly("agent credential cannot access console authentication"),
            key_preflight: KeyPreflight::None,
        }
    }

    const fn console_or_agent(key_preflight: KeyPreflight) -> Self {
        Self {
            public_probe: false,
            agent: AgentPolicy::AgentAllowed,
            key_preflight,
        }
    }
}

pub(crate) fn route_policy(method: &str, path: &str) -> Option<RoutePolicy> {
    if matches!(
        (method, path),
        (
            "GET",
            "/api/health" | "/favicon.ico" | "/apple-touch-icon.png"
        )
    ) {
        return Some(RoutePolicy {
            public_probe: true,
            agent: AgentPolicy::AgentAllowed,
            key_preflight: KeyPreflight::None,
        });
    }
    if (method, path) == ("POST", "/api/desktop/sign-in") {
        return Some(RoutePolicy::admin(
            "desktop sign-in requires the installing user",
        ));
    }
    if matches!(
        (method, path),
        ("GET", "/api/auth/status" | "/api/auth/sessions")
            | (
                "POST",
                "/api/auth/first-run"
                    | "/api/auth/login"
                    | "/api/auth/logout"
                    | "/api/auth/change-password"
                    | "/api/auth/sessions/revoke"
            )
    ) {
        return Some(RoutePolicy::auth());
    }
    if matches!(method, "GET" | "POST") && crate::console_settings::is_route(path) {
        return Some(RoutePolicy::admin(
            "settings routes are restricted to administrators",
        ));
    }
    if (method == "GET" && path == "/personas")
        || (matches!(method, "GET" | "POST")
            && (path == "/api/personas" || path.starts_with("/api/personas/")))
    {
        return Some(RoutePolicy::admin(
            "persona routes are restricted to administrators",
        ));
    }
    if (method == "GET" && path == "/schedule")
        || (matches!(method, "GET" | "POST")
            && (path == "/api/schedule" || path.starts_with("/api/schedule/")))
    {
        return Some(RoutePolicy::admin(
            "日程接口仅限管理员控制台，不接受 agent 凭据",
        ));
    }
    if (method == "GET" && matches!(path, "/tasks" | "/api/tasks"))
        || (matches!(method, "GET" | "POST") && path.starts_with("/api/tasks/"))
    {
        return Some(RoutePolicy::admin(
            "任务接口仅限管理员控制台，不接受 agent 凭据",
        ));
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
        return Some(RoutePolicy::console_or_agent(KeyPreflight::SessionKey));
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

    match (method, path) {
        ("GET", "/api/qq/login") => Some(RoutePolicy::admin(
            "QQ login state is restricted to administrators",
        )),
        ("POST", "/api/restart") => Some(RoutePolicy::admin(
            "restart is restricted to administrators",
        )),
        ("GET", "/api/dependencies")
        | ("POST", "/api/dependencies/check" | "/api/dependencies/approve") => Some(
            RoutePolicy::admin("dependency upgrades are restricted to administrators"),
        ),
        ("GET", "/api/releases" | "/api/releases/settings")
        | ("POST", "/api/releases/settings" | "/api/releases/approve") => Some(RoutePolicy::admin(
            "release settings are restricted to administrators",
        )),
        ("GET", "/api/backups/settings" | "/api/backups")
        | (
            "POST",
            "/api/backups/settings" | "/api/backups/restore/preview" | "/api/backups/restore",
        ) => Some(RoutePolicy::admin(
            "backup settings are restricted to administrators",
        )),
        ("POST", "/api/conversations/scope") => Some(RoutePolicy::admin("该接口仅控制台可用")),

        ("POST", "/api/session/reset") => Some(RoutePolicy::console_or_agent(
            KeyPreflight::SessionKeyOrClosedOwnerReset,
        )),
        ("POST", "/api/memory/archive" | "/api/memory/history") => {
            Some(RoutePolicy::console_or_agent(KeyPreflight::SessionKey))
        }

        ("GET", "/api/social")
        | ("POST", "/api/social" | "/api/social/state" | "/api/social/flush")
        | (
            "POST",
            "/api/socialV2/send-message"
            | "/api/socialV2/send-burst"
            | "/api/socialV2/send-poke"
            | "/api/socialV2/send-voice",
        )
        | (
            "POST",
            "/api/voice/send"
            | "/api/send/voice"
            | "/api/socialV2/mark-read"
            | "/api/socialV2/wake-config"
            | "/api/socialV2/wait",
        )
        | (
            "GET",
            "/api/socialV2/message-detail"
            | "/api/images/message"
            | "/api/socialV2/forward-message",
        )
        | ("POST", "/api/socialV2/forward-media")
        | (
            "GET",
            "/api/socialV2/sticker-image"
            | "/api/socialV2/self-image"
            | "/api/socialV2/sticker-list",
        )
        | (
            "POST",
            "/api/socialV2/sticker-list"
            | "/api/socialV2/sticker-note"
            | "/api/socialV2/sticker-remark",
        )
        | ("POST", "/api/socialV2/send-sticker" | "/api/socialV2/collect-sticker")
        | (
            "POST",
            "/api/send/group" | "/api/send/private" | "/api/send/reply" | "/api/authorize/read",
        )
        | ("POST", "/api/socialV2/reset" | "/api/socialV2/wake" | "/api/socialV2/wake-snapshot")
        | ("GET", "/api/socialV2/memory")
        | (
            "POST",
            "/api/socialV2/memory-append"
            | "/api/socialV2/memory-update"
            | "/api/socialV2/memory-remove"
            | "/api/socialV2/memory-clear",
        )
        | (
            "GET" | "POST",
            "/api/socialV2/config"
            | "/api/socialV2/activity"
            | "/api/socialV2/feedback"
            | "/api/socialV2/initial-prompt",
        )
        | (
            "GET",
            "/api/socialV2/state"
            | "/api/socialV2/prompt"
            | "/api/socialV2/slang/query"
            | "/api/socialV2/states",
        )
        | ("POST", "/api/socialV2/slang/submit" | "/api/socialV2/feedback-clear")
        | (
            "GET",
            "/api/socialV2/unread"
            | "/api/socialV2/recent"
            | "/api/socialV2/my-recent"
            | "/api/socialV2/active-members",
        )
        | (
            "GET",
            "/api/inbound/gaps" | "/api/socialV2/send-outbox" | "/api/sessions" | "/api/turns",
        )
        | ("POST", "/api/inbound/gaps/retry" | "/api/turns/resolve")
        | (
            "GET",
            "/api/model-chain" | "/api/voice/list" | "/api/socialV2/voices" | "/api/tokens/turns",
        )
        | ("POST", "/api/session-models" | "/api/slang/extract") => {
            Some(RoutePolicy::console_or_agent(KeyPreflight::SessionKey))
        }

        (
            "GET",
            "/" | "/api/status" | "/api/pending" | "/api/whitelist" | "/api/security"
            | "/api/audit/recent",
        )
        | ("POST", "/api/whitelist" | "/api/security" | "/api/console/token" | "/api/audit/undo")
        | ("GET", "/api/session-models" | "/api/socialV2/tool-log")
        | ("POST", "/api/socialV2/tool-log/clear")
        | (
            "GET",
            "/api/roles"
            | "/api/roles/content"
            | "/api/presets"
            | "/api/preset/sim-prompt"
            | "/api/preset/sim-prompt/backups",
        )
        | (
            "POST",
            "/api/roles/create" | "/api/roles/update" | "/api/roles/rename" | "/api/roles/delete"
            | "/api/roles/limit" | "/api/role" | "/api/role-mode" | "/api/mode",
        )
        | ("POST", "/api/preset/sim-prompt" | "/api/preset/sim-prompt/restore")
        | ("GET", "/api/stickers")
        | ("POST", "/api/stickers/note" | "/api/stickers/sync" | "/api/stickers/remark")
        | (
            "GET",
            "/api/tokens/by-provider"
            | "/api/usage/attribution"
            | "/api/tokens/summary"
            | "/api/tokens/series"
            | "/api/tokens/recent",
        )
        | ("GET", "/api/slang" | "/api/recovery-record")
        | (
            "POST",
            "/api/slang"
            | "/api/slang/clear"
            | "/api/slang/config"
            | "/api/slang/batch-delete"
            | "/api/slang/batch-confirm"
            | "/api/slang/batch-reject"
            | "/api/slang/research",
        ) => Some(RoutePolicy::console_or_agent(KeyPreflight::None)),
        _ => None,
    }
}

pub(crate) fn supports_request(context: &RuntimeContext, method: &str, path: &str) -> bool {
    !context.is_isolated() || route_policy(method, path).is_some()
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

    pub(crate) fn gate(&self) -> Result<Option<std::fs::File>, (u16, String)> {
        super::config_gate(self.context)
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
}

impl VerifiedCaller {
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
    match super::desktop_sign_in_selected(stream, req, context, port) {
        Ok(session) => reply(200, json!({"ok":true,"session":session})),
        Err((status, error)) => reply(status, json!({"ok":false,"error":error})),
    }
}

pub(crate) fn enforce_key_preflight(
    req: &Request,
    cfg: &Value,
    context: &RuntimeContext,
    caller: &VerifiedCaller,
) -> Option<(u16, String, String)> {
    let policy = route_policy(&req.method, &req.path);
    let preflight = policy
        .map(|p| p.key_preflight)
        .unwrap_or(KeyPreflight::SessionKey);
    if preflight == KeyPreflight::None {
        return None;
    }
    let memory_agent_call = memory_agent_call(req);
    let has_credential = caller.is_admitted()
        || req
            .headers
            .get("x-agent-token")
            .is_some_and(|t| !t.trim().is_empty());
    let has_agent_header = req.headers.contains_key("x-agent-token");
    let defer_agent_allowlist = memory_agent_call
        || (has_agent_header
            && (social_agent_policy_route(req) || pending_admin_agent_gate_route(req)));
    let defer_send_message_agent_allowlist =
        (req.method == "POST" && req.path == "/api/socialV2/send-message" && has_agent_header)
            || agent::is_send_route(req)
            || (context.is_isolated()
                && req.method == "POST"
                && matches!(
                    req.path.as_str(),
                    "/api/socialV2/wake" | "/api/authorize/read"
                ));
    if has_credential && !defer_agent_allowlist && !defer_send_message_agent_allowlist {
        if let Some(key) = request_session_key(req) {
            let closed_owner_reset = preflight == KeyPreflight::SessionKeyOrClosedOwnerReset
                && context.is_isolated()
                && runtime_mode(context) == "closed-agent"
                && crate::chat::is_owner_key(cfg, &key);
            if !key_in_allowlist(cfg, &key) && !closed_owner_reset {
                return Some((
                    403,
                    "application/json; charset=utf-8".to_string(),
                    json!({"ok": false, "error": format!("会话不在允许范围内：{key}")}).to_string(),
                ));
            }
        }
    }
    None
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
    if let Some(policy) = route_policy(&req.method, &req.path) {
        if let AgentPolicy::AdminOnly(message) = policy.agent {
            if caller.is_agent() {
                return Some(reply(403, message.to_string()));
            }
        }
    }
    if let Some((status, message)) = agent::agent_global_denial(req, cfg, context) {
        return Some(reply(status, message));
    }
    if let Some((status, message)) = tool_log_agent_denial(req, cfg, context) {
        return Some(reply(status, message));
    }
    if let Some((status, message)) = pending_agent_denial(req) {
        return Some(reply(status, message));
    }
    if let Some((status, message)) = send_message_global_agent_denial(req, cfg, context) {
        return Some(reply(status, message));
    }
    if let Some((status, message)) = social_agent_policy_denial(req, cfg, context) {
        return Some(reply(status, message));
    }
    None
}

#[cfg(test)]
thread_local! {
    static TEST_PEER_SAME_USER: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn peer_is_installing_user(stream: Option<&TcpStream>) -> bool {
    #[cfg(test)]
    if let Some(override_same) = TEST_PEER_SAME_USER.with(|cell| cell.get()) {
        return override_same;
    }
    stream.is_some_and(super::same_installing_user)
}

fn eligible_for_local_auto_sign_in(req: &Request, port: u16, admission: &FrontAdmission) -> bool {
    if req.method != "GET" || !PAGE_MOUNTS.contains(&req.path.as_str()) {
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
    let mut admission = if context.is_isolated() && !public {
        if let Some((status, value)) = host_denial(req, port) {
            return json_reply(status, value);
        }
        let mut admission = match admit_selected_request(req, context, port) {
            Ok(admission) => admission,
            Err((status, error)) => return json_reply(status, json!({"ok":false,"error":error})),
        };
        let desktop = matches!(
            admission.proof,
            Some(crate::console_auth::ConsoleAuthority::Desktop { .. })
        );
        if req.method == "GET" && !canonical_request(req, port) && !desktop {
            if let Some(mount) = PAGE_MOUNTS.iter().copied().find(|mount| *mount == req.path) {
                let mut response = ConsoleResponse::from((302, String::new(), String::new()));
                response.redirect = Some((port, mount));
                return response;
            }
        }
        if eligible_for_local_auto_sign_in(req, port, &admission) && peer_is_installing_user(stream)
        {
            match crate::console_auth::auth_local_browser_auto_sign_in(context, port) {
                Ok(Some((issued, authority))) => {
                    admission.proof = Some(authority);
                    admission.renewal = Some(BrowserCookie::Renew {
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
        admitted_context.console_authority = admission.proof.clone();
        Some(admission)
    } else {
        None
    };
    #[cfg(test)]
    if let Some(hook) = AFTER_ADMISSION.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let agent_marker = has_agent_marker(req);
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
    take_config_writes();
    let (audit_context, status, ctype, mut body, auth_cookies, renewal) =
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
                )
            } else {
                let renewal = admission.renewal.take();
                let mut caller = VerifiedCaller {
                    context: admitted_context,
                    config: admission.config,
                    kind,
                    memory: admission.memory,
                    browser_acl: admission.browser_acl,
                };
                let (status, ctype, body) = route_with_caller(&mut caller, req);
                (caller.context, status, ctype, body, Vec::new(), renewal)
            }
        } else {
            let mut caller = VerifiedCaller {
                context: admitted_context,
                config: cfg.clone(),
                kind,
                memory: crate::memory_actions::ConsoleCapability::Conversation,
                browser_acl: None,
            };
            let (status, ctype, body) = route_with_caller(&mut caller, req);
            (caller.context, status, ctype, body, Vec::new(), None)
        };
    let config_writes = take_config_writes();
    let record_admin = audit && administrator;
    if audit && ((200..300).contains(&status) || record_admin) {
        if let Err(error) = audit_write(
            &audit_context,
            req,
            status,
            &body,
            record_admin,
            &before,
            config_writes,
        ) {
            crate::safe_println!(
                "[console] ⚠️ 审计记录写入失败（{} {}，状态 {status}）：{error}",
                req.method,
                req.path
            );
            if audit_context.is_isolated() {
                if let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(&body) {
                    reply.insert("audit".into(), json!({"ok": false, "error": format!("审计记录写入失败，此变更无法撤销：{error}")}));
                    body = Value::Object(reply).to_string();
                }
            }
        }
    }
    let mut response = ConsoleResponse::from((status, ctype, body));
    if !auth_cookies.is_empty() {
        response.cookies = auth_cookies;
    } else if (200..300).contains(&status)
        && req.method == "POST"
        && req.path == "/api/console/token"
    {
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

        let (s_set, _, b_set) = crate::console_settings::route(
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

        let make_page_req = |host: &str, path: &str, extra: &[(&str, &str)]| {
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
        };

        // Unavailable peer (`stream = None`, no test override) -> refused, no cookie.
        TEST_PEER_SAME_USER.with(|cell| cell.set(None));
        let unavailable = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(unavailable.status, 401);
        assert!(unavailable.cookies.is_empty());

        // Different user (`Some(false)`) -> refused, no cookie.
        TEST_PEER_SAME_USER.with(|cell| cell.set(Some(false)));
        let other_user = handle_http_request(
            None,
            &make_page_req(&canonical, "/settings", &[("sec-fetch-site", "none")]),
            &cfg,
            &context,
            port,
        );
        assert_eq!(other_user.status, 401);
        assert!(other_user.cookies.is_empty());

        // Same user (`Some(true)`), cross-site / bad Host / agent / explicit token / API path -> refused, no cookie.
        TEST_PEER_SAME_USER.with(|cell| cell.set(Some(true)));
        for (req, expected_status) in [
            (
                make_page_req(&canonical, "/settings", &[("sec-fetch-site", "cross-site")]),
                401,
            ),
            (
                make_page_req(&canonical, "/settings", &[("sec-fetch-site", "same-site")]),
                401,
            ),
            (
                make_page_req(
                    &canonical,
                    "/settings",
                    &[("origin", "http://evil.example")],
                ),
                401,
            ),
            (make_page_req("evil.example:3999", "/settings", &[]), 403),
            (
                make_page_req(&canonical, "/settings", &[("x-agent-token", "agent-123")]),
                401,
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

        // Same-user canonical page GET -> issues browser cookie & serves page (200).
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

        // Issued cookie authenticates /api/auth/status as a browser session.
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

        // Enabling `security.requirePasswordLocally` turns off auto sign-in even for same user.
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
        assert_eq!(disabled.status, 401);
        assert!(disabled.cookies.is_empty());
    }
}
