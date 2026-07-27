//! Serves the built frontend from `webui/dist`, embedded into the binary
//! by `rust-embed` (`TASKS.md` T5.1, issue #18 — "資產 embed"). In release
//! builds these bytes are compiled straight into the executable, so
//! `serialwrap daemon` needs nothing on disk at runtime to serve a working
//! UI (verified by this task's acceptance test: move `webui/dist` away
//! after `cargo build --release` and the server still works).

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../webui/dist"]
struct Assets;

/// Whether `path`'s last segment has a file extension. Vite's build always
/// gives real static assets a hashed extension (`index-Hh0iSpFP.js`), so a
/// missing path that still looks like a filename (`/assets/old-chunk.js`
/// after a rebuild renamed it, or a plain typo) is almost certainly a
/// genuine 404 — not an SPA client-side route, which this task's frontend
/// doesn't have yet but T5.2+'s might.
fn looks_like_a_static_file(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
}

/// Whether `raw_path` (the untrimmed `uri.path()`, always leading-slash) is
/// `/api` itself or anything under it. `starts_with("/api/")` alone misses
/// the bare `/api` (no trailing slash) — a request for exactly that path
/// would otherwise fall through to the SPA fallback and get a `200`
/// `index.html` instead of the `404` every other unmatched API path gets.
fn is_api_path(raw_path: &str) -> bool {
    raw_path == "/api" || raw_path.starts_with("/api/")
}

/// Fallback handler for any request no other route matched: serves the
/// embedded asset at the request path, or `index.html` for anything that
/// looks like an SPA client-side route (no file extension in the last path
/// segment) — but never for `/api/*` or a missing extensioned path (e.g. a
/// stale `/assets/*.js` reference), where a miss should stay a `404`, not
/// silently turn into the app shell (which the browser would then choke on
/// trying to parse as JS/CSS).
pub async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref().to_string())],
            content.data,
        )
            .into_response();
    }

    if is_api_path(uri.path()) || looks_like_a_static_file(path) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match Assets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "no frontend build found — run `npm run build` in webui/",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever `webui/dist` currently contains (the real build in CI/dev,
    /// or `build.rs`'s placeholder on a Rust-only checkout) must include an
    /// `index.html` — this is the one thing every code path above assumes.
    #[test]
    fn embedded_assets_always_include_an_index_html() {
        assert!(
            Assets::get("index.html").is_some(),
            "webui/dist must contain index.html (see build.rs and webui/README.md)"
        );
    }

    #[test]
    fn extensioned_paths_are_not_treated_as_spa_routes() {
        assert!(looks_like_a_static_file("assets/index-Hh0iSpFP.js"));
        assert!(looks_like_a_static_file("assets/old-chunk-1234.js"));
        assert!(looks_like_a_static_file("favicon.ico"));
    }

    #[test]
    fn extensionless_paths_do_not_look_like_static_files() {
        assert!(!looks_like_a_static_file("devices/42"));
        assert!(!looks_like_a_static_file(""));
    }

    #[tokio::test]
    async fn a_missing_extensioned_asset_404s_instead_of_falling_back_to_index_html() {
        use axum::http::StatusCode;

        let response = serve_asset("/assets/does-not-exist-1234.js".parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Regression test for a review finding on PR #43: `starts_with("/api/")`
    /// alone doesn't match the bare `/api` (no trailing slash), so a request
    /// for exactly that path used to fall through to the SPA branch and get
    /// a `200` `index.html` instead of a `404`.
    #[tokio::test]
    async fn bare_api_path_with_no_trailing_slash_still_404s() {
        use axum::http::StatusCode;

        let response = serve_asset("/api".parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_extensionless_unknown_path_falls_back_to_index_html() {
        use axum::http::StatusCode;

        let response = serve_asset("/devices/42".parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
