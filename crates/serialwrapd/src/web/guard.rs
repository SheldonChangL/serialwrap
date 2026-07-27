//! Loopback-only guard (`TASKS.md` T5.1, issue #18 — "非 localhost 連線被
//! 拒"). Binding `TcpListener` to `127.0.0.1` already makes the daemon
//! unreachable from another host at the OS level; this middleware is
//! defense in depth against a future change accidentally widening the
//! bind address (e.g. `0.0.0.0` for convenience) — the loopback check is
//! then the only thing standing between the write gate and the network.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Whether `addr`'s IP is loopback — the entire IPv4 `127.0.0.0/8` block
/// (not just `127.0.0.1`) and IPv6 `::1`, matching `IpAddr::is_loopback`'s
/// own definition.
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Reject any request whose peer address isn't loopback with `403`, before
/// it reaches any route handler.
pub async fn loopback_only(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if !is_loopback(&addr) {
        return (
            StatusCode::FORBIDDEN,
            "serialwrap web GUI only accepts connections from localhost; use `ssh -L` for \
             remote access",
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_loopback_block_is_accepted_not_just_127_0_0_1() {
        assert!(is_loopback(&"127.0.0.1:1234".parse().unwrap()));
        assert!(is_loopback(&"127.5.6.7:1".parse().unwrap()));
        assert!(is_loopback(&"127.255.255.255:1".parse().unwrap()));
    }

    #[test]
    fn ipv6_loopback_is_accepted() {
        assert!(is_loopback(&"[::1]:1234".parse().unwrap()));
    }

    #[test]
    fn private_and_public_addresses_are_rejected() {
        assert!(!is_loopback(&"10.0.0.5:1234".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.1:1234".parse().unwrap()));
        assert!(!is_loopback(&"172.16.0.1:1234".parse().unwrap()));
        assert!(!is_loopback(&"8.8.8.8:1234".parse().unwrap()));
        assert!(!is_loopback(&"[2001:db8::1]:1234".parse().unwrap()));
    }

    /// End-to-end through the real router (not just the pure predicate
    /// above): a non-loopback `ConnectInfo` — the same extension type
    /// `axum::serve(...).into_make_service_with_connect_info::<SocketAddr>()`
    /// inserts from the real peer address — must never reach a route
    /// handler at all.
    #[tokio::test]
    async fn non_loopback_connect_info_is_rejected_before_any_route_handler() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        use crate::protocol::backend::testing::TestBackend;
        use crate::protocol::backend::DeviceBackend;
        use crate::protocol::Shared;

        let shared = Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test",
        ));
        let router = crate::web::router(shared);

        let rejected = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/health")
                    .extension(ConnectInfo(
                        "203.0.113.7:9999".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let allowed = router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/health")
                    .extension(ConnectInfo("127.0.0.1:9999".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }
}
