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
//!
//! # T5.3 additions (issue #20)
//!
//! - `POST /api/devices/:id/config`/[`set_config`] and `POST
//!   /api/devices/:id/control_lines`/[`set_control_lines`]: the port
//!   settings popover's write side. Both call straight into
//!   [`crate::protocol::backend::DeviceBackend`] exactly like
//!   `Request::SetConfig`/`Request::SetControlLine`'s UDS handlers do
//!   (`protocol::session`, out of this task's scope to touch) — same
//!   ungated-for-humans posture: the Security-model wiki's policy table
//!   gates only `dtr_pulse` (a hardware pulse/reset), not a plain
//!   baud/frame change or a one-shot DTR/RTS assert, and the GUI operator is
//!   always the `human`/trusted party in that table, never an `agent`. See
//!   [`GUI_CHANGED_BY`] for the identity string these use in place of a
//!   kernel-verified UDS peer.
//! - [`get_config`] additionally computes `decode_health` — see
//!   [`compute_decode_health`]'s doc comment for why this API didn't already
//!   exist and what it measures.
//! - [`list_approvals`]/[`approve_approval`]/[`deny_approval`]: the
//!   approval card's read/decide API (T5.4, issue #21) — calls
//!   [`crate::protocol::Shared::gate`] directly, the *exact* [`crate::gate::Gate`]
//!   instance `serialwrap approvals`/`approvals approve`/`approvals deny`
//!   already decide through over UDS (`protocol::session`'s
//!   `Request::ApprovalsList`/`ApprovalApprove`/`ApprovalDeny` handlers) —
//!   there is only ever one [`crate::gate::approval::PendingQueue`] per
//!   daemon (see `lib.rs`'s `serve_forever`: one `Arc<Shared>` feeds both the
//!   UDS and web listeners), so a CLI decision and a GUI decision racing the
//!   same pending id resolve through the identical atomic
//!   `PendingQueue::decide` — whichever call actually removes the entry
//!   wins, the other gets [`crate::gate::approval::DecideError`], mapped
//!   here to `409 already_decided` (see [`decide_error_response`]).
//! - [`test_submit_write`]: gated behind [`crate::TEST_BACKEND_DEVICE_ENV`]
//!   exactly like [`test_inject`] — lets the E2E suite simulate "an `agent`
//!   client asked to write a gated command" (there is no in-browser way to
//!   open a real UDS connection as an MCP/CLI client) by calling
//!   [`crate::gate::Gate::submit_write`] directly, the same entry point
//!   `Request::Write`'s handler uses. Deliberately does *not* call
//!   [`crate::protocol::backend::DeviceBackend::write_bytes`] on approval —
//!   this harness has no real device fd behind it in E2E mode (`TestBackend`
//!   with no `register_writer`, a Rust-test-only API not reachable over
//!   HTTP) and this task's scope is proving the *approval flow*, not
//!   byte-level write fidelity (already covered elsewhere). It appends the
//!   `tx` audit record an approved write would produce directly instead, so
//!   "指令執行 → 稽核有紀錄" (T5.4 acceptance criterion 7) is provable from
//!   the same stream every other audit record lives in.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gate::approval::{DecideError, Decision};
use crate::gate::{GateDecision, RequesterCtx, DEFAULT_LOG_CONTEXT_LINES};
use crate::port::DeviceId;
use crate::presentation::{page_to_json, PresentationLimits};
use crate::protocol::Shared;
use crate::query::{AssembledLine, QueryError};
use wrap_proto::ClientType;

/// Identity string the GUI's ungated config/control-line/approval endpoints
/// record as `changed_by`/`approved_by` in place of a kernel-verified UDS
/// peer (`"<name>:<pid>"` elsewhere in this crate — see
/// `protocol::session`'s `changed_by` convention). The embedded web GUI has
/// no equivalent: [`crate::web::guard`]'s loopback-only check authenticates
/// *the machine*, not a per-request user, and this project ships no
/// authentication layer in v1 at all (see the Security-model wiki's
/// "Network exposure" section) — there is exactly one operator per running
/// daemon in that model, so a fixed label is honest rather than inventing a
/// per-request identity this transport genuinely doesn't have.
const GUI_CHANGED_BY: &str = "gui";

pub fn routes() -> Router<Arc<Shared>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}/tail", get(tail))
        .route("/api/devices/{id}/config", get(get_config).post(set_config))
        .route("/api/devices/{id}/control_lines", post(set_control_lines))
        .route("/api/devices/{id}/test/inject", post(test_inject))
        .route(
            "/api/devices/{id}/test/submit_write",
            post(test_submit_write),
        )
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}/approve", post(approve_approval))
        .route("/api/approvals/{id}/deny", post(deny_approval))
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

