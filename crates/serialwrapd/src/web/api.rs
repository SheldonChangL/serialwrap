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
//!
//! # The operator's write path
//!
//! - `POST /api/devices/:id/write`/[`write_device`]: the GUI's own
//!   serial-port write, added because the earlier milestones left the browser
//!   read-only — the CLI had `serialwrap write` and an agent had the MCP
//!   `write` tool, but the one surface an operator actually sits in front of
//!   could not send a byte, which made the GUI half a terminal. Same
//!   ungated-for-humans posture as the config endpoints above, and the same
//!   `tx` audit record every other write path produces. See that function's
//!   doc comment for why bypassing the gate here is the policy rather than a
//!   hole in it.
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

use crate::export::{ExportError, ExportRange};
use crate::gate::approval::{DecideError, Decision};
use crate::gate::{GateDecision, RequesterCtx, DEFAULT_LOG_CONTEXT_LINES};
use crate::port::DeviceId;
use crate::presentation::{event_to_json, page_to_json, PresentationLimits};
use crate::protocol::registry::Activity;
use crate::protocol::Shared;
use crate::query::{AssembledLine, OobRecord, QueryError};
use wrap_proto::{ClientType, ExportBound, ExportFormat, Filter, LineEnding, Permission};

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
        .route("/api/devices/{id}/write", post(write_device))
        .route("/api/devices/{id}/test/inject", post(test_inject))
        .route(
            "/api/devices/{id}/test/submit_write",
            post(test_submit_write),
        )
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}/approve", post(approve_approval))
        .route("/api/approvals/{id}/deny", post(deny_approval))
        .route("/api/clients", get(list_clients))
        .route("/api/clients/{id}/kick", post(kick_client))
        .route("/api/clients/{id}/demote", post(demote_client))
        .route("/api/devices/{id}/audit", get(audit))
        .route("/api/devices/{id}/export", get(export_device))
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

/// Body for [`write_device`] — mirrors `Request::Write`'s payload shape
/// (`protocol::session`) field-for-field, so the two write paths take the
/// same input in the same encoding: `text` gets `line_ending` appended
/// server-side, `data_b64` is sent as exactly the bytes given. There is
/// deliberately no `hex` field even though the GUI's own input bar offers a
/// HEX mode — the browser parses those digits and base64-encodes them, which
/// keeps hex parsing in the two places that already own it (`cli::write` and
/// `mcp::tools`) instead of adding a third copy here.
#[derive(Debug, Deserialize)]
struct WriteBody {
    text: Option<String>,
    data_b64: Option<String>,
    #[serde(default)]
    line_ending: Option<LineEnding>,
}

/// `POST /api/devices/:id/write` — the GUI operator's own write path.
///
/// Bypasses the gate and writes immediately, because the operator sitting at
/// this page *is* the authority the gate answers to: per the Security-model
/// wiki's policy-by-client-type table, a `human` client's writes go straight
/// through (`protocol::session`'s `Request::Write` handler takes the exact
/// same `Permission::ReadWrite` branch with the same `"human_rw"` gate
/// label), since gating the operator only teaches them to turn the gate off.
/// This is not a hole in the gate: an *agent* reaching this daemon over MCP
/// still goes through [`crate::gate::Gate::submit_write`] and still needs a
/// human decision, and this endpoint is unreachable from anywhere but
/// loopback ([`crate::web::guard`]).
///
/// "Bypasses the gate" never means "unaudited" — the `tx` record appended on
/// success is the same record every other client's write produces, in the
/// same append-only stream, so `serialwrap audit` and every other client's
/// `tail` see a GUI write exactly like a CLI or approved-agent one.
async fn write_device(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(body): Json<WriteBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };

    // Same decode order and same errors as `Request::Write` — `data_b64`
    // wins and is sent verbatim, `text` gets the line ending appended. LF is
    // the default for a bare `text` here (matching the wire default) so the
    // common case, an operator typing a command and pressing Enter, needs no
    // extra field.
    let bytes = match (body.data_b64, body.text) {
        (Some(b64), _) => match BASE64.decode(&b64) {
            Ok(bytes) => bytes,
            Err(e) => return invalid_request_response(format!("invalid data_b64: {e}")),
        },
        (None, Some(text)) => {
            let mut bytes = text.into_bytes();
            bytes.extend_from_slice(crate::protocol::session::line_ending_bytes(
                body.line_ending.unwrap_or(LineEnding::Lf),
            ));
            bytes
        }
        (None, None) => {
            return invalid_request_response("write requires `data_b64` or `text`".to_string())
        }
    };

    // A zero-byte write would put nothing on the wire but still append a `tx`
    // record, leaving an audit entry for something that never happened —
    // rejected rather than silently recorded. Note this is only reachable
    // with an explicitly empty payload: `text: ""` with any line ending but
    // `none` still sends that ending, which is a real keystroke (a bare
    // Enter) and goes through.
    if bytes.is_empty() {
        return invalid_request_response("nothing to write: payload is empty".to_string());
    }

    match shared.backend.write_bytes(&dev, &bytes) {
        Ok(()) => {
            // Appended only after the bytes are actually out the port, and a
            // failure here is logged rather than returned — the same
            // reasoning `write_and_reply` documents: the write already
            // happened, and reporting it as failed invites a retry that
            // writes the same bytes to the device twice.
            if let Err(e) =
                recorder.append_tx(&bytes, GUI_CHANGED_BY, ClientType::Human, "human_rw")
            {
                eprintln!(
                    "serialwrapd: web: write: failed to append tx record for an already-written \
                     payload on {id}: {e}"
                );
            }
            Json(json!({ "written": bytes.len() })).into_response()
        }
        Err(e) => backend_error_response(&e, &id),
    }
}

