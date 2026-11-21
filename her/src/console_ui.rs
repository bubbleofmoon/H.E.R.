//! Embedded web console UI bundle (`her-ui/dist/`) resolved from `her/build.rs`.

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// Strict Content-Security-Policy applied to every embedded UI HTML and static asset response.
pub(crate) const STRICT_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; form-action 'self'; base-uri 'none';";
pub(crate) const NOSNIFF: &str = "nosniff";

/// Canonical SPA redirect targets for retired standalone console HTML URLs.
pub(crate) fn legacy_redirect(path: &str) -> Option<&'static str> {
    match path {
        "/personas" => Some("/persona"),
        "/tasks" => Some("/schedule"),
        "/settings/conversations" => Some("/conversations"),
        "/settings/models" => Some("/models"),
        _ => None,
    }
}

/// Resolves an embedded asset by exact path or SPA navigation fallback.
///
/// Returns `None` for `/api` routes, `/assets/*` misses, and missing file-extension paths so API
/// routers and 404 handlers keep their own contracts.
pub(crate) fn resolve_asset(path: &str) -> Option<&'static EmbeddedAsset> {
    if path == "/api" || path.starts_with("/api/") {
        return None;
    }
    if let Some(exact) = UI_ASSETS.iter().find(|asset| asset.path == path) {
        return Some(exact);
    }
    if path.starts_with("/assets/") {
        return None;
    }
    let last_segment = path.rsplit('/').next().unwrap_or("");
    if last_segment.contains('.') {
        return None;
    }
    UI_ASSETS.iter().find(|asset| asset.path == "/index.html")
}

#[cfg(test)]
pub(crate) fn is_stub() -> bool {
    UI_IS_STUB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_redirects_map_to_canonical_spa_paths() {
        assert_eq!(legacy_redirect("/personas"), Some("/persona"));
        assert_eq!(legacy_redirect("/tasks"), Some("/schedule"));
        assert_eq!(
            legacy_redirect("/settings/conversations"),
            Some("/conversations")
        );
        assert_eq!(legacy_redirect("/settings/models"), Some("/models"));
        assert_eq!(legacy_redirect("/conversations"), None);
        assert_eq!(legacy_redirect("/api/settings"), None);
    }

    #[test]
    fn resolve_asset_serves_index_and_spa_fallback_but_refuses_api_and_missing_assets() {
        let index = resolve_asset("/index.html").expect("/index.html embedded");
        assert!(index.content_type.starts_with("text/html"));
        assert!(!index.bytes.is_empty());

        for spa_route in [
            "/",
            "/conversations",
            "/persona",
            "/login",
            "/setup",
            "/security",
        ] {
            let resolved =
                resolve_asset(spa_route).expect("SPA navigation route falls back to index");
            assert_eq!(resolved.path, "/index.html", "{spa_route}");
        }

        assert!(resolve_asset("/api/status").is_none());
        assert!(resolve_asset("/api/unknown").is_none());
        assert!(resolve_asset("/api").is_none());
        assert!(resolve_asset("/assets/does-not-exist.js").is_none());
        assert!(resolve_asset("/missing-file.txt").is_none());
        let _ = is_stub();
    }

    #[test]
    fn csp_and_nosniff_constants_forbid_inline_scripts_and_styles() {
        assert!(!STRICT_CSP.contains("'unsafe-inline'"));
        assert!(!STRICT_CSP.contains("'unsafe-eval'"));
        assert!(STRICT_CSP.contains("script-src 'self'"));
        assert!(STRICT_CSP.contains("style-src 'self'"));
        assert_eq!(NOSNIFF, "nosniff");
    }
}
