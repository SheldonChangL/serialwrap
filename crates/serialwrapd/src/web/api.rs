//! `GET /api/*` (`TASKS.md` T5.1, issue #18): one-shot queries. Deliberately
//! small for this task — just enough for the frontend to prove it can call
//! a real API and render a real result. See [`crate::web`]'s module docs
//! for why this duplicates (rather than calls into) the JSON shaping
//! `protocol::session::dispatch` already does for the equivalent UDS ops.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::protocol::Shared;

pub fn routes() -> Router<Arc<Shared>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/devices", get(list_devices))
}

/// Liveness/readiness probe — used by the E2E harness (`webui/e2e/`) to
/// know the daemon is up before navigating a browser to it, and a
/// reasonable thing for an operator's own tooling to poll too.
async fn health(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({ "ok": true, "server_version": shared.server_version }))
}

/// Mirrors `Request::ListDevices`'s reply shape (`protocol::session`'s
/// `dispatch`) field-for-field, so a client already speaking the UDS wire
/// format sees the same shape here — not because any code is shared (see
/// this module's doc comment).
async fn list_devices(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let devices = shared.backend.list_devices();
    let arr: Vec<Value> = devices
        .iter()
        .map(|d| {
            json!({
                "id": d.id.0,
                "path": d.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "connected": d.connected,
                "config": d.config,
            })
        })
        .collect();
    Json(json!({ "devices": arr }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::protocol::backend::testing::TestBackend;
    use crate::protocol::backend::DeviceBackend;
    use crate::protocol::Shared;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test-version",
        ))
    }

    #[tokio::test]
    async fn health_reports_ok_and_server_version() {
        let router = crate::web::router(shared());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["server_version"], "test-version");
    }

    #[tokio::test]
    async fn devices_endpoint_reflects_the_backend_with_no_devices_registered() {
        let router = crate::web::router(shared());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/devices")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["devices"].as_array().unwrap().len(), 0);
    }

    /// Regression test for a review finding on PR #43 (#6): the only
    /// previous `/api/devices` test asserted an *empty* array, so the
    /// `id`/`path`/`connected`/`config` field mapping itself had zero
    /// coverage — renaming a field would still pass every Rust test, and
    /// CI has no real serial device for the Playwright E2E to catch it
    /// either. This pins the exact shape against a real registered device.
    #[tokio::test]
    async fn devices_endpoint_maps_a_registered_device_field_for_field() {
        use crate::port::DeviceId;
        use crate::recorder::{Recorder, RecorderConfig};

        let tmp = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(
            Recorder::open(tmp.path(), "dev-1", RecorderConfig::default()).expect("open recorder"),
        );
        let backend = Arc::new(TestBackend::new());
        backend.register(DeviceId("dev-1".to_string()), recorder);
        let shared = Arc::new(Shared::new(
            backend as Arc<dyn DeviceBackend>,
            "test-version",
        ));

        let router = crate::web::router(shared);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/devices")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let devices = value["devices"].as_array().expect("devices array");
        assert_eq!(devices.len(), 1);
        let device = &devices[0];
        assert_eq!(device["id"], "dev-1");
        assert_eq!(device["connected"], true);
        assert!(
            device["path"].is_null(),
            "TestBackend::register doesn't set a path"
        );
        assert_eq!(
            device["config"]["baud"], 9600,
            "PortConfig::default()'s baud, round-tripped through the config field"
        );
    }
}