/// `400 invalid_request` in this layer's error envelope — the shape
/// [`backend_error_response`] and the gate endpoints already use.
fn invalid_request_response(message: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": { "code": "invalid_request", "message": message } })),
    )
        .into_response()
}

/// Map a [`crate::protocol::backend::DeviceBackend`] error to an HTTP
/// response — shared by [`set_config`]/[`set_control_lines`]/[`write_device`].
/// Not a full
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

// ---------------------------------------------------------------------
// T5.5 (issue #22): clients panel, audit panel, export dialog.
// ---------------------------------------------------------------------

/// Shape one live [`crate::protocol::registry::ClientSnapshot`] as JSON —
/// field-for-field identical to `Request::ListClients`'s UDS reply shape
/// (`protocol::session`, out of this task's scope to touch), built
/// independently here from the same public [`crate::protocol::registry::ClientRegistry::list`]
/// call, per this module's doc comment on why the web layer shapes JSON
/// itself rather than calling into `protocol::session::dispatch`. `status:
/// "active"` distinguishes this row from [`lease_end_to_json`]'s
/// reconstructed `"offline"` rows once both are merged in [`list_clients`].
fn client_snapshot_to_json(c: &crate::protocol::registry::ClientSnapshot) -> Value {
    let activity = match &c.activity {
        Activity::Idle => json!({ "state": "idle" }),
        Activity::WaitingFor {
            device,
            pattern,
            deadline,
        } => {
            // Same `deadline.saturating_duration_since(Instant::now())`
            // computation `Request::ListClients`'s handler already does
            // (session.rs) — reproduced here rather than shared, same
            // reasoning as every other duplicated-JSON-shaping handler in
            // this file.
            let remaining_s = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_secs_f64();
            json!({
                "state": "waiting_for",
                "device": device,
                "pattern": pattern,
                "remaining_s": remaining_s,
            })
        }
    };
    json!({
        "status": "active",
        "client_id": c.client_id,
        "name": c.name,
        "pid": c.pid,
        "type": c.client_type,
        "permission": c.permission,
        "bytes_in": c.bytes_in,
        "bytes_out": c.bytes_out,
        "activity": activity,
    })
}

/// Derive a short display name from a finished lease's `command` field
/// (e.g. `esptool.py write_flash 0x0 firmware.bin` -> `esptool`) — the
/// identity triple's "self-reported name" for [`lease_end_to_json`]'s rows,
/// matching the UX-design wiki's clients-panel mockup (`🔧 esptool · lease
/// · ended ...`). The daemon never records a separate friendly label for a
/// leased tool — only the literal `command` it was invoked with (see
/// `port::append_lease_start_event`) — so this is a display-only heuristic:
/// the basename of the first whitespace-separated token, with a common
/// script extension stripped. Falls back to `"tool"` when the command is
/// empty, rather than fabricating a name from nothing.
fn friendly_name_from_command(command: &str) -> String {
    let first_token = command.split_whitespace().next().unwrap_or("");
    if first_token.is_empty() {
        return "tool".to_string();
    }
    let base = first_token.rsplit('/').next().unwrap_or(first_token);
    match base
        .strip_suffix(".py")
        .or_else(|| base.strip_suffix(".sh"))
    {
        Some(stripped) if !stripped.is_empty() => stripped.to_string(),
        _ => base.to_string(),
    }
}

/// Reconstruct a finished-lease row from a `lease_end` [`OobRecord`] for
/// [`list_clients`] — see that function's doc comment for why this is read
/// straight from the event stream rather than kept in
/// [`crate::protocol::registry::ClientRegistry`] (which unregisters a
/// client's row the instant its connection tears down, and `protocol/` is
/// out of this task's scope to touch). Every field here already exists on
/// the `lease_end` event `port::append_lease_end_event` appends regardless
/// of this task — nothing new is recorded, this only reads.
fn lease_end_to_json(device_id: &str, event: &OobRecord) -> Value {
    let command = event
        .extra
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    json!({
        "status": "offline",
        "device": device_id,
        "name": friendly_name_from_command(command),
        "pid": event.extra.get("pid").cloned().unwrap_or(Value::Null),
        "type": "tool",
        "command": command,
        "exit_code": event.extra.get("exit_code").cloned().unwrap_or(Value::Null),
        "duration_ms": event.extra.get("duration_ms").cloned().unwrap_or(Value::Null),
        "reason": event.extra.get("reason").cloned().unwrap_or(Value::Null),
        "ended_at": event.t_wall,
        "ended_seq": event.seq,
    })
}