/// `GET /api/devices/:id/config` (`TASKS.md` T5.2, issue #19; `decode_health`
/// added by T5.3, issue #20): current port configuration plus
/// framing/overrun/parity error counters, for the status bar's config chip
/// and always-on error counters. `error_counts` serializes
/// [`crate::error_counts::ErrorCounts::Unavailable`] as
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
    let decode_health = match shared.backend.recorder(&dev) {
        Some(recorder) => {
            let state = shared.queries.get_or_spawn(&dev, Arc::clone(&recorder));
            // Same reasoning as `tail`'s `get_or_spawn` call: an `ingest`
            // just happened synchronously on first reference, but an
            // already-cached state (e.g. a live-log view already open on
            // this device) only gets refreshed on its own 5ms poller tick —
            // a one-shot `GET` here must not report stale decode health
            // while it waits for that tick.
            state.ingest(&recorder);
            let lines = state
                .tail(DECODE_HEALTH_WINDOW_LINES, None)
                .map(|page| page.lines)
                .unwrap_or_default();
            compute_decode_health(&lines, config.baud)
        }
        None => DecodeHealth::default(),
    };
    Json(json!({
        "config": config,
        "error_counts": error_counts,
        "decode_health": decode_health,
    }))
    .into_response()
}

/// `POST /api/devices/:id/config` (T5.3, issue #20): the port settings
/// popover's "Apply" action. `body` is a partial [`crate::port_config::PortConfig`]
/// patch — the exact same merge-onto-current semantics
/// `wrap_proto::Request::SetConfig`'s `config` field already documents (see
/// [`crate::protocol::backend::DeviceBackend::set_config`]) — so a caller
/// only ever sends the fields the operator actually changed (e.g. just
/// `{"baud": 74880}`), same as the "還原" (revert) button, which replays a
/// `config_change` event's whole `old` value back through this same
/// endpoint. Ungated — see [`GUI_CHANGED_BY`]'s doc comment.
async fn set_config(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(patch): Json<serde_json::Map<String, Value>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    match shared.backend.set_config(&dev, &patch, GUI_CHANGED_BY) {
        Ok(config) => Json(json!({ "config": config })).into_response(),
        Err(e) => backend_error_response(&e, &id),
    }
}

/// Body for [`set_control_lines`] — mirrors `wrap_proto::Request::SetControlLine`'s
/// `dtr`/`rts` fields (each `None` means "leave this line untouched", the
/// same "independently optional" semantics that request already has).
#[derive(Debug, Deserialize)]
struct ControlLinesBody {
    dtr: Option<bool>,
    rts: Option<bool>,
}

/// `POST /api/devices/:id/control_lines` (T5.3, issue #20): the port
/// settings popover's DTR/RTS toggle switches — a live, immediate assert,
/// deliberately a separate endpoint from [`set_config`] (which only ever
/// touches [`crate::port_config::PortConfig`]'s *open-time* policy field,
/// `open_control_lines`) per the UX-design wiki's "control lines are
/// separated from data settings" principle: toggling a data setting like
/// baud should never, as a side effect of one "Apply" click, also pulse a
/// physical line and reset the board. Ungated — see [`GUI_CHANGED_BY`]'s
/// doc comment (and, for why a plain assert is ungated even for an `agent`
/// while `dtr_pulse` is not, this module's own doc comment).
async fn set_control_lines(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(body): Json<ControlLinesBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    match shared
        .backend
        .set_control_line(&dev, body.dtr, body.rts, GUI_CHANGED_BY)
    {
        Ok(()) => Json(json!({ "dtr": body.dtr, "rts": body.rts })).into_response(),
        Err(e) => backend_error_response(&e, &id),
    }
}

/// Map a [`crate::protocol::backend::DeviceBackend`] error to an HTTP
/// response — shared by [`set_config`]/[`set_control_lines`]. Not a full
/// mirror of `protocol::session`'s private `backend_error_to_wire` (out of
/// this task's scope to touch/reuse — see `web::mod`'s module doc comment on
/// why this layer independently shapes JSON rather than calling into
/// `protocol::session`); this is deliberately simpler, since the web layer
/// only needs "which HTTP status" and "what went wrong", not the full wire
/// error-code taxonomy the UDS protocol carries.
fn backend_error_response(e: &std::io::Error, device: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    if e.kind() == std::io::ErrorKind::NotFound {
        return device_not_found_response(device);
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": { "code": "invalid_request", "message": e.to_string() } })),
    )
        .into_response()
}

/// How many of a device's most-recently-assembled lines [`compute_decode_health`]
/// inspects. Small and fixed (not "since last config change" or similar) —
/// this is meant to answer "is what's arriving *right now* decodable",
/// which a short recent window does better than an ever-growing one that
/// would dilute a genuinely garbled stream with clean history from before a
/// baud mismatch started.
const DECODE_HEALTH_WINDOW_LINES: usize = 50;

