//! `GET /api/*` (`TASKS.md` T5.1, issue #18; extended by T5.2, issue #19):
//! one-shot queries. Deliberately small for T5.1 — just enough for the
//! frontend to prove it can call a real API and render a real result. See
//! [`crate::web`]'s module docs for why this duplicates (rather than calls
//! into) the JSON shaping `protocol::session::dispatch` already does for
//! the equivalent UDS ops.
//!
//! # T5.2 additions
//!
//! - [`tail`]: the live log view's initial page, via
//!   [`crate::query::DeviceQueryState::tail_presented`] — i.e. the exact
//!   same context-protection presentation layer (dedup folding, binary
//!   summarization) T3.2 built for the MCP bridge. The GUI is a consumer of
//!   that layer, not a second implementation of it — see
//!   `crate::presentation`'s module docs, and `webui/src/lib/liveLog.ts`
//!   for the frontend side of this contract.
//! - [`get_config`]: [`crate::port_config::PortConfig`] plus
//!   [`crate::error_counts::ErrorCounts`] for the status bar's config chip
//!   and framing/overrun/parity counters — [`crate::error_counts::ErrorCounts::Unavailable`]
//!   serializes as `{"status":"unavailable"}`, never a bare `0` (see that
//!   type's module docs for why this distinction matters).
//! - [`test_inject`]: gated behind [`crate::TEST_BACKEND_DEVICE_ENV`] —
//!   lets the Playwright E2E suite append real records (rx/tx/event/gate)
//!   to a [`crate::TEST_BACKEND_DEVICE_ENV`]-registered device's real
//!   `Recorder`, so T5.2's acceptance criteria (5,000 lines/sec throughput,
//!   dedup/binary folding, TX/event rendering, follow/pause) can be proven
//!   against the actual compiled daemon binary and the actual
//!   record→recorder→query→presentation→WS pipeline, not a frontend-only
//!   fixture. Never reachable in a real deployment: see that constant's
//!   docs for why gating it on the same env var that switches `run()` to
//!   the test backend is safe.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::port::DeviceId;
use crate::presentation::{page_to_json, PresentationLimits};
use crate::protocol::Shared;
use crate::query::QueryError;
use wrap_proto::ClientType;

pub fn routes() -> Router<Arc<Shared>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}/tail", get(tail))
        .route("/api/devices/{id}/config", get(get_config))
        .route("/api/devices/{id}/test/inject", post(test_inject))
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

/// Query params for [`tail`]. `n` mirrors the UDS `tail` op's own
/// parameter name and default-of-omission convention (an operator asking
/// for "the log" almost always means "the recent log", not the entire
/// history since boot).
#[derive(Debug, Deserialize)]
struct TailParams {
    n: Option<usize>,
}

/// Default number of lines [`tail`] returns when `n` is omitted — enough to
/// fill a typical viewport several times over without the initial page
/// load itself becoming the bottleneck; the live WS subscribe (`stream.rs`)
/// is what keeps the view current from here on.
const DEFAULT_TAIL_LINES: usize = 500;

fn device_not_found_response(device: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "device_not_found", "message": format!("no such device: {device}") } })),
    )
        .into_response()
}

fn query_error_response(e: QueryError) -> axum::response::Response {
    use axum::response::IntoResponse;
    match e {
        QueryError::DataAgedOut {
            oldest_available_seq,
        } => (
            StatusCode::GONE,
            Json(json!({
                "error": {
                    "code": "data_aged_out",
                    "oldest_available_seq": oldest_available_seq,
                }
            })),
        )
            .into_response(),
        QueryError::InvalidPattern(message) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": "invalid_request", "message": message } })),
        )
            .into_response(),
    }
}

/// `GET /api/devices/:id/tail?n=` (`TASKS.md` T5.2, issue #19): the live
/// log view's initial page. Calls
/// [`crate::query::DeviceQueryState::tail_presented`] with the default
/// [`PresentationLimits`] — the exact dedup-folding/binary-summarization
/// layer T3.2 built, reused verbatim (see this module's doc comment).
/// [`page_to_json`]'s `cursor` field is what the frontend hands back as
/// `since_cursor` when it opens the follow-on `WS /api/stream?device=...`
/// subscription (`stream.rs`), closing the tail-then-subscribe gap the
/// same way the UDS `Client-protocol` wiki documents.
async fn tail(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Query(params): Query<TailParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };
    let state = shared.queries.get_or_spawn(&dev, recorder);
    let n = params.n.unwrap_or(DEFAULT_TAIL_LINES);
    match state.tail_presented(n, None, &PresentationLimits::default()) {
        Ok(page) => Json(page_to_json(&page)).into_response(),
        Err(e) => query_error_response(e),
    }
}