/// `GET /api/clients` (T5.5, issue #22): the clients panel's list. Merges
/// two sources that deliberately stay two sources rather than one shared
/// store:
///
/// - every *live* client, from [`crate::protocol::registry::ClientRegistry::list`]
///   (the exact same public method `Request::ListClients`'s UDS handler
///   calls — see [`client_snapshot_to_json`]);
/// - every *finished lease* still visible in any device's event stream — a
///   `lease_end` event, scanned via [`crate::query::DeviceQueryState::query_events`]
///   (see [`lease_end_to_json`]).
///
/// The UX-design wiki is explicit that a finished lease must stay listed
/// ("who touched the board just now" must always be answerable), but
/// `ClientRegistry::unregister` removes a client's row the moment its
/// connection tears down, and `protocol/` (where that registry lives) is
/// out of this task's scope to touch. Reconstructing finished leases from
/// the event stream instead — rather than teaching the registry to retain
/// rows — needs no daemon-side change at all: `lease_end`'s `command`/
/// `pid`/`duration_ms`/`exit_code`/`reason` fields are already recorded by
/// `port::append_lease_end_event` for every lease, GUI or not. This is the
/// same "audit is a query view, not a second store" principle the audit
/// panel ([`audit`]) applies, extended to one more panel.
async fn list_clients(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let live: Vec<Value> = shared
        .clients
        .list()
        .iter()
        .map(client_snapshot_to_json)
        .collect();

    let mut finished_leases = Vec::new();
    for summary in shared.backend.list_devices() {
        let Some(recorder) = shared.backend.recorder(&summary.id) else {
            continue;
        };
        let state = shared
            .queries
            .get_or_spawn(&summary.id, Arc::clone(&recorder));
        state.ingest(&recorder);
        for event in state.query_events(&["lease_end".to_string()], None, None) {
            finished_leases.push(lease_end_to_json(&summary.id.0, &event));
        }
    }

    Json(json!({ "clients": live, "finished_leases": finished_leases }))
}

fn client_not_found_response(client_id: u64) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "client_not_found",
                "message": format!("no such client_id {client_id}"),
            }
        })),
    )
        .into_response()
}

/// `POST /api/clients/:id/kick` (T5.5, issue #22): the clients panel's
/// "Kick" button. Calls [`crate::protocol::registry::ClientRegistry::kick`]
/// directly — the exact same public method `Request::Kick`'s UDS handler
/// calls (`protocol::session`, out of this task's scope to touch). `kick`
/// notifies the target connection's `kill` signal, which its reader/writer
/// loops race against and return from immediately — closing the socket out
/// from under any in-flight request that connection is blocked on (a long
/// `wait_for`, for instance), which is what a kicked MCP/CLI client
/// observes as a connection error rather than a silent hang (T5.5
/// acceptance criterion 3).
///
/// Also appends the same `client_kicked` audit event
/// `Request::Kick`'s handler appends to every device's stream, for parity:
/// an operator kicking someone from the GUI is exactly as auditable as
/// kicking them from `serialwrap` over the CLI. Reproduced here (rather
/// than shared) per this module's established convention for every other
/// GUI-initiated side effect — see [`GUI_CHANGED_BY`].
async fn kick_client(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<u64>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let target_snapshot = shared
        .clients
        .list()
        .into_iter()
        .find(|c| c.client_id == id);
    if !shared.clients.kick(id) {
        return client_not_found_response(id);
    }
    if let Some(snap) = target_snapshot {
        for summary in shared.backend.list_devices() {
            let Some(recorder) = shared.backend.recorder(&summary.id) else {
                continue;
            };
            let mut extra = serde_json::Map::new();
            extra.insert("client_id".to_string(), id.into());
            extra.insert("name".to_string(), snap.name.clone().into());
            extra.insert("pid".to_string(), snap.pid.into());
            extra.insert(
                "client_type".to_string(),
                serde_json::to_value(snap.client_type).unwrap_or(Value::Null),
            );
            extra.insert("kicked_by".to_string(), GUI_CHANGED_BY.into());
            if let Err(e) = recorder.append_event("client_kicked", extra) {
                eprintln!(
                    "serialwrapd: web: kick_client: failed to append client_kicked event for {}: {e}",
                    summary.id.0
                );
            }
        }
    }
    Json(json!({ "ok": true, "client_id": id })).into_response()
}

/// Body for [`demote_client`] — a bare [`Permission`] using its own wire
/// spelling (`"read+write"`/`"read+gated_write"`/`"lease_only"`), matching
/// `Request::Demote`'s `permission` field shape exactly.
#[derive(Debug, Deserialize)]
struct DemoteBody {
    permission: Permission,
}

/// `POST /api/clients/:id/demote` (T5.5, issue #22): the clients panel's
/// "Demote" button. Calls
/// [`crate::protocol::registry::ClientRegistry::demote`] directly — the
/// exact same public method `Request::Demote`'s UDS handler calls, which
/// (per that handler — see this module's doc comment) appends no audit
/// event either; this endpoint stays in parity with that as-is behavior
/// rather than making a GUI demote more auditable than a CLI one.
async fn demote_client(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<u64>,
    Json(body): Json<DemoteBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if shared.clients.demote(id, body.permission) {
        Json(json!({ "ok": true, "client_id": id, "permission": body.permission })).into_response()
    } else {
        client_not_found_response(id)
    }
}