/// Below this many sampled bytes, [`compute_decode_health`] never suggests a
/// baud change — a one- or two-line sample is too small for "most of this
/// is undecodable" to mean anything (a single stray byte would swing the
/// ratio wildly).
const DECODE_HEALTH_MIN_BYTES: usize = 32;

/// Fraction of undecodable bytes at/above which [`compute_decode_health`]
/// suggests an alternate baud. The UX-design wiki's own mockup example is a
/// dramatic 92%; this is deliberately far more sensitive (any baud mismatch
/// this task has actually observed — e.g. 9600 vs. 115200 — corrupts most
/// bytes almost immediately, not marginally), while still comfortably above
/// the noise a handful of genuinely binary bytes in otherwise-clean text
/// would produce.
const DECODE_HEALTH_THRESHOLD: f64 = 0.2;

/// Common baud rates [`suggest_alternate_baud`] picks from, in the order
/// they're tried — 115200 and 74880 first because they're this project's
/// own two most-cited rates (the UX-design wiki's mockup names both: a
/// generic "commonly wrong" default and the ESP8266 boot-log rate this
/// whole feature was motivated by), the rest a standard descending list.
const COMMON_BAUD_CANDIDATES: &[u32] = &[115_200, 74_880, 9600, 57_600, 38_400, 19_200, 230_400];

/// Count how many bytes of `bytes` are part of an invalid UTF-8 sequence —
/// the raw measurement [`compute_decode_health`]'s ratio is built from.
/// Walks `str::from_utf8`'s own error reporting rather than reimplementing
/// UTF-8 validation: `Utf8Error::valid_up_to` is the prefix that *did*
/// decode, and `Utf8Error::error_len` is `Some(n)` for a genuinely invalid
/// n-byte sequence or `None` only for a truncated sequence at the very end
/// of the slice (which, since there's no more input coming in a
/// point-in-time sample like this, is counted as undecodable too — treating
/// it as "fine" would understate exactly the case a real baud mismatch
/// produces at a chunk boundary).
fn count_invalid_utf8_bytes(bytes: &[u8]) -> usize {
    let mut invalid = 0usize;
    let mut offset = 0usize;
    loop {
        let rest = &bytes[offset..];
        match std::str::from_utf8(rest) {
            Ok(_) => break,
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let bad_len = e.error_len().unwrap_or(rest.len() - valid_up_to);
                invalid += bad_len;
                offset += valid_up_to + bad_len;
                if offset >= bytes.len() {
                    break;
                }
            }
        }
    }
    invalid
}

/// Pick a baud to suggest instead of `current` — the first entry in
/// [`COMMON_BAUD_CANDIDATES`] that isn't `current` itself. Deliberately not
/// an attempt to *derive* the device's actual correct baud from the garbled
/// bytes themselves: a wrong-baud UART mismatch corrupts bits during
/// sampling, not just framing, so there is no decode-and-compare trick that
/// recovers the right rate from already-corrupted bytes after the fact —
/// only reopening the port at a candidate rate and checking would (out of
/// this task's scope: `port*.rs` is explicitly off-limits). This is
/// consequently a *suggestion* in the same spirit as the UX-design wiki's
/// own mockup text ("this chip commonly boots at 74880"): a reasonable
/// next-thing-to-try, not a measurement.
fn suggest_alternate_baud(current: u32) -> u32 {
    COMMON_BAUD_CANDIDATES
        .iter()
        .copied()
        .find(|&b| b != current)
        .unwrap_or(115_200)
}

/// Result of [`compute_decode_health`] — serialized directly as `GET
/// /api/devices/:id/config`'s `decode_health` field.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
struct DecodeHealth {
    /// Total raw bytes sampled (across up to [`DECODE_HEALTH_WINDOW_LINES`]
    /// recent lines) — `0` when the device has no assembled lines yet.
    checked_bytes: usize,
    /// `count_invalid_utf8_bytes(sampled) / checked_bytes`, or `0.0` when
    /// `checked_bytes` is `0` (nothing sampled is not the same claim as
    /// "everything sampled decoded fine", but there's also nothing here to
    /// warn about either).
    undecodable_ratio: f64,
    /// `Some(baud)` only when both [`DECODE_HEALTH_MIN_BYTES`] and
    /// [`DECODE_HEALTH_THRESHOLD`] are met — see [`suggest_alternate_baud`].
    suggested_baud: Option<u32>,
}

impl Default for DecodeHealth {
    fn default() -> Self {
        Self {
            checked_bytes: 0,
            undecodable_ratio: 0.0,
            suggested_baud: None,
        }
    }
}