/// `GET /api/devices/:id/config` (`TASKS.md` T5.2, issue #19): current port
/// configuration plus framing/overrun/parity error counters, for the
/// status bar's config chip and always-on error counters. `error_counts`
/// serializes [`crate::error_counts::ErrorCounts::Unavailable`] as
/// `{"status":"unavailable"}` — never a bare `0` — so the frontend has no
/// numeric field to accidentally reach for when the platform (or, in
/// `TEST_BACKEND_DEVICE_ENV` mode, the test backend — see that constant's
/// docs) never measured anything at all.
async fn get_config(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    let config = match shared.backend.get_config(&dev) {
        Ok(c) => c,
        Err(_) => return device_not_found_response(&id),
    };
    let error_counts = shared.backend.error_counts(&dev).ok();
    Json(json!({
        "config": config,
        "error_counts": error_counts,
    }))
    .into_response()
}

/// One synthetic record [`test_inject`] can append. Deliberately mirrors
/// [`crate::recorder::Recorder`]'s own `append_*` method shapes field for
/// field rather than inventing a new schema — this endpoint's whole job is
/// "drive the real recorder the way a real device/client would", not
/// define a parallel test-only representation.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InjectOp {
    /// Raw device output. `text`'s bytes are appended as-is (a trailing
    /// `\n` is *not* added automatically — callers wanting a complete line
    /// must include it, same as a real device's byte stream) unless
    /// `data_b64` is given instead, for injecting deliberately-invalid
    /// UTF-8 (the binary-summary/hex-chip acceptance criterion).
    Rx {
        text: Option<String>,
        data_b64: Option<String>,
    },
    Tx {
        text: Option<String>,
        data_b64: Option<String>,
        client: String,
        client_type: ClientType,
        gate: String,
    },
    Event {
        name: String,
        #[serde(default)]
        extra: serde_json::Map<String, Value>,
    },
    Gate {
        action: String,
        reason: String,
        request_seq: u64,
    },
}

#[derive(Debug, Deserialize)]
struct InjectBody {
    ops: Vec<InjectOp>,
}

fn resolve_bytes(text: Option<String>, data_b64: Option<String>) -> Result<Vec<u8>, String> {
    if let Some(b64) = data_b64 {
        return BASE64
            .decode(&b64)
            .map_err(|e| format!("invalid data_b64: {e}"));
    }
    Ok(text.unwrap_or_default().into_bytes())
}