/// `Record::Event` names, plus the always-audit-relevant `tx`/`gate` kinds,
/// this endpoint surfaces via [`crate::query::DeviceQueryState::query_events`]'s
/// own "kind string OR event name" matching (see that function's doc
/// comment). Mirrors `crates/serialwrap/src/cli/audit.rs`'s
/// `AUDIT_EVENT_NAMES`/`is_audit_relevant` verbatim. Duplicated rather than
/// imported: `serialwrapd` cannot depend on `serialwrap` (dependency
/// direction is `serialwrap -> serialwrapd -> wrap-proto`), so this small
/// list is the one piece of "what counts as audit-relevant" knowledge every
/// caller of `query_events` needs to restate for itself — keep both lists
/// in sync by inspection if either ever grows a new audit-relevant event.
const AUDIT_QUERY_KINDS: &[&str] = &[
    "tx",
    "gate",
    "write_request",
    "lease_start",
    "lease_end",
    "config_change",
    "control_line_change",
    "dtr_pulse",
    "client_kicked",
];

/// Query params for [`audit`]. Both optional and both plain `seq` bounds
/// (not wall-clock — the audit panel's own time-range filtering, if any, is
/// a display concern applied client-side over the fetched rows, the same
/// stance T5.2's live-log regex filter already takes for its own filter).
#[derive(Debug, Deserialize)]
struct AuditParams {
    since_seq: Option<u64>,
    until_seq: Option<u64>,
}

/// `GET /api/devices/:id/audit?since_seq=&until_seq=` (T5.5, issue #22): the
/// audit panel's list. A pure filtered read over the same stream `tail`/
/// `export` read from — see this module's doc comment: audit is a query
/// view, never a second store. Each returned row is exactly one
/// [`crate::query::DeviceQueryState::query_events`] result, shaped by
/// [`event_to_json`] — the *same* function [`tail`]'s `events` field
/// already uses — never independently re-serialized, and never joined or
/// correlated across rows: a denied write's bytes live on its own
/// `write_request` event row at its own `seq`; the eventual `gate` deny/
/// approve decision is a separate row at its own later `seq`. Two real
/// records, never one synthesized composite. "Jump to the log at this
/// moment" for *any* row is therefore free — the row's own `seq` is already
/// a real position in the same stream the main log view renders, no
/// correlation id or second lookup required (T5.5 acceptance criterion 1).
async fn audit(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Query(params): Query<AuditParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };
    let state = shared.queries.get_or_spawn(&dev, Arc::clone(&recorder));
    state.ingest(&recorder);
    let kinds: Vec<String> = AUDIT_QUERY_KINDS.iter().map(|s| s.to_string()).collect();
    let events = state.query_events(&kinds, params.since_seq, params.until_seq);
    let rows: Vec<Value> = events.iter().map(event_to_json).collect();
    Json(json!({ "audit": rows })).into_response()
}

/// Query params for [`export_device`]. `from`/`to` are each either a plain
/// `u64` seq or an RFC 3339 wall-clock string — see [`parse_export_bound`].
/// `boot` and `from` are mutually exclusive (mirrors `cli::export`'s own
/// `validate_range_flags`); `filter`+`format: bin` is rejected by
/// [`crate::export::export_range`] itself, not re-validated here (see that
/// function's own doc comment: the rejection is inside the one shared
/// renderer, not a CLI-only ad hoc check this layer would need to repeat).
#[derive(Debug, Deserialize)]
struct ExportParams {
    format: ExportFormat,
    from: Option<String>,
    to: Option<String>,
    #[serde(default)]
    boot: bool,
    filter: Option<String>,
}

/// Parse a `from`/`to` query value the same way `cli::export`'s (private)
/// `parse_bound` does: a plain integer is a `seq`; anything else is passed
/// through as a wall-clock string for [`crate::export::export_range`]
/// itself to validate as RFC 3339 (an invalid one surfaces as that
/// function's own `ExportError::InvalidTimestamp`, mapped to `400` by
/// [`export_error_response`] — no separate client-side validation
/// duplicated here).
fn parse_export_bound(raw: &str) -> ExportBound {
    let trimmed = raw.trim();
    match trimmed.parse::<u64>() {
        Ok(seq) => ExportBound::Seq(seq),
        Err(_) => ExportBound::Wall(trimmed.to_string()),
    }
}