/// Whether recent output looks like it's arriving at the wrong baud rate,
/// and if so, what to try instead (`TASKS.md` T5.3, issue #20's "亂碼偵測建
/// 議" requirement). This API did not exist before this task — confirmed by
/// grepping this crate for any existing undecodable-ratio/baud-suggestion
/// logic before writing this (none found) — so it's added here, scoped to
/// exactly this one piece of read-only observability: no new persisted
/// state, no change to how bytes are recorded or decoded elsewhere (`raw`
/// stays the untouched source of truth everywhere else in this crate; this
/// function only ever samples it to compute a ratio for display).
///
/// `lines` should be the device's most-recently-assembled
/// [`AssembledLine`]s (see [`get_config`]'s call site: `DeviceQueryState::tail`)
/// — each already has its terminating `\n` stripped and its exact original
/// bytes in `raw` (never `text`, which is already lossily re-encoded and
/// would hide the very thing this function measures).
fn compute_decode_health(lines: &[AssembledLine], current_baud: u32) -> DecodeHealth {
    let mut checked_bytes = 0usize;
    let mut invalid = 0usize;
    for line in lines {
        checked_bytes += line.raw.len();
        invalid += count_invalid_utf8_bytes(&line.raw);
    }
    let undecodable_ratio = if checked_bytes == 0 {
        0.0
    } else {
        invalid as f64 / checked_bytes as f64
    };
    let suggested_baud = (checked_bytes >= DECODE_HEALTH_MIN_BYTES
        && undecodable_ratio >= DECODE_HEALTH_THRESHOLD)
        .then(|| suggest_alternate_baud(current_baud));
    DecodeHealth {
        checked_bytes,
        undecodable_ratio,
        suggested_baud,
    }
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

// ---------------------------------------------------------------------
// T5.4 (issue #21): the approval card's read/decide API.
// ---------------------------------------------------------------------

/// `GET /api/approvals`: every currently pending write, across every
/// device — the same [`crate::gate::approval::ApprovalSnapshot`] shape
/// `serialwrap approvals`'s plain-text listing already renders field-by-
/// field (`crates/serialwrap/src/cli/approvals.rs`), here returned as JSON
/// wholesale via its own `#[derive(Serialize)]`. There is no per-device
/// filter: v1 is one device per browser tab (see the UX-design wiki's
/// "deliberate omissions"), and a stray approval for a *different* device
/// still carries its own `device` field, so the card can simply say whose
/// approval it is rather than the endpoint silently hiding it.
async fn list_approvals(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({ "approvals": shared.gate.list() }))
}

/// Map a [`DecideError`] (id already resolved by someone else, or never
/// existed) to `409 Conflict` — the wire signal the approval card's own
/// "don't produce a double decision" handling reacts to (T5.4 acceptance
/// criterion 3): whichever of a concurrent CLI `approvals approve`/`deny`
/// and this same GUI click loses the race to
/// [`crate::gate::approval::PendingQueue::decide`]'s atomic
/// `HashMap::remove` gets exactly this response, never a silent success.
fn decide_error_response(id: u64, e: DecideError) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": {
                "code": "already_decided",
                "message": e.to_string(),
                "approval_id": id,
            }
        })),
    )
        .into_response()
}

/// `POST /api/approvals/:id/approve` — calls
/// [`crate::protocol::Shared::gate`]'s [`crate::gate::Gate::decide`]
/// directly, same as `Request::ApprovalApprove`'s UDS handler (out of this
/// task's scope to touch — see this module's doc comment on why the web
/// layer independently calls the same public pieces rather than that
/// handler itself). `approved_by` is [`GUI_CHANGED_BY`]: see that constant's
/// doc comment.
async fn approve_approval(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<u64>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match shared.gate.decide(
        id,
        Decision::Approved {
            approved_by: GUI_CHANGED_BY.to_string(),
        },
    ) {
        Ok(()) => {
            Json(json!({ "ok": true, "approval_id": id, "decision": "approved" })).into_response()
        }
        Err(e) => decide_error_response(id, e),
    }
}

/// Optional body for [`deny_approval`] — an empty JSON object (`{}`) is a
/// valid, reason-less deny; a caller only sends `reason` when the operator
/// typed one in.
#[derive(Debug, Deserialize, Default)]
struct DenyBody {
    reason: Option<String>,
}

/// `POST /api/approvals/:id/deny` — mirrors `Request::ApprovalDeny`'s
/// default-reason convention (`"denied_by_operator:<changed_by>"`) when
/// `reason` is omitted.
async fn deny_approval(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<u64>,
    Json(body): Json<DenyBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let reason = body
        .reason
        .unwrap_or_else(|| format!("denied_by_operator:{GUI_CHANGED_BY}"));
    match shared.gate.decide(id, Decision::Denied { reason }) {
        Ok(()) => {
            Json(json!({ "ok": true, "approval_id": id, "decision": "denied" })).into_response()
        }
        Err(e) => decide_error_response(id, e),
    }
}

