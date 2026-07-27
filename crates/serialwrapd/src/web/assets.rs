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

/// Fallback handler for any request no other route matched: serves the
/// embedded asset at the request path, or `index.html` for anything that
/// looks like an SPA client-side route (no file extension) — but never for
/// `/api/*`, where a miss should stay a `404`, not silently turn into the
/// app shell.
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

    if uri.path().starts_with("/api/") {
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
}