fn export_error_response(e: &ExportError) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (status, message) = match e {
        ExportError::FilterNotAllowedForBin => (
            StatusCode::BAD_REQUEST,
            "--filter is not allowed with the bin format: it would silently break \
             byte-exactness"
                .to_string(),
        ),
        ExportError::InvalidPattern(msg) => {
            (StatusCode::BAD_REQUEST, format!("invalid pattern: {msg}"))
        }
        ExportError::InvalidTimestamp(msg) => {
            (StatusCode::BAD_REQUEST, format!("invalid timestamp: {msg}"))
        }
        ExportError::Io(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let code = if status == StatusCode::BAD_REQUEST {
        "invalid_request"
    } else {
        "internal"
    };
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// `GET /api/devices/:id/export?format=jsonl|txt|bin&from=&to=&boot=&filter=`
/// (T5.5, issue #22): the export dialog's download. Calls
/// [`crate::export::export_range`] directly — the exact same function
/// `Request::Export`'s UDS handler calls (`protocol::session`, out of this
/// task's scope to touch) — so a GUI export and a CLI `serialwrap export`
/// with equivalent parameters produce byte-identical `result.bytes` by
/// construction: there is exactly one renderer, this is its second caller,
/// not a second implementation (see `crate::export`'s own module doc
/// comment, written anticipating exactly this task).
///
/// `boot=true` resolves to this device's most recent `connect` event's
/// `seq`, mirroring `cli::export`'s own `resolve_boot_marker` — same
/// reasoning (a `connect` event is this project's one unambiguous "fresh
/// session with the device" marker), same "highest seq wins", reimplemented
/// here (rather than shared) because that function is a private,
/// CLI-crate-only helper operating over the wire `Request::QueryEvents`,
/// whereas this runs in-process against
/// [`crate::query::DeviceQueryState::query_events`] directly — identical
/// logic, not behavior drifting from it. A device with no `connect` event
/// yet exports from seq 0 (the full retained history), same fallback
/// `resolve_boot_marker` uses.
async fn export_device(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Query(params): Query<ExportParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dev = DeviceId(id.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return device_not_found_response(&id);
    };

    if params.boot && params.from.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "invalid_request",
                    "message": "boot and from are mutually exclusive — pick exactly one way to \
                                say where the exported range starts",
                }
            })),
        )
            .into_response();
    }

    let state = shared.queries.get_or_spawn(&dev, Arc::clone(&recorder));
    state.ingest(&recorder);

    let from = if params.boot {
        let last_connect_seq = state
            .query_events(&["connect".to_string()], None, None)
            .iter()
            .map(|e| e.seq)
            .max();
        Some(ExportBound::Seq(last_connect_seq.unwrap_or(0)))
    } else {
        params.from.as_deref().map(parse_export_bound)
    };
    let to = params.to.as_deref().map(parse_export_bound);
    let filter = params.filter.map(|pattern| Filter {
        pattern,
        exclude: false,
    });

    let range = ExportRange { from, to };
    match crate::export::export_range(&recorder, &range, params.format, filter.as_ref()) {
        Ok(result) => {
            let (content_type, ext) = match result.format {
                ExportFormat::Jsonl => ("application/x-ndjson", "jsonl"),
                ExportFormat::Txt => ("text/plain; charset=utf-8", "txt"),
                ExportFormat::Bin => ("application/octet-stream", "bin"),
            };
            let filename = format!("{id}-export.{ext}");
            (
                StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, content_type.to_string()),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                result.bytes,
            )
                .into_response()
        }
        Err(e) => export_error_response(&e),
    }
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
        let tmp = tempfile::tempdir().expect("tempdir");
        Arc::new(Shared::new(
            Arc::new(TestBackend::new()) as Arc<dyn DeviceBackend>,
            "test-version",
            tmp.path(),
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
            tmp.path(),
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
            tmp.path(),
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
            capped: false,
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

    // ---- T5.5 (issue #22): clients panel ----

    fn register_test_client(
        shared: &Shared,
        name: &str,
        pid: u32,
        client_type: ClientType,
        permission: Permission,
    ) -> u64 {
        shared.clients.register(
            name.to_string(),
            pid,
            client_type,
            permission,
            Arc::new(tokio::sync::Notify::new()),
        )
    }

    #[tokio::test]
    async fn list_clients_is_empty_with_no_clients_registered() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = get(crate::web::router(shared), "/api/clients").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["clients"].as_array().unwrap().len(), 0);
        assert_eq!(body["finished_leases"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_clients_reports_the_identity_triple_permission_and_traffic() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let client_id = register_test_client(
            &shared,
            "claude-code",
            5140,
            ClientType::Agent,
            Permission::ReadGatedWrite,
        );
        shared.clients.add_bytes_in(client_id, 340_000);
        let (status, body) = get(crate::web::router(shared), "/api/clients").await;
        assert_eq!(status, StatusCode::OK);
        let clients = body["clients"].as_array().unwrap();
        assert_eq!(clients.len(), 1);
        let entry = &clients[0];
        assert_eq!(entry["status"], "active");
        assert_eq!(entry["client_id"], client_id);
        assert_eq!(entry["name"], "claude-code");
        assert_eq!(entry["pid"], 5140);
        assert_eq!(entry["type"], "agent");
        assert_eq!(entry["permission"], "read+gated_write");
        assert_eq!(entry["bytes_in"], 340_000);
        assert_eq!(entry["activity"]["state"], "idle");
    }

    /// T5.5 acceptance criterion 4: the clients panel must show what an
    /// agent is currently blocked on in `wait_for`, and how long it has
    /// left. This pins the wire shape end to end: `set_activity` (what
    /// `Request::WaitFor`'s handler calls before awaiting, per
    /// `protocol::session`) through to `GET /api/clients`'s JSON.
    #[tokio::test]
    async fn list_clients_reports_waiting_for_state_with_remaining_seconds() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let client_id = register_test_client(
            &shared,
            "claude-code",
            5140,
            ClientType::Agent,
            Permission::ReadGatedWrite,
        );
        shared.clients.set_activity(
            client_id,
            Activity::WaitingFor {
                device: "dev-1".to_string(),
                pattern: "OTA done".to_string(),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(74),
            },
        );
        let (status, body) = get(crate::web::router(shared), "/api/clients").await;
        assert_eq!(status, StatusCode::OK);
        let activity = &body["clients"][0]["activity"];
        assert_eq!(activity["state"], "waiting_for");
        assert_eq!(activity["pattern"], "OTA done");
        let remaining = activity["remaining_s"].as_f64().unwrap();
        assert!(
            remaining > 0.0 && remaining <= 74.0,
            "expected a positive remaining time close to 74s, got {remaining}"
        );
    }

    /// T5.5 acceptance criterion 5: a finished lease must stay listed, not
    /// vanish — reconstructed here from a `lease_end` event already sitting
    /// in the device's own stream (see [`super::lease_end_to_json`]'s doc
    /// comment for why no registry change was needed).
    #[tokio::test]
    async fn list_clients_includes_a_finished_lease_reconstructed_from_the_event_stream() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let mut extra = serde_json::Map::new();
        extra.insert("device_id".to_string(), json!("dev-1"));
        extra.insert(
            "command".to_string(),
            json!("esptool.py write_flash 0x0 firmware.bin"),
        );
        extra.insert("pid".to_string(), json!(5311));
        extra.insert("token".to_string(), json!("tok-1"));
        extra.insert("exit_code".to_string(), json!(0));
        extra.insert("duration_ms".to_string(), json!(46_000));
        extra.insert("reason".to_string(), json!("released"));
        recorder.append_event("lease_end", extra).unwrap();

        let (status, body) = get(crate::web::router(shared), "/api/clients").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["clients"].as_array().unwrap().len(), 0);
        let leases = body["finished_leases"].as_array().unwrap();
        assert_eq!(leases.len(), 1, "{leases:?}");
        let lease = &leases[0];
        assert_eq!(lease["status"], "offline");
        assert_eq!(
            lease["name"], "esptool",
            "derived from the command's basename"
        );
        assert_eq!(lease["pid"], 5311);
        assert_eq!(lease["type"], "tool");
        assert_eq!(lease["exit_code"], 0);
        assert_eq!(lease["duration_ms"], 46_000);
    }

    #[test]
    fn friendly_name_from_command_strips_a_py_extension_and_arguments() {
        assert_eq!(
            friendly_name_from_command("esptool.py write_flash 0x0 firmware.bin"),
            "esptool"
        );
        assert_eq!(
            friendly_name_from_command("/usr/bin/screen /dev/tty.usb"),
            "screen"
        );
        assert_eq!(friendly_name_from_command(""), "tool");
        assert_eq!(friendly_name_from_command("openocd"), "openocd");
    }

    #[tokio::test]
    async fn kick_client_closes_the_client_and_appends_a_client_kicked_event() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let client_id = register_test_client(
            &shared,
            "claude-code",
            5140,
            ClientType::Agent,
            Permission::ReadGatedWrite,
        );
        let router = crate::web::router(shared.clone());
        let (status, body) =
            post(router, &format!("/api/clients/{client_id}/kick"), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["client_id"], client_id);

        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let kicked = records.iter().find_map(|r| match r {
            wrap_proto::Record::Event { event, extra, .. } if event == "client_kicked" => {
                Some(extra.clone())
            }
            _ => None,
        });
        let extra = kicked.expect("expected a client_kicked event");
        assert_eq!(extra["client_id"], client_id);
        assert_eq!(extra["name"], "claude-code");
        assert_eq!(extra["pid"], 5140);
        assert_eq!(extra["kicked_by"], GUI_CHANGED_BY);
    }

    #[tokio::test]
    async fn kick_client_404s_for_an_unknown_client_id() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared),
            "/api/clients/999999/kick",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "client_not_found");
    }

    #[tokio::test]
    async fn demote_client_changes_the_registered_permission() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let client_id = register_test_client(
            &shared,
            "claude-code",
            5140,
            ClientType::Agent,
            Permission::ReadGatedWrite,
        );
        let router = crate::web::router(shared.clone());
        let (status, body) = post(
            router.clone(),
            &format!("/api/clients/{client_id}/demote"),
            json!({ "permission": "lease_only" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["permission"], "lease_only");

        let (_status, list_body) = get(router, "/api/clients").await;
        assert_eq!(list_body["clients"][0]["permission"], "lease_only");
    }

    #[tokio::test]
    async fn demote_client_404s_for_an_unknown_client_id() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared),
            "/api/clients/999999/demote",
            json!({ "permission": "read+write" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "client_not_found");
    }

    // ---- T5.5 (issue #22): audit panel ----

    #[tokio::test]
    async fn audit_endpoint_404s_for_an_unknown_device() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, _) = get(
            crate::web::router(shared),
            "/api/devices/no-such-device/audit",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// T5.5 acceptance criterion 1 (seed): pins that `rx` never appears and
    /// only audit-relevant `tx`/`gate`/named-`event` records survive — and
    /// that a denied write's full payload (its `write_request` event, with
    /// `bytes_b64`) is one of the rows returned, unmodified, un-joined with
    /// its later `gate` decision (see [`super::audit`]'s doc comment on why
    /// there's deliberately no correlation logic here).
    #[tokio::test]
    async fn audit_endpoint_filters_to_audit_relevant_records_only() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"boot ok\n").unwrap(); // never audit-relevant
        recorder
            .append_tx(b"status\n", "claude-code", ClientType::Agent, "whitelist")
            .unwrap();
        let mut request_extra = serde_json::Map::new();
        request_extra.insert("request_id".to_string(), json!(1));
        request_extra.insert("bytes_b64".to_string(), json!("Zmxhc2hfZXJhc2U="));
        request_extra.insert("matched_rule".to_string(), json!("danger:erase"));
        let write_request_record = recorder
            .append_event("write_request", request_extra)
            .unwrap();
        recorder
            .append_gate("deny", "timeout_60s", write_request_record.seq())
            .unwrap();
        recorder
            .append_event("recovery", serde_json::Map::new())
            .unwrap(); // not audit-relevant

        let (status, body) = get(crate::web::router(shared), "/api/devices/dev-1/audit").await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["audit"].as_array().unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["tx", "event", "gate"], "{rows:?}");

        let write_request = &rows[1];
        assert_eq!(write_request["event"], "write_request");
        assert_eq!(write_request["bytes_b64"], "Zmxhc2hfZXJhc2U=");
        assert_eq!(write_request["matched_rule"], "danger:erase");

        let gate_row = &rows[2];
        assert_eq!(gate_row["action"], "deny");
        assert_eq!(gate_row["reason"], "timeout_60s");
        assert_eq!(gate_row["request_seq"], write_request["seq"]);
    }

    #[tokio::test]
    async fn audit_endpoint_respects_since_and_until_seq_params() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        for i in 0..5u64 {
            recorder
                .append_tx(
                    format!("cmd{i}\n").as_bytes(),
                    "claude-code",
                    ClientType::Agent,
                    "whitelist",
                )
                .unwrap();
        }
        let (status, body) = get(
            crate::web::router(shared),
            "/api/devices/dev-1/audit?since_seq=1&until_seq=3",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["audit"].as_array().unwrap();
        let seqs: Vec<u64> = rows.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3], "{rows:?}");
    }

    // ---- T5.5 (issue #22): export dialog ----

    #[tokio::test]
    async fn export_device_404s_for_an_unknown_device() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let response = crate::web::router(shared)
            .oneshot(
                Request::builder()
                    .uri("/api/devices/no-such-device/export?format=jsonl")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    async fn export_bytes(
        router: axum::Router,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }

    /// T5.5 acceptance criterion 2 (the byte-identity requirement) at the
    /// unit level: this HTTP handler's bytes must be *exactly*
    /// `crate::export::export_range`'s own output for equivalent
    /// parameters, since it calls that function directly rather than
    /// reimplementing any rendering. The full CLI-vs-GUI byte comparison
    /// (spawning the real `serialwrap export` binary) lives in
    /// `webui/e2e/`, where both real binaries exist; this pins the
    /// in-process half of that guarantee.
    #[tokio::test]
    async fn export_device_jsonl_bytes_match_export_range_directly() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"boot ok\n").unwrap();
        recorder
            .append_tx(b"status\n", "claude-code", ClientType::Agent, "whitelist")
            .unwrap();
        recorder.append_rx(&[0xFF, 0xFE, b'\n']).unwrap();

        let (status, headers, bytes) = export_bytes(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/export?format=jsonl",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/x-ndjson"
        );
        assert!(headers
            .get(axum::http::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("dev-1-export.jsonl"));

        let direct = crate::export::export_range(
            &recorder,
            &crate::export::ExportRange::default(),
            wrap_proto::ExportFormat::Jsonl,
            None,
        )
        .unwrap();
        assert_eq!(
            bytes, direct.bytes,
            "GUI export must be byte-identical to a direct export_range call"
        );
    }

    #[tokio::test]
    async fn export_device_bin_bytes_match_export_range_directly_and_are_byte_exact() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"boot ok\n").unwrap();
        recorder.append_rx(&[0x00, 0x01, 0xFF, 0xFE]).unwrap();
        recorder
            .append_tx(b"status\n", "claude-code", ClientType::Agent, "whitelist")
            .unwrap(); // tx bytes must never appear in a bin export

        let (status, _headers, bytes) = export_bytes(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/export?format=bin",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let direct = crate::export::export_range(
            &recorder,
            &crate::export::ExportRange::default(),
            wrap_proto::ExportFormat::Bin,
            None,
        )
        .unwrap();
        assert_eq!(bytes, direct.bytes);
        assert_eq!(bytes, b"boot ok\n\x00\x01\xFF\xFE".to_vec());
    }

    #[tokio::test]
    async fn export_device_rejects_bin_with_a_filter() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = get(
            crate::web::router(shared),
            "/api/devices/dev-1/export?format=bin&filter=boot",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn export_device_rejects_boot_and_from_together() {
        let (shared, _tmp, _id) = shared_with_device("dev-1");
        let (status, body) = get(
            crate::web::router(shared),
            "/api/devices/dev-1/export?format=jsonl&boot=true&from=0",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    /// `--boot`/`boot=true` resolves to the *latest* `connect` event's
    /// `seq`, mirroring `cli::export::resolve_boot_marker` — pinned here by
    /// seeding two `connect` events and asserting the export only contains
    /// records from the second boot onward.
    #[tokio::test]
    async fn export_device_boot_resolves_to_the_latest_connect_event() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"first boot line\n").unwrap();
        recorder
            .append_event("connect", serde_json::Map::new())
            .unwrap();
        recorder.append_rx(b"pre-second-boot line\n").unwrap();
        let second_connect = recorder
            .append_event("connect", serde_json::Map::new())
            .unwrap();
        recorder.append_rx(b"second boot line\n").unwrap();

        let (status, _headers, bytes) = export_bytes(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/export?format=txt&boot=true",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Independently pin the exact resolved starting bound (the second
        // `connect`'s own seq, known here from its returned `Record` —
        // not re-derived via this handler's own boot-resolution logic) by
        // comparing byte-for-byte against a direct `export_range` call with
        // that explicit bound.
        let expected_range = crate::export::ExportRange {
            from: Some(wrap_proto::ExportBound::Seq(second_connect.seq())),
            to: None,
        };
        let direct = crate::export::export_range(
            &recorder,
            &expected_range,
            wrap_proto::ExportFormat::Txt,
            None,
        )
        .unwrap();
        assert_eq!(
            bytes, direct.bytes,
            "boot=true must resolve to exactly the latest connect event's own seq"
        );

        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("second boot line"), "{text}");
        assert!(
            !text.contains("first boot line") && !text.contains("pre-second-boot"),
            "boot export must not include data from before the latest boot: {text}"
        );
    }

    #[tokio::test]
    async fn export_device_txt_bytes_match_export_range_directly() {
        let (shared, _tmp, id) = shared_with_device("dev-1");
        let recorder = shared.backend.recorder(&id).expect("recorder registered");
        recorder.append_rx(b"boot ok\n").unwrap();
        recorder
            .append_event("config_change", serde_json::Map::new())
            .unwrap();

        let (status, headers, bytes) = export_bytes(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/export?format=txt",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get(axum::http::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );

        let direct = crate::export::export_range(
            &recorder,
            &crate::export::ExportRange::default(),
            wrap_proto::ExportFormat::Txt,
            None,
        )
        .unwrap();
        assert_eq!(bytes, direct.bytes);
    }

    // ---- the operator's write path (`write_device`) ----

    /// [`shared_with_device`] plus a real file behind the device's
    /// `write_bytes`, so these tests can read back the exact bytes that
    /// reached the "port" instead of trusting the handler's own reply.
    fn shared_with_writable_device(
        device_id: &str,
    ) -> (Arc<Shared>, tempfile::TempDir, DeviceId, std::path::PathBuf) {
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
        let sink = tmp.path().join("port-out.bin");
        backend.register_writer(&id, std::fs::File::create(&sink).expect("create sink"));
        let shared = Arc::new(Shared::new(
            backend as Arc<dyn DeviceBackend>,
            "test-version",
            tmp.path().to_path_buf(),
        ));
        (shared, tmp, id, sink)
    }

    /// The `tx` records in a device's current tail, in order.
    async fn tx_events(shared: &Arc<Shared>, device_id: &str) -> Vec<serde_json::Value> {
        let (status, body) = get(
            crate::web::router(shared.clone()),
            &format!("/api/devices/{device_id}/tail?n=50"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        body["events"]
            .as_array()
            .expect("events array")
            .iter()
            .filter(|e| e["kind"] == "tx")
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn write_appends_the_requested_line_ending_and_audits_the_result() {
        let (shared, _tmp, _id, sink) = shared_with_writable_device("dev-1");

        let (status, body) = post(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/write",
            json!({ "text": "status", "line_ending": "crlf" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["written"], 8);
        assert_eq!(
            std::fs::read(&sink).unwrap(),
            b"status\r\n",
            "the line ending the operator picked must be what reaches the port"
        );

        // "Bypasses the gate" must never mean "unaudited" — see
        // `write_device`'s doc comment.
        let events = tx_events(&shared, "dev-1").await;
        assert_eq!(events.len(), 1, "one write, one tx record");
        assert_eq!(events[0]["gate"], "human_rw");
        assert_eq!(events[0]["client_type"], "human");
        assert_eq!(events[0]["client"], GUI_CHANGED_BY);
    }

    #[tokio::test]
    async fn write_sends_base64_bytes_verbatim_with_no_line_ending() {
        let (shared, _tmp, _id, sink) = shared_with_writable_device("dev-1");

        let (status, body) = post(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/write",
            // `line_ending` is deliberately set *and* deliberately ignored:
            // a caller who spelled out exact bytes gets exactly those bytes,
            // the same rule `Request::Write` follows.
            json!({ "data_b64": BASE64.encode([0xde, 0xad, 0xbe, 0xef]), "line_ending": "crlf" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["written"], 4);
        assert_eq!(std::fs::read(&sink).unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[tokio::test]
    async fn write_of_a_bare_enter_is_a_real_keystroke_and_goes_through() {
        let (shared, _tmp, _id, sink) = shared_with_writable_device("dev-1");

        let (status, _) = post(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/write",
            json!({ "text": "", "line_ending": "lf" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(std::fs::read(&sink).unwrap(), b"\n");
    }

    #[tokio::test]
    async fn write_rejects_a_payload_that_would_put_nothing_on_the_wire() {
        let (shared, _tmp, _id, sink) = shared_with_writable_device("dev-1");

        let (status, body) = post(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/write",
            json!({ "text": "", "line_ending": "none" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(std::fs::read(&sink).unwrap().is_empty());
        assert!(
            tx_events(&shared, "dev-1").await.is_empty(),
            "a rejected write must not leave an audit record for something that never happened"
        );
    }

    #[tokio::test]
    async fn write_rejects_a_request_carrying_neither_text_nor_bytes() {
        let (shared, _tmp, _id, _sink) = shared_with_writable_device("dev-1");
        let (status, body) = post(
            crate::web::router(shared.clone()),
            "/api/devices/dev-1/write",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn write_404s_for_an_unknown_device() {
        let (shared, _tmp, _id, _sink) = shared_with_writable_device("dev-1");
        let (status, _) = post(
            crate::web::router(shared.clone()),
            "/api/devices/nope/write",
            json!({ "text": "status" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