/// Body for [`test_submit_write`] — deliberately shaped like an `agent`
/// client's own `write` request (bytes + who's asking) rather than mirroring
/// [`InjectBody`]'s record-shape convention, since this endpoint drives the
/// *live* gate decision path, not a pre-recorded fixture.
#[derive(Debug, Deserialize)]
struct TestSubmitWriteBody {
    text: Option<String>,
    data_b64: Option<String>,
    requester_name: String,
    requester_pid: u32,
    /// Defaults to `agent` — the one client type the write gate actually
    /// gates (see the Security-model wiki's policy-by-client-type table);
    /// a test simulating a `human`/`tool` write has no reason to go through
    /// this endpoint in the first place (both bypass the gate entirely).
    #[serde(default)]
    client_type: Option<ClientType>,
    #[serde(default)]
    session_request_no: Option<u64>,
}

/// `POST /api/devices/:id/test/submit_write` (T5.4, issue #21): gated behind
/// [`crate::TEST_BACKEND_DEVICE_ENV`] exactly like [`test_inject`] — see
/// this module's doc comment for why this exists and what it deliberately
/// does *not* do (a real `DeviceBackend::write_bytes` call).
async fn test_submit_write(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(body): Json<TestSubmitWriteBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if std::env::var(crate::TEST_BACKEND_DEVICE_ENV).is_err() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };
    let bytes = match resolve_bytes(body.text, body.data_b64) {
        Ok(bytes) => bytes,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "code": "invalid_request", "message": message } })),
            )
                .into_response();
        }
    };

    let state = shared.queries.get_or_spawn(&dev, Arc::clone(&recorder));
    state.ingest(&recorder);
    // Same "log lines immediately before this request" context a real
    // `Request::Write` handler fetches (`protocol::session`) — reproduced
    // here rather than shared, per this module's doc comment.
    let log_context = state
        .tail(DEFAULT_LOG_CONTEXT_LINES, None)
        .map(|page| page.lines.into_iter().map(|l| l.text).collect())
        .unwrap_or_default();

    let client_type = body.client_type.unwrap_or(ClientType::Agent);
    let ctx = RequesterCtx {
        device: id.clone(),
        name: body.requester_name.clone(),
        pid: body.requester_pid,
        client_type,
        session_request_no: body.session_request_no.unwrap_or(1),
    };
    let (decision, rx) = shared
        .gate
        .submit_write(&recorder, &bytes, ctx, log_context);
    let changed_by = format!("{}:{}", body.requester_name, body.requester_pid);

    let response_body = match &decision {
        GateDecision::Allow { reason } => {
            // An immediately-allowed write still produces a `tx` record in
            // real production (`write_and_reply`, `protocol::session`) —
            // reproduced here so a whitelisted test write is just as
            // visible in the audit trail as a gated-then-approved one.
            if let Err(e) = recorder.append_tx(&bytes, changed_by.as_str(), client_type, reason) {
                eprintln!(
                    "serialwrapd: web: test_submit_write: failed to append tx record for an \
                     immediately-allowed write on {id}: {e}"
                );
            }
            json!({ "decision": "allow", "reason": reason })
        }
        GateDecision::Pending { id: approval_id } => {
            spawn_test_write_completion(Arc::clone(&recorder), bytes, changed_by, client_type, rx);
            json!({ "decision": "pending", "id": approval_id })
        }
        GateDecision::ForcePending {
            id: approval_id,
            matched_rule,
        } => {
            spawn_test_write_completion(Arc::clone(&recorder), bytes, changed_by, client_type, rx);
            json!({ "decision": "force_pending", "id": approval_id, "matched_rule": matched_rule })
        }
    };
    Json(response_body).into_response()
}