/// `POST /api/devices/:id/test/inject` (`TASKS.md` T5.2, issue #19) — see
/// this module's doc comment and [`crate::TEST_BACKEND_DEVICE_ENV`] for why
/// this exists and why it can only ever do anything in a daemon started
/// with that env var set. A production deployment (the env var unset)
/// always 404s here, *before* even looking at `id` — the fabricate-log-
/// records capability must not exist at all outside an explicit test run,
/// since this project's entire value proposition rests on the recorded
/// log being what the device actually said (see the UX-design wiki's
/// "gaps are shown, never smoothed" principle — a log that can be silently
/// injected into is the same trust failure in a different shape).
async fn test_inject(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(body): Json<InjectBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if std::env::var(crate::TEST_BACKEND_DEVICE_ENV).is_err() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };
    let mut count = 0u64;
    for op in body.ops {
        let result = match op {
            InjectOp::Rx { text, data_b64 } => resolve_bytes(text, data_b64)
                .and_then(|bytes| recorder.append_rx(&bytes).map_err(|e| e.to_string())),
            InjectOp::Tx {
                text,
                data_b64,
                client,
                client_type,
                gate,
            } => resolve_bytes(text, data_b64).and_then(|bytes| {
                recorder
                    .append_tx(&bytes, client, client_type, gate)
                    .map_err(|e| e.to_string())
            }),
            InjectOp::Event { name, extra } => recorder
                .append_event(name, extra)
                .map_err(|e| e.to_string()),
            InjectOp::Gate {
                action,
                reason,
                request_seq,
            } => recorder
                .append_gate(action, reason, request_seq)
                .map_err(|e| e.to_string()),
        };
        match result {
            Ok(_) => count += 1,
            Err(message) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": { "code": "invalid_request", "message": message, "injected": count } })),
                )
                    .into_response();
            }
        }
    }
    Json(json!({ "ok": true, "injected": count })).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::port::DeviceId;
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

    // ---- T5.2 (issue #19) additions ----

    /// Serializes any test that mutates the process-wide
    /// `TEST_BACKEND_DEVICE_ENV` var, since `cargo test` runs every `#[test]`
    /// in this binary as threads sharing one process's environment —
    /// without this, `test_inject_...env_var_set`/`...env_var_unset` could
    /// interleave and see each other's var state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn shared_with_device(device_id: &str) -> (Arc<Shared>, tempfile::TempDir, DeviceId) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(
            crate::recorder::Recorder::open(
                tmp.path(),
                device_id,
                crate::recorder::RecorderConfig::default(),
            )
            .expect("open recorder"),
        );
        let backend = Arc::new(TestBackend::new());
        let id = DeviceId(device_id.to_string());
        backend.register(id.clone(), recorder);
        let shared = Arc::new(Shared::new(
            backend as Arc<dyn DeviceBackend>,
            "test-version",
        ));
        (shared, tmp, id)
    }

    async fn get(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
        };
        (status, value)
    }

    async fn post(
        router: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
        };
        (status, value)
    }

    #[tokio::test]
    async fn tail_404s_for_an_unknown_device() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = get(
            crate::web::router(shared),
            "/api/devices/no-such-device/tail",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "device_not_found");
    }

    /// Pins the exact contract T5.2's report calls out: the GUI's `tail`
    /// endpoint reuses `presentation::present` verbatim, so three identical
    /// lines fold into one `PresentedLine::Fold` entry here exactly the way
    /// `presentation.rs`'s own unit tests already prove for the underlying
    /// function — this test would fail if the web layer ever grew its own,
    /// separate folding logic instead of calling into T3.2's.
    #[tokio::test]
    async fn tail_applies_the_presentation_layers_dedup_folding() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        for _ in 0..3 {
            recorder.append_rx(b"read timeout\n").unwrap();
        }
        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/tail").await;
        assert_eq!(status, StatusCode::OK);
        let lines = body["lines"].as_array().expect("lines array");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["folded"], true);
        assert_eq!(lines[0]["count"], 3);
        assert_eq!(lines[0]["text"], "read timeout");
    }

    #[tokio::test]
    async fn tail_respects_the_n_query_param() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        for i in 0..10 {
            recorder
                .append_rx(format!("line-{i}\n").as_bytes())
                .unwrap();
        }
        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/tail?n=2").await;
        assert_eq!(status, StatusCode::OK);
        let lines = body["lines"].as_array().expect("lines array");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["text"], "line-8");
        assert_eq!(lines[1]["text"], "line-9");
    }

    #[tokio::test]
    async fn config_404s_for_an_unknown_device() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, _) = get(
            crate::web::router(shared),
            "/api/devices/no-such-device/config",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// `TestBackend::error_counts` always reports `Unavailable` (it has no
    /// real fd/ioctl underneath) — this pins the honest-unavailable
    /// acceptance criterion's *wire shape* (`{"status":"unavailable"}`,
    /// never a bare `0`) for the GUI's status bar, independent of which
    /// platform actually runs the test.
    #[tokio::test]
    async fn config_reports_port_config_and_unavailable_error_counts() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/config").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["config"]["baud"], 9600);
        assert_eq!(body["error_counts"]["status"], "unavailable");
        assert!(
            body["error_counts"].get("framing").is_none(),
            "unavailable must carry no numeric fields at all: {body}"
        );
    }

    // These three tests deliberately hold `ENV_LOCK` across `.await` points:
    // the whole point of the lock is to keep *other test threads* from
    // mutating the same process-wide env var while this test's async body
    // (which needs to await the router) is in flight — a `tokio::test`'s
    // single-threaded runtime never contends with itself for this lock, so
    // there's no deadlock risk, only the cross-thread exclusion this is for.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_inject_404s_when_the_env_var_is_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, _) = post(
            crate::web::router(shared),
            "/api/devices/dev-1/test/inject",
            json!({ "ops": [{ "kind": "rx", "text": "hi\n" }] }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "test_inject must never do anything in a non-test-backend daemon"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_inject_appends_rx_tx_event_and_gate_records_when_enabled() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let router = crate::web::router(shared);
        let (status, body) = post(
            router.clone(),
            "/api/devices/dev-1/test/inject",
            json!({
                "ops": [
                    { "kind": "rx", "text": "boot ok\n" },
                    { "kind": "tx", "text": "status\n", "client": "claude-code", "client_type": "agent", "gate": "whitelist" },
                    { "kind": "event", "name": "config_change", "extra": { "field": "baud", "old": 9600, "new": 115200 } },
                    { "kind": "gate", "action": "deny", "reason": "timeout_60s", "request_seq": 0 },
                ]
            }),
        )
        .await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["injected"], 4);

        let (_status, page) = get(router, "/api/devices/dev-1/tail").await;
        let lines = page["lines"].as_array().expect("lines");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["text"], "boot ok");
        let events = page["events"].as_array().expect("events");
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[0]["kind"], "tx");
        assert_eq!(events[0]["client"], "claude-code");
        assert_eq!(events[0]["gate"], "whitelist");
        assert_eq!(events[1]["kind"], "event");
        assert_eq!(events[1]["event"], "config_change");
        assert_eq!(events[1]["new"], 115200);
        assert_eq!(events[2]["kind"], "gate");
        assert_eq!(events[2]["action"], "deny");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_inject_invalid_base64_is_a_structured_400_not_a_panic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared),
            "/api/devices/dev-1/test/inject",
            json!({ "ops": [{ "kind": "rx", "data_b64": "not-valid-base64!!" }] }),
        )
        .await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
    }
}
