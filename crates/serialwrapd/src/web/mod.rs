//! Embedded web GUI infrastructure (`TASKS.md` T5.1, issue #18): an `axum`
//! server exposing `GET /api/*` and `WS /api/stream`, plus the built
//! frontend (`rust-embed`, see [`assets`]) — served on `127.0.0.1` only, so
//! `serialwrap daemon` needs nothing else running for a browser to be
//! useful. Remote access is meant to go through `ssh -L` (see the wiki's
//! Client-protocol page and this task's issue): token/TLS are explicitly
//! out of scope for v1.
//!
//! # Module map
//!
//! - [`assets`]: `rust-embed`-backed static file serving + SPA fallback.
//! - [`guard`]: the loopback-only check, applied as middleware.
//! - [`api`]: `GET /api/*` — one-shot queries.
//! - [`stream`]: `WS /api/stream` — connection liveness today (hello +
//!   heartbeat); T5.2 is where this starts actually pushing device
//!   records.
//!
//! # Why this doesn't call into `protocol::session::dispatch`
//!
//! The wiki's Client-protocol page says the GUI "reaches the same request
//! set over HTTP and WebSocket" as the UDS clients — but `dispatch` is a
//! private fn in `protocol::session`, and `protocol/` is out of scope for
//! this task (see the T5.1 issue's stated boundaries). Instead, [`api`]
//! calls straight into the same *public* pieces `dispatch` itself uses
//! (`Shared::backend`, a [`crate::protocol::backend::DeviceBackend`]) —
//! same underlying operation, shaped into JSON independently. A future
//! task that touches `protocol/` might want to extract per-op logic into
//! functions both transports share instead of two call sites drifting
//! apart; noted here rather than attempted now.
mod api;
mod assets;
mod guard;
mod stream;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::protocol::Shared;

pub use guard::is_loopback;

/// Default TCP port the embedded web GUI listens on. Fixed and documented
/// (see `webui/README.md`) rather than announced only after startup — a
/// user typing `http://127.0.0.1:5590` into a browser needs to already
/// know it. Override with `SERIALWRAP_WEB_PORT` (mainly for tests running
/// several daemons at once; production has no reason to).
pub const DEFAULT_PORT: u16 = 5590;

/// Resolve the socket address to bind: always `127.0.0.1` — never
/// configurable to anything else, since binding wider is exactly what
/// this task's "localhost only" requirement exists to prevent. Only the
/// port varies, and only via env var; there's no CLI flag for it.
pub fn web_addr() -> SocketAddr {
    // Known limitation (deferred, not fixed here): a malformed
    // `SERIALWRAP_WEB_PORT` (non-numeric, out of `u16` range) silently
    // falls back to `DEFAULT_PORT` rather than failing loudly — someone
    // debugging "why is it on 5590 when I set the env var" gets no signal
    // at all. Low risk today (this var is set by this crate's own test
    // harness and `webui/e2e/daemon.ts`, never by an end user), but if a
    // CLI flag for this is ever added, it should reject a bad value
    // instead of inheriting this fallback.
    let port = std::env::var("SERIALWRAP_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Build the full router: API + WS routes, the embedded-asset fallback,
/// and the loopback-only guard — everything except actually binding a
/// socket, so tests can drive it with `tower::ServiceExt::oneshot` without
/// any real networking.
pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .merge(api::routes())
        .merge(stream::routes())
        .fallback(assets::serve_asset)
        .with_state(shared)
        .layer(axum::middleware::from_fn(guard::loopback_only))
}

/// Bind `addr` and serve forever. Mirrors `protocol::server::serve`'s
/// "this is a rest-of-process-lifetime future" shape — see
/// [`serve_on`] for the variant tests use when they need the bound
/// address first (e.g. binding port `0`).
pub async fn serve(addr: SocketAddr, shared: Arc<Shared>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_on(listener, shared).await
}

/// Serve on an already-bound `listener` forever, propagating a fatal
/// `axum::serve` error rather than swallowing it — an operator whose
/// browser can never reach the daemon deserves to know why, not a
/// silently-half-working daemon (this project's honesty stance, applied
/// to daemon startup itself: see `serialwrapd::run`'s doc comment on why
/// the web listener bind is `?`-propagated rather than logged-and-skipped).
pub async fn serve_on(listener: TcpListener, shared: Arc<Shared>) -> std::io::Result<()> {
    let local_addr = listener.local_addr()?;
    eprintln!("serialwrapd: web: listening on http://{local_addr}");
    axum::serve(
        listener,
        router(shared).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use crate::protocol::backend::testing::TestBackend;
    use crate::protocol::backend::DeviceBackend;
    use crate::protocol::Shared;

    /// End-to-end over a *real* TCP socket — every other `web` test drives
    /// the router with `tower::ServiceExt::oneshot` and hand-injects a
    /// `ConnectInfo` extension, so none of them would notice a bug in the
    /// actual `into_make_service_with_connect_info::<SocketAddr>()` wiring
    /// this function does (review finding #11 on PR #43: `web::serve` had
    /// zero callers and zero coverage). A real loopback connection's own
    /// peer address should, unsurprisingly, pass the loopback guard.
    #[tokio::test]
    async fn serve_wires_up_a_real_socket_end_to_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test-version",
            tmp.path(),
        ));
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let server = tokio::spawn(super::serve_on(listener, shared));

        let mut stream = TcpStream::connect(bound).await.unwrap();
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);

        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200 OK, got: {response}"
        );
        assert!(response.contains("\"ok\":true"));

        server.abort();
    }
}