/// Background half of [`test_submit_write`]'s `Pending`/`ForcePending` path:
/// await the eventual [`Decision`] and, only on approval, append the `tx`
/// record a real approved write would produce (see [`test_submit_write`]'s
/// doc comment for why no real `write_bytes` call happens here). A denial
/// (operator-denied or timed out) needs nothing further — it's already
/// fully audited by [`crate::gate::approval::PendingQueue::decide`]'s own
/// `gate` record, appended synchronously before this task ever runs.
fn spawn_test_write_completion(
    recorder: Arc<crate::recorder::Recorder>,
    bytes: Vec<u8>,
    changed_by: String,
    client_type: ClientType,
    rx: Option<tokio::sync::oneshot::Receiver<Decision>>,
) {
    let Some(rx) = rx else {
        // Unreachable in practice: `Gate::submit_write` always returns a
        // receiver for `Pending`/`ForcePending` (its own doc comment says
        // so) — defensive, not a real branch this function's callers hit.
        return;
    };
    tokio::spawn(async move {
        if let Ok(Decision::Approved { approved_by }) = rx.await {
            let gate_label = format!("approved_by:{approved_by}");
            if let Err(e) = recorder.append_tx(&bytes, changed_by.as_str(), client_type, gate_label)
            {
                eprintln!(
                    "serialwrapd: web: test_submit_write: failed to append tx record after \
                     approval: {e}"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
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

    // ---- T5.3 (issue #20): decode-health / baud-suggestion pure functions ----

    #[test]
    fn count_invalid_utf8_bytes_is_zero_for_clean_ascii() {
        assert_eq!(count_invalid_utf8_bytes(b"boot ok, all good here"), 0);
    }

    #[test]
    fn count_invalid_utf8_bytes_counts_a_lone_continuation_byte() {
        // A single stray 0xFF is exactly one invalid byte, not the whole
        // buffer — a baud-mismatch stream is mostly garbage, but this
        // pins that a single bad byte among otherwise-clean text doesn't
        // get over-counted.
        let bytes = b"before\xFFafter";
        assert_eq!(count_invalid_utf8_bytes(bytes), 1);
    }

    #[test]
    fn count_invalid_utf8_bytes_counts_every_byte_of_a_fully_garbled_run() {
        let bytes: Vec<u8> = (0..40).map(|i| 0x80u8.wrapping_add(i)).collect();
        assert_eq!(count_invalid_utf8_bytes(&bytes), bytes.len());
    }

    #[test]
    fn count_invalid_utf8_bytes_counts_a_truncated_multibyte_sequence_at_the_end() {
        // 0xE0 alone starts a 3-byte sequence with no continuation bytes to
        // follow — `Utf8Error::error_len()` is `None` for this (a
        // "could still become valid with more input" case), and this
        // function's own doc comment says that's still counted as
        // undecodable for a point-in-time sample with no more input coming.
        let bytes = b"ok\xE0";
        assert_eq!(count_invalid_utf8_bytes(bytes), 1);
    }

    #[test]
    fn suggest_alternate_baud_never_returns_the_current_value() {
        for candidate in COMMON_BAUD_CANDIDATES {
            assert_ne!(suggest_alternate_baud(*candidate), *candidate);
        }
        // A baud not in the candidate list at all still gets a real
        // suggestion (the first candidate), not e.g. itself.
        assert_eq!(suggest_alternate_baud(1_234_567), 115_200);
    }

    fn line_with_raw(raw: &[u8]) -> crate::query::AssembledLine {
        crate::query::AssembledLine {
            raw: raw.to_vec(),
            text: String::from_utf8_lossy(raw).into_owned(),
            seq: 0,
            t_mono: 0.0,
            t_wall: "t0".to_string(),
        }
    }

    #[test]
    fn compute_decode_health_reports_no_suggestion_for_clean_text() {
        let lines = vec![
            line_with_raw(b"I (312) wifi: connected, ip 192.168.1.44"),
            line_with_raw(b"I (530) sensor: init ok, 4 channels"),
        ];
        let health = compute_decode_health(&lines, 115_200);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.suggested_baud, None);
    }

    #[test]
    fn compute_decode_health_suggests_a_different_baud_for_a_mostly_garbled_sample() {
        let garbled: Vec<u8> = (0..200).map(|i| 0x80u8.wrapping_add(i as u8)).collect();
        let lines = vec![line_with_raw(&garbled)];
        let health = compute_decode_health(&lines, 9600);
        assert!(health.checked_bytes >= DECODE_HEALTH_MIN_BYTES);
        assert!(
            health.undecodable_ratio >= DECODE_HEALTH_THRESHOLD,
            "{health:?}"
        );
        let suggested = health.suggested_baud.expect("expected a suggestion");
        assert_ne!(
            suggested, 9600,
            "must never suggest the already-wrong baud back"
        );
    }

    #[test]
    fn compute_decode_health_withholds_a_suggestion_below_the_minimum_sample_size() {
        // Two garbled bytes is a 100% ratio but far below
        // `DECODE_HEALTH_MIN_BYTES` — a real baud mismatch corrupts a lot
        // more than this, and a suggestion off a 2-byte sample would be
        // noise, not signal.
        let lines = vec![line_with_raw(&[0xFF, 0xFE])];
        let health = compute_decode_health(&lines, 115_200);
        assert_eq!(health.undecodable_ratio, 1.0);
        assert_eq!(
            health.suggested_baud, None,
            "sample too small to act on despite a 100% ratio"
        );
    }

    #[test]
    fn compute_decode_health_is_empty_for_no_lines() {
        let health = compute_decode_health(&[], 115_200);
        assert_eq!(health.checked_bytes, 0);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.suggested_baud, None);
    }

    // ---- T5.3 (issue #20): GET /config surfaces decode_health end to end ----

    #[tokio::test]
    async fn config_endpoint_surfaces_a_baud_suggestion_after_a_garbled_burst() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let garbled: Vec<u8> = (0..300).map(|i| 0x80u8.wrapping_add(i as u8)).collect();
        let mut with_newline = garbled.clone();
        with_newline.push(b'\n');
        recorder.append_rx(&with_newline).unwrap();

        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/config").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["decode_health"]["undecodable_ratio"].as_f64().unwrap() >= DECODE_HEALTH_THRESHOLD,
            "{body}"
        );
        assert!(body["decode_health"]["suggested_baud"].is_u64(), "{body}");
    }

    #[tokio::test]
    async fn config_endpoint_reports_no_suggestion_for_clean_output() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"boot ok\n").unwrap();

        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/config").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["decode_health"]["suggested_baud"].is_null(), "{body}");
    }

    // ---- T5.3 (issue #20): POST /config and /control_lines ----

    #[tokio::test]
    async fn post_config_merges_the_patch_and_appends_a_config_change_event() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let router = crate::web::router(shared.clone());
        let (status, body) = post(
            router.clone(),
            "/api/devices/dev-1/config",
            json!({ "baud": 74_880 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["config"]["baud"], 74_880);
        // Untouched fields keep their prior value — a patch, not a full
        // replace.
        assert_eq!(body["config"]["data_bits"], "eight");

        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let config_change = records.iter().find_map(|r| match r {
            wrap_proto::Record::Event { event, extra, .. } if event == "config_change" => {
                Some(extra.clone())
            }
            _ => None,
        });
        let extra = config_change.expect("expected a config_change event");
        assert_eq!(extra["new"]["baud"], 74_880);
        assert_eq!(extra["changed_by"], GUI_CHANGED_BY);
    }

    #[tokio::test]
    async fn post_config_404s_for_an_unknown_device() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, _) = post(
            crate::web::router(shared),
            "/api/devices/no-such-device/config",
            json!({ "baud": 9600 }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_config_rejects_an_invalid_field_with_400() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared),
            "/api/devices/dev-1/config",
            json!({ "data_bits": "not_a_real_variant" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn post_control_lines_applies_dtr_and_rts_and_is_reflected_in_the_log() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let router = crate::web::router(shared.clone());
        let (status, body) = post(
            router,
            "/api/devices/dev-1/control_lines",
            json!({ "dtr": true, "rts": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["dtr"], true);
        assert_eq!(body["rts"], false);

        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let lines: Vec<&str> = records
            .iter()
            .filter_map(|r| match r {
                wrap_proto::Record::Event { event, .. } if event == "control_line_change" => {
                    Some(event.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2, "expected one event each for dtr and rts");
    }

    // ---- T5.4 (issue #21): approvals list/approve/deny over HTTP ----

    async fn submit_pending_write(
        router: axum::Router,
        device_id: &str,
        bytes_text: &str,
    ) -> (StatusCode, serde_json::Value) {
        post(
            router,
            &format!("/api/devices/{device_id}/test/submit_write"),
            json!({
                "text": bytes_text,
                "requester_name": "claude-code",
                "requester_pid": 4242,
            }),
        )
        .await
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn approvals_list_shows_a_pending_write_and_approve_lets_it_through() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let router = crate::web::router(shared.clone());

        // "default_pending" text: not whitelisted, not a danger pattern —
        // built-in rules have no whitelist at all, so anything lands
        // `Pending`.
        let (status, submit_body) =
            submit_pending_write(router.clone(), "dev-1", "custom_cmd").await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        assert_eq!(status, StatusCode::OK, "{submit_body}");
        assert_eq!(submit_body["decision"], "pending");
        let approval_id = submit_body["id"].as_u64().expect("pending id");

        let (status, list_body) = get(router.clone(), "/api/approvals").await;
        assert_eq!(status, StatusCode::OK);
        let approvals = list_body["approvals"].as_array().expect("approvals array");
        let entry = approvals
            .iter()
            .find(|a| a["id"].as_u64() == Some(approval_id))
            .expect("submitted request must be listed as pending");
        assert_eq!(entry["requester_name"], "claude-code");
        assert_eq!(entry["requester_pid"], 4242);
        assert_eq!(entry["bytes_text"], "custom_cmd");
        assert!(entry["timeout_s"].as_f64().unwrap() > 0.0);

        let (status, approve_body) = post(
            router.clone(),
            &format!("/api/approvals/{approval_id}/approve"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{approve_body}");
        assert_eq!(approve_body["decision"], "approved");

        // No longer pending.
        let (_status, list_after) = get(router.clone(), "/api/approvals").await;
        assert!(list_after["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["id"].as_u64() != Some(approval_id)));

        // Approval resolves the write: a `tx` record lands in the same
        // device's stream — "指令執行 → 稽核有紀錄" (T5.4 acceptance
        // criterion 7). The completion task is spawned, not awaited by the
        // approve response itself, so poll rather than assume it already
        // ran.
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let saw_tx = wait_until(std::time::Duration::from_secs(2), || {
            recorder
                .read_since(0, usize::MAX)
                .unwrap()
                .records
                .iter()
                .any(|r| matches!(r, wrap_proto::Record::Tx { gate, .. } if gate.starts_with("approved_by:")))
        })
        .await;
        assert!(saw_tx, "expected a tx record for the approved write");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn denying_a_pending_write_never_produces_a_tx_record() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let router = crate::web::router(shared.clone());
        let (_status, submit_body) =
            submit_pending_write(router.clone(), "dev-1", "custom_cmd").await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        let approval_id = submit_body["id"].as_u64().expect("pending id");

        let (status, deny_body) = post(
            router.clone(),
            &format!("/api/approvals/{approval_id}/deny"),
            json!({ "reason": "operator says no" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deny_body}");
        assert_eq!(deny_body["decision"], "denied");

        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        // Give any (incorrect) completion task a moment to run, then assert
        // it never did.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let has_tx = recorder
            .read_since(0, usize::MAX)
            .unwrap()
            .records
            .iter()
            .any(|r| matches!(r, wrap_proto::Record::Tx { .. }));
        assert!(!has_tx, "a denied write must never produce a tx record");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn deciding_the_same_pending_write_twice_is_a_409_not_a_double_decision() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let router = crate::web::router(shared.clone());
        let (_status, submit_body) =
            submit_pending_write(router.clone(), "dev-1", "custom_cmd").await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        let approval_id = submit_body["id"].as_u64().expect("pending id");

        // First decision (simulating the CLI) wins.
        let (first_status, _) = post(
            router.clone(),
            &format!("/api/approvals/{approval_id}/deny"),
            json!({}),
        )
        .await;
        assert_eq!(first_status, StatusCode::OK);

        // A concurrent GUI click on the same id must not also succeed.
        let (second_status, second_body) = post(
            router.clone(),
            &format!("/api/approvals/{approval_id}/approve"),
            json!({}),
        )
        .await;
        assert_eq!(second_status, StatusCode::CONFLICT, "{second_body}");
        assert_eq!(second_body["error"]["code"], "already_decided");
    }

    #[tokio::test]
    async fn deciding_an_unknown_approval_id_is_a_409_not_a_panic() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared),
            "/api/approvals/999999/approve",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "already_decided");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_whitelisted_test_write_is_allowed_immediately_and_produces_a_tx_record() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::TEST_BACKEND_DEVICE_ENV, "dev-1");
        let (shared, _tmp, id) = shared_with_device("dev-1");
        // Built-in rules have five danger patterns and *no* whitelist, so
        // "erase" — a danger pattern — forces approval rather than
        // allowing immediately; assert the `force_pending` shape instead,
        // proving danger-priority survives this test-only path too.
        let router = crate::web::router(shared.clone());
        let (status, body) = submit_pending_write(router, "dev-1", "flash_erase 0x0").await;
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["decision"], "force_pending");
        assert_eq!(body["matched_rule"], "danger:erase");

        // Nothing written yet — still pending.
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let has_tx = recorder
            .read_since(0, usize::MAX)
            .unwrap()
            .records
            .iter()
            .any(|r| matches!(r, wrap_proto::Record::Tx { .. }));
        assert!(!has_tx);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_submit_write_404s_when_the_env_var_is_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(crate::TEST_BACKEND_DEVICE_ENV);
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, _) = submit_pending_write(crate::web::router(shared), "dev-1", "status").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Poll `condition` until it's `true` or `timeout` elapses — used for
    /// [`spawn_test_write_completion`]'s background task, which the approve
    /// HTTP response deliberately doesn't block on (see that function's doc
    /// comment). Not a fixed sleep: this returns as soon as the condition is
    /// actually observed, same "wait for the real event" discipline
    /// `webui/e2e/*.spec.ts` already follows (see `TASKS.md`'s test-
    /// discipline section, issue #39) — the 20ms poll interval and 2s
    /// ceiling are generous slack for CI scheduling noise around a single
    /// `tokio::spawn`, not a value this test's correctness depends on.
    async fn wait_until(timeout: std::time::Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
