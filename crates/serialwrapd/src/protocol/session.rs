//! Per-connection handling (`TASKS.md` T1.4): handshake, framed
//! newline-delimited JSON I/O, and request dispatch.
//!
//! # Why every request is its own spawned task
//!
//! The wiki is explicit: "Requests are independently ordered — a long-
//! running `wait_for` does not block other requests on the same
//! connection." [`reader_loop`] never awaits a request's outcome — it
//! reads one line, `tokio::spawn`s a task to handle it, and immediately
//! goes back to reading the next line. A `wait_for` that blocks for
//! seconds only blocks *its own* task; a `list_devices` request on the
//! same connection, read afterward, is handled by a separate task and
//! replies as soon as it's ready, out of order if it finishes first. Every
//! reply carries its request's `id` specifically so a client can match
//! out-of-order replies back up.
//!
//! # Why a kick takes effect immediately even mid-`wait_for`
//!
//! [`reader_loop`] and [`writer_loop`] both race their I/O against a
//! per-client `kill: Arc<Notify>` (see `registry::ClientRegistry::kick`).
//! Notifying it drops both stream halves within one tick, regardless of
//! whether some other spawned request task on this connection is still
//! blocked inside a long `wait_for` — that orphaned task's next `tx.send`
//! will simply fail silently (the channel closed underneath it) once the
//! writer loop exits, which is fine: nobody is reading its answer anymore.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use wrap_proto::{
    ClientType, ErrorCode, HelloAck, HelloRequest, LineEnding, Permission, Request, WireError,
};

use crate::gate::approval::Decision as ApprovalDecision;
use crate::gate::{dtr_pulse_gate_bytes, GateDecision, RequesterCtx, DEFAULT_LOG_CONTEXT_LINES};
use crate::port::{DeviceId, LeaseError};
use crate::query::{OobRecord, QueryError, QueryPage, WaitForOutcome};

use super::peer_cred;
use super::registry::Activity;
use super::server::Shared;

/// Hard cap on one line's length (either direction doesn't matter — this
/// guards *reading*). Generous relative to any real request/response this
/// protocol produces; exists purely so a client that never sends `\n`
/// can't grow the daemon's memory unbounded (`TASKS.md` T1.4 acceptance
/// criterion: "超長行...不 panic", never mind OOM). A line at or under this
/// bound is accepted regardless of content — even a deliberately huge but
/// well-formed request is only rejected by JSON/schema validation, not by
/// this cap.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on `wait_for`'s `timeout_s`, one day. Exists so a fuzzed or
/// malicious `timeout_s` (`NaN`, negative, or an overflowing literal like
/// `1e400`, which `serde_json` happily parses to `f64::INFINITY` rather
/// than rejecting) can never reach `Duration::from_secs_f64` — which
/// panics on any non-finite or negative input — see [`clamp_timeout`].
const MAX_TIMEOUT_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Turn a client-supplied `timeout_s` into a [`Duration`] that can never
/// panic `Duration::from_secs_f64`, regardless of what a fuzzed or
/// malicious client sends: non-finite (`NaN`/`inf`) becomes `0s`, negative
/// becomes `0s`, and anything absurdly large is capped at
/// [`MAX_TIMEOUT_SECS`].
fn clamp_timeout(timeout_s: f64) -> Duration {
    if !timeout_s.is_finite() {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(timeout_s.clamp(0.0, MAX_TIMEOUT_SECS))
}

enum ReadLineResult {
    Line(Vec<u8>),
    Eof,
    TooLong,
}

/// Read one newline-delimited line as raw bytes (no UTF-8 requirement —
/// that's JSON parsing's job, and it fails cleanly on invalid UTF-8),
/// bounded by [`MAX_LINE_BYTES`]. Uses `fill_buf`/`consume` rather than
/// `AsyncBufReadExt::read_until` specifically so the length cap is
/// enforced *during* accumulation, not only after an unbounded read
/// already completed.
async fn read_line_bounded(
    reader: &mut (impl AsyncBufReadExt + Unpin),
) -> io::Result<ReadLineResult> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                ReadLineResult::Eof
            } else {
                // Trailing bytes with no final newline before EOF: treat
                // as EOF too (nothing left to frame as a complete line).
                ReadLineResult::Eof
            });
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            let consumed = pos + 1;
            reader.consume(consumed);
            return Ok(if buf.len() > MAX_LINE_BYTES {
                ReadLineResult::TooLong
            } else {
                ReadLineResult::Line(buf)
            });
        }
        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
        if buf.len() > MAX_LINE_BYTES {
            return Ok(ReadLineResult::TooLong);
        }
    }
}

async fn write_line(w: &mut OwnedWriteHalf, line: &str) -> io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await
}

fn ok_reply(id: u64, mut body: serde_json::Value) -> String {
    let obj = body
        .as_object_mut()
        .expect("response bodies are always JSON objects");
    obj.insert("id".to_string(), id.into());
    obj.insert("ok".to_string(), true.into());
    body.to_string()
}

fn err_reply(id: Option<u64>, error: WireError) -> String {
    serde_json::json!({ "id": id, "ok": false, "error": error }).to_string()
}

/// Map a `DeviceBackend` I/O failure onto a structured wire error. Every
/// backend method uses `io::ErrorKind::NotFound` for "no such device" and
/// `NotConnected` for "known but not currently attached" (see
/// `port::PortConfigApi`'s docs) — this is the one place that convention
/// gets translated into `wrap_proto::ErrorCode`.
fn backend_error_to_wire(e: &io::Error, device: &str) -> WireError {
    match e.kind() {
        io::ErrorKind::NotFound => WireError::new(
            ErrorCode::DeviceNotFound,
            format!("no such device: {device}"),
        ),
        io::ErrorKind::NotConnected => WireError::new(
            ErrorCode::DeviceDisconnected,
            format!("{device} is not currently attached"),
        ),
        io::ErrorKind::InvalidInput => WireError::new(ErrorCode::InvalidRequest, e.to_string()),
        _ => WireError::new(ErrorCode::Internal, e.to_string()),
    }
}

/// Map a [`LeaseError`] onto a structured wire error (`TASKS.md` T2.2,
/// issue #9). `context` is the device name for [`LeaseError::UnknownDevice`]/
/// [`LeaseError::NotConnected`]/[`LeaseError::AlreadyLeased`] (acquire-side
/// errors, which always know the device), or the token for
/// [`LeaseError::UnknownToken`] (release-side — a release request never
/// names a device, only a token).
fn lease_error_to_wire(e: &LeaseError, context: &str) -> WireError {
    match e {
        LeaseError::UnknownDevice => WireError::new(
            ErrorCode::DeviceNotFound,
            format!("no such device: {context}"),
        ),
        LeaseError::NotConnected => WireError::new(
            ErrorCode::DeviceDisconnected,
            format!("{context} is not currently attached"),
        ),
        LeaseError::AlreadyLeased { holder } => WireError::new(
            ErrorCode::LeaseHeld,
            format!("{context} already has an active lease, held by {holder}"),
        )
        .with("holder", holder.clone()),
        LeaseError::UnknownToken => WireError::new(
            ErrorCode::InvalidRequest,
            format!("no active lease for token {context:?}"),
        ),
        LeaseError::Io(msg) => WireError::new(ErrorCode::Internal, msg.clone()),
    }
}

/// Bytes to append after `text` for a given [`LineEnding`] — the write
/// path's own encoding step (`TASKS.md` T2.1, issue #8). See the wiki:
/// sending the wrong line ending to a firmware CLI is "a classic source of
/// 'the board ignored my command'", which is exactly why this is a
/// parameter on the request rather than a client-side convention. Not added
/// to `wrap_proto::LineEnding` itself since only a write path needs the
/// actual byte sequence; `data_b64` writes never go through this at all —
/// see the `Request::Write` handler. Shared with [`crate::web::api`]'s
/// `write` endpoint, which appends the exact same bytes for the exact same
/// reason (a GUI operator picking `CRLF` from a dropdown is making the same
/// choice a CLI caller makes with `--line-ending`).
pub(crate) fn line_ending_bytes(line_ending: LineEnding) -> &'static [u8] {
    match line_ending {
        LineEnding::Lf => b"\n",
        LineEnding::Crlf => b"\r\n",
        LineEnding::Cr => b"\r",
        LineEnding::None => b"",
    }
}

fn query_error_to_wire(e: QueryError) -> WireError {
    match e {
        QueryError::DataAgedOut {
            oldest_available_seq,
        } => WireError::new(ErrorCode::DataAgedOut, "cursor points into evicted data")
            .with("oldest_available_seq", oldest_available_seq),
        QueryError::InvalidPattern(msg) => {
            WireError::new(ErrorCode::InvalidRequest, format!("invalid pattern: {msg}"))
        }
    }
}

/// Map a [`crate::export::ExportError`] onto a structured wire error
/// (`TASKS.md` T2.4, issue #11). Every variant here is a client-correctable
/// `invalid_request` — a range that partially or wholly overlaps
/// ring-evicted data is *not* one of these (see
/// [`crate::export::ExportResult::truncated_start`]); it's a normal `ok`
/// reply carrying a warning, never an error.
fn export_error_to_wire(e: &crate::export::ExportError) -> WireError {
    use crate::export::ExportError;
    match e {
        ExportError::FilterNotAllowedForBin => WireError::new(
            ErrorCode::InvalidRequest,
            "--filter is not allowed with the bin format: it would silently break byte-exactness",
        ),
        ExportError::InvalidPattern(msg) => {
            WireError::new(ErrorCode::InvalidRequest, format!("invalid pattern: {msg}"))
        }
        ExportError::InvalidTimestamp(msg) => WireError::new(
            ErrorCode::InvalidRequest,
            format!("invalid timestamp: {msg}"),
        ),
        ExportError::Io(e) => WireError::new(ErrorCode::Internal, e.to_string()),
    }
}

/// Wire shape for one assembled line: always `text`/`seq`/`t_mono`/`t_wall`,
/// plus `raw_b64` — the line's exact original bytes, base64-encoded — but
/// *only* when `text` alone can't already reconstruct them byte-for-byte.
///
/// That's exactly when `raw` isn't valid UTF-8: if it is, `text` (produced
/// via `String::from_utf8_lossy` over already-valid UTF-8, which is a
/// no-op) has the identical bytes as `raw`, so shipping a second
/// base64-encoded copy would be pure overhead for the overwhelmingly common
/// case (ordinary text log lines). Only genuinely binary/mixed-encoding
/// lines — rare relative to total line volume on a real device stream —
/// pay the ~33% base64 size cost, and only for that one line. See issue
/// #32 and the [Client protocol
/// wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
/// for the full rationale and the field's presence rule.
fn line_json(l: &crate::query::AssembledLine) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("text".to_string(), l.text.clone().into());
    obj.insert("seq".to_string(), l.seq.into());
    obj.insert("t_mono".to_string(), l.t_mono.into());
    obj.insert("t_wall".to_string(), l.t_wall.clone().into());
    if std::str::from_utf8(&l.raw).is_err() {
        obj.insert("raw_b64".to_string(), BASE64.encode(&l.raw).into());
    }
    // Issue #52: only present (and `true`) when the partial-buffer cap
    // force-completed this line rather than an actual terminator — additive
    // field, omitted entirely for every ordinarily-terminated line, so
    // existing consumers that don't know about it see no change at all.
    if l.capped {
        obj.insert("capped".to_string(), true.into());
    }
    serde_json::Value::Object(obj)
}

fn oob_json(e: &OobRecord) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("seq".to_string(), e.seq.into());
    obj.insert("t_mono".to_string(), e.t_mono.into());
    obj.insert("t_wall".to_string(), e.t_wall.clone().into());
    obj.insert(
        "kind".to_string(),
        serde_json::to_value(e.kind).unwrap_or(serde_json::Value::Null),
    );
    if let Some(name) = &e.name {
        obj.insert("event".to_string(), name.clone().into());
    }
    for (k, v) in &e.extra {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
    serde_json::Value::Object(obj)
}

fn page_json(page: &QueryPage) -> serde_json::Value {
    serde_json::json!({
        "lines": page.lines.iter().map(line_json).collect::<Vec<_>>(),
        "events": page.events.iter().map(oob_json).collect::<Vec<_>>(),
        "cursor": page.cursor,
    })
}

pub async fn handle_connection(stream: UnixStream, shared: Arc<Shared>) {
    let peer_pid = match peer_cred::peer_pid(&stream) {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("serialwrapd: protocol: failed to read peer credentials: {e}");
            return;
        }
    };

    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut write_half = write_half;

    let hello_bytes = match read_line_bounded(&mut reader).await {
        Ok(ReadLineResult::Line(bytes)) => bytes,
        Ok(ReadLineResult::Eof) => return,
        Ok(ReadLineResult::TooLong) => {
            let _ = write_line(
                &mut write_half,
                &err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, "line too long"),
                ),
            )
            .await;
            return;
        }
        Err(e) => {
            eprintln!("serialwrapd: protocol: read error before handshake: {e}");
            return;
        }
    };

    let hello: HelloRequest = match serde_json::from_slice::<HelloRequest>(&hello_bytes) {
        Ok(h) if h.op == "hello" => h,
        Ok(_) => {
            let _ = write_line(
                &mut write_half,
                &err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, "first message must be `hello`"),
                ),
            )
            .await;
            return;
        }
        Err(e) => {
            let _ = write_line(
                &mut write_half,
                &err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, format!("malformed hello: {e}")),
                ),
            )
            .await;
            return;
        }
    };

    let permission = Permission::for_client_type(hello.client_type);
    let kill = Arc::new(Notify::new());
    let client_id = shared.clients.register(
        hello.name.clone(),
        peer_pid,
        hello.client_type,
        permission,
        Arc::clone(&kill),
    );
    let changed_by = format!("{}:{}", hello.name, peer_pid);

    let ack = HelloAck {
        ok: true,
        permission,
        pid: peer_pid,
        server: shared.server_version.clone(),
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let _ = tx.send(serde_json::to_string(&ack).expect("HelloAck always serializes"));

    let writer_task = tokio::spawn(writer_loop(write_half, rx, Arc::clone(&kill)));

    reader_loop(
        reader,
        Arc::clone(&shared),
        client_id,
        changed_by,
        peer_pid,
        tx,
        Arc::clone(&kill),
    )
    .await;

    shared.clients.unregister(client_id);
    kill.notify_waiters();
    let _ = writer_task.await;
}

async fn writer_loop(
    mut write_half: OwnedWriteHalf,
    mut rx: UnboundedReceiver<String>,
    kill: Arc<Notify>,
) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(line) => {
                        if write_line(&mut write_half, &line).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
            _ = kill.notified() => return,
        }
    }
}

async fn reader_loop(
    mut reader: BufReader<OwnedReadHalf>,
    shared: Arc<Shared>,
    client_id: u64,
    changed_by: String,
    peer_pid: u32,
    tx: UnboundedSender<String>,
    kill: Arc<Notify>,
) {
    loop {
        let line = tokio::select! {
            r = read_line_bounded(&mut reader) => r,
            _ = kill.notified() => return,
        };
        match line {
            Ok(ReadLineResult::Line(bytes)) => {
                shared.clients.add_bytes_in(client_id, bytes.len() as u64);
                let shared = Arc::clone(&shared);
                let changed_by = changed_by.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    handle_request(bytes, client_id, changed_by, peer_pid, shared, tx).await;
                });
            }
            Ok(ReadLineResult::Eof) => return,
            Ok(ReadLineResult::TooLong) => {
                let _ = tx.send(err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, "line too long"),
                ));
                // Give the writer task a scheduling turn to drain that
                // reply before `handle_connection` notifies `kill` right
                // after this function returns — otherwise an unlucky
                // scheduling order could have the kill signal and the
                // queued reply become ready in the same poll, and
                // `writer_loop`'s `select!` is not biased toward the
                // channel over the kill notification.
                tokio::task::yield_now().await;
                return;
            }
            Err(e) => {
                eprintln!("serialwrapd: protocol: read error: {e}");
                return;
            }
        }
    }
}

fn send(shared: &Shared, client_id: u64, tx: &UnboundedSender<String>, line: String) {
    shared.clients.add_bytes_out(client_id, line.len() as u64);
    let _ = tx.send(line);
}

/// Everything [`write_and_reply`] needs about *this connection and this
/// request* — bundled into one struct (rather than eight separate
/// parameters) purely to stay under clippy's `too_many_arguments`; every
/// field here is identical across both of `Request::Write`'s call sites
/// into `write_and_reply` (the `human` bypass and an `agent` write the
/// gate ultimately allowed), only `bytes`/`gate_label` differ per call.
struct WriteReplyCtx<'a> {
    shared: &'a Shared,
    client_id: u64,
    tx: &'a UnboundedSender<String>,
    id: u64,
    dev: &'a DeviceId,
    device: &'a str,
    changed_by: &'a str,
    client_type: ClientType,
}

/// Actually send `bytes` out `dev`'s port, append the audit `tx` record,
/// and reply — the one place both the `human` bypass path and an `agent`
/// write the gate ultimately let through (whitelisted or human-approved)
/// converge, so this "write, then audit, then reply" sequence and its
/// error handling exist exactly once (`TASKS.md` T2.1/T4.1/T4.2). Mirrors
/// this endpoint's original human-only body verbatim, generalized only by
/// taking `gate_label` as a parameter instead of hardcoding `"human_rw"`:
/// `"human_rw"` for the human bypass, `"whitelist:<pattern>"` for an
/// agent's immediately-allowed write, or `"approved_by:<name:pid>"` for one
/// a human approved out of the pending queue (`TASKS.md` T4.2 acceptance
/// criterion 7).
fn write_and_reply(ctx: &WriteReplyCtx, bytes: &[u8], gate_label: &str) {
    match ctx.shared.backend.write_bytes(ctx.dev, bytes) {
        Ok(()) => {
            // Record the tx event *after* the bytes are actually out the
            // port — see this function's callers' doc comments for
            // `gate_label`'s three possible shapes. A failure to append is
            // logged, not returned as an error to the client: the write
            // itself already succeeded, and reporting it as failed would
            // invite a duplicate retry that writes the same bytes to the
            // device twice.
            if let Some(recorder) = ctx.shared.backend.recorder(ctx.dev) {
                if let Err(e) =
                    recorder.append_tx(bytes, ctx.changed_by, ctx.client_type, gate_label)
                {
                    eprintln!(
                        "serialwrapd: protocol: failed to append tx record for {}: {e}",
                        ctx.device
                    );
                }
            }
            send(
                ctx.shared,
                ctx.client_id,
                ctx.tx,
                ok_reply(ctx.id, serde_json::json!({ "written": bytes.len() })),
            );
        }
        Err(e) => send(
            ctx.shared,
            ctx.client_id,
            ctx.tx,
            err_reply(Some(ctx.id), backend_error_to_wire(&e, ctx.device)),
        ),
    }
}

async fn handle_request(
    raw: Vec<u8>,
    client_id: u64,
    changed_by: String,
    peer_pid: u32,
    shared: Arc<Shared>,
    tx: UnboundedSender<String>,
) {
    let value: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            send(
                &shared,
                client_id,
                &tx,
                err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, format!("malformed JSON: {e}")),
                ),
            );
            return;
        }
    };
    let id = match value.get("id").and_then(|v| v.as_u64()) {
        Some(id) => id,
        None => {
            send(
                &shared,
                client_id,
                &tx,
                err_reply(
                    None,
                    WireError::new(ErrorCode::InvalidRequest, "missing or non-numeric `id`"),
                ),
            );
            return;
        }
    };
    let request: Request = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            send(
                &shared,
                client_id,
                &tx,
                err_reply(
                    Some(id),
                    WireError::new(ErrorCode::InvalidRequest, e.to_string()),
                ),
            );
            return;
        }
    };

    dispatch(id, request, client_id, &changed_by, peer_pid, &shared, &tx).await;
}

async fn dispatch(
    id: u64,
    request: Request,
    client_id: u64,
    changed_by: &str,
    peer_pid: u32,
    shared: &Arc<Shared>,
    tx: &UnboundedSender<String>,
) {
    match request {
        Request::ListDevices => {
            let devices = shared.backend.list_devices();
            let arr: Vec<_> = devices
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "id": d.id.0,
                        "path": d.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                        "connected": d.connected,
                        "config": d.config,
                    })
                })
                .collect();
            send(
                shared,
                client_id,
                tx,
                ok_reply(id, serde_json::json!({ "devices": arr })),
            );
        }

        Request::GetConfig { device } => {
            let dev = DeviceId(device.clone());
            match shared.backend.get_config(&dev) {
                Ok(config) => {
                    let error_counts = shared
                        .backend
                        .error_counts(&dev)
                        .unwrap_or(crate::error_counts::ErrorCounts::Unavailable);
                    send(
                        shared,
                        client_id,
                        tx,
                        ok_reply(
                            id,
                            serde_json::json!({ "config": config, "error_counts": error_counts }),
                        ),
                    );
                }
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), backend_error_to_wire(&e, &device)),
                ),
            }
        }

        Request::SetConfig { device, config } => {
            let dev = DeviceId(device.clone());
            match shared.backend.set_config(&dev, &config, changed_by) {
                Ok(new_config) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(id, serde_json::json!({ "config": new_config })),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), backend_error_to_wire(&e, &device)),
                ),
            }
        }

        Request::SetControlLine { device, dtr, rts } => {
            let dev = DeviceId(device.clone());
            match shared.backend.set_control_line(&dev, dtr, rts, changed_by) {
                Ok(()) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(id, serde_json::json!({ "dtr": dtr, "rts": rts })),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), backend_error_to_wire(&e, &device)),
                ),
            }
        }

        Request::DtrPulse {
            device,
            duration_ms,
        } => {
            // `TASKS.md` T4.4 (issue #17): `dtr_pulse` physically resets
            // most boards — a hardware state change, not a display setting
            // like `set_config` — so, per the Security-model wiki's policy
            // table, an `agent` connection must go through the exact same
            // gate a `write` request does, never straight to the backend.
            // `human`/`tool` follow the identical permission split
            // `Request::Write` already established: a human bypasses the
            // gate (still fully audited via the `dtr_pulse` event
            // `DeviceBackend::dtr_pulse` itself appends); a `tool` has no
            // byte-level write path at all (`LeaseOnly` — see that
            // handler's own doc comment) and dtr_pulse is no exception.
            let dev = DeviceId(device.clone());

            let Some((client_type, permission)) = shared.clients.type_and_permission(client_id)
            else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::Internal, "client identity not found"),
                    ),
                );
                return;
            };

            if permission == Permission::LeaseOnly {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::PermissionDenied,
                            "tool clients have no dtr_pulse path — acquire a lease instead \
                             (see `serialwrap run --`)",
                        ),
                    ),
                );
                return;
            }

            if permission == Permission::ReadWrite {
                match shared
                    .backend
                    .dtr_pulse(&dev, Duration::from_millis(duration_ms), changed_by)
                {
                    Ok(()) => send(
                        shared,
                        client_id,
                        tx,
                        ok_reply(
                            id,
                            serde_json::json!({ "pulsed": true, "duration_ms": duration_ms }),
                        ),
                    ),
                    Err(e) => send(
                        shared,
                        client_id,
                        tx,
                        err_reply(Some(id), backend_error_to_wire(&e, &device)),
                    ),
                }
                return;
            }

            // Only `ReadGatedWrite` (an `agent`) reaches here.
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let Some((name, pid, _)) = shared.clients.identity(client_id) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::Internal, "client identity not found"),
                    ),
                );
                return;
            };
            let session_request_no = shared.clients.next_write_attempt(client_id);
            let log_context = shared
                .queries
                .get_or_spawn(&dev, Arc::clone(&recorder))
                .tail(DEFAULT_LOG_CONTEXT_LINES, None)
                .map(|page| page.lines.into_iter().map(|l| l.text).collect())
                .unwrap_or_else(|e| {
                    eprintln!(
                        "serialwrapd: protocol: failed to fetch log context for a pending \
                         dtr_pulse approval on {device}: {e:?}"
                    );
                    Vec::new()
                });

            let ctx = RequesterCtx {
                device: device.clone(),
                name,
                pid,
                client_type,
                session_request_no,
            };
            // Synthetic "bytes" so the same whitelist/danger rule-matching
            // `write` uses can also name `dtr_pulse` explicitly — see
            // `gate::dtr_pulse_gate_bytes`'s doc comment. This never
            // reaches `DeviceBackend::write_bytes`; only used to decide
            // allow/pending/force-pending.
            let gate_bytes = dtr_pulse_gate_bytes(duration_ms);
            let (decision, rx) = shared
                .gate
                .submit_write(&recorder, &gate_bytes, ctx, log_context);
            let matched_rule = match &decision {
                GateDecision::ForcePending { matched_rule, .. } => Some(matched_rule.clone()),
                GateDecision::Allow { .. } | GateDecision::Pending { .. } => None,
            };
            let resolution: Result<String, String> = match decision {
                GateDecision::Allow { reason } => Ok(reason),
                GateDecision::Pending { .. } | GateDecision::ForcePending { .. } => {
                    let rx = rx.expect(
                        "Gate::submit_write always returns a receiver for Pending/ForcePending",
                    );
                    match rx.await {
                        Ok(ApprovalDecision::Approved { approved_by }) => {
                            Ok(format!("approved_by:{approved_by}"))
                        }
                        Ok(ApprovalDecision::Denied { reason }) => Err(reason),
                        Err(_) => Err("approval channel closed unexpectedly".to_string()),
                    }
                }
            };

            match resolution {
                Ok(_gate_label) => match shared.backend.dtr_pulse(
                    &dev,
                    Duration::from_millis(duration_ms),
                    changed_by,
                ) {
                    Ok(()) => send(
                        shared,
                        client_id,
                        tx,
                        ok_reply(
                            id,
                            serde_json::json!({ "pulsed": true, "duration_ms": duration_ms }),
                        ),
                    ),
                    Err(e) => send(
                        shared,
                        client_id,
                        tx,
                        err_reply(Some(id), backend_error_to_wire(&e, &device)),
                    ),
                },
                Err(reason) => {
                    let mut err = WireError::new(
                        ErrorCode::WriteDenied,
                        format!("dtr_pulse denied: {reason}"),
                    )
                    .with("reason", reason);
                    if let Some(rule) = matched_rule {
                        err = err.with("matched_rule", rule);
                    }
                    send(shared, client_id, tx, err_reply(Some(id), err));
                }
            }
        }

        Request::Tail { device, n, filter } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let state = shared.queries.get_or_spawn(&dev, recorder);
            match state.tail(n, filter.as_ref()) {
                Ok(page) => send(shared, client_id, tx, ok_reply(id, page_json(&page))),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), query_error_to_wire(e)),
                ),
            }
        }

        Request::ReadSince {
            device,
            cursor,
            max_bytes,
            filter,
        } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let state = shared.queries.get_or_spawn(&dev, recorder);
            match state.read_since(cursor, max_bytes, filter.as_ref()) {
                Ok(page) => send(shared, client_id, tx, ok_reply(id, page_json(&page))),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), query_error_to_wire(e)),
                ),
            }
        }

        Request::WaitFor {
            device,
            pattern,
            timeout_s,
        } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let state = shared.queries.get_or_spawn(&dev, recorder);
            let timeout = clamp_timeout(timeout_s);
            shared.clients.set_activity(
                client_id,
                Activity::WaitingFor {
                    device: device.clone(),
                    pattern: pattern.clone(),
                    deadline: Instant::now() + timeout,
                },
            );
            let outcome = state.wait_for(&pattern, timeout).await;
            shared.clients.set_activity(client_id, Activity::Idle);
            match outcome {
                Ok(WaitForOutcome::Matched {
                    line,
                    raw,
                    seq,
                    elapsed_ms,
                }) => {
                    let mut body = serde_json::json!({
                        "result": "matched",
                        "line": line,
                        "seq": seq,
                        "elapsed_ms": elapsed_ms,
                    });
                    // Same raw_b64 presence rule as `line_json`: only when
                    // `line` (the lossy text) can't already reconstruct the
                    // real bytes byte-for-byte (issue #13 — `wait_for` had
                    // the same byte-fidelity gap issue #32 fixed for
                    // `tail`/`read_since`).
                    if std::str::from_utf8(&raw).is_err() {
                        body["raw_b64"] = BASE64.encode(&raw).into();
                    }
                    send(shared, client_id, tx, ok_reply(id, body));
                }
                Ok(WaitForOutcome::TimedOut {
                    elapsed_ms,
                    timeout_s,
                }) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({ "result": "timeout", "elapsed_ms": elapsed_ms, "timeout_s": timeout_s }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), query_error_to_wire(e)),
                ),
            }
        }

        Request::Subscribe {
            device,
            filter,
            since_cursor,
        } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let state = shared.queries.get_or_spawn(&dev, recorder);
            // `since_cursor` closes the tail-then-subscribe gap (issue
            // #32): resolve it to the exact same starting position
            // `read_since(since_cursor)` would use, so the first thing this
            // subscription ever drains is exactly what a `read_since` call
            // at this same instant would have returned — no gap, no
            // duplicate. Omitted, this falls back to the old
            // start-from-now snapshot.
            let mut idx = match since_cursor {
                Some(since_cursor) => match state.cursor_from_seq(since_cursor) {
                    Ok(idx) => idx,
                    Err(e) => {
                        send(
                            shared,
                            client_id,
                            tx,
                            err_reply(Some(id), query_error_to_wire(e)),
                        );
                        return;
                    }
                },
                None => (state.line_count(), state.event_count()),
            };
            loop {
                let notified = state.notified();
                match state.drain_since(idx, filter.as_ref()) {
                    Ok(drained) => {
                        idx = drained.next;
                        if !drained.lines.is_empty() || !drained.events.is_empty() {
                            let push = serde_json::json!({
                                "id": id,
                                "ok": true,
                                "push": true,
                                "lines": drained.lines.iter().map(line_json).collect::<Vec<_>>(),
                                "events": drained.events.iter().map(oob_json).collect::<Vec<_>>(),
                            })
                            .to_string();
                            shared.clients.add_bytes_out(client_id, push.len() as u64);
                            if tx.send(push).is_err() {
                                return;
                            }
                            continue;
                        }
                    }
                    Err(e) => {
                        send(
                            shared,
                            client_id,
                            tx,
                            err_reply(Some(id), query_error_to_wire(e)),
                        );
                        return;
                    }
                }
                // Bound the wait so a subscriber whose connection died
                // without any further device traffic still notices
                // (`tx.is_closed()`) instead of parking forever.
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if tx.is_closed() {
                            return;
                        }
                    }
                }
            }
        }

        Request::QueryEvents {
            device,
            kinds,
            since_seq,
            until_seq,
        } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let state = shared.queries.get_or_spawn(&dev, recorder);
            let events = state.query_events(&kinds, since_seq, until_seq);
            send(
                shared,
                client_id,
                tx,
                ok_reply(
                    id,
                    serde_json::json!({ "events": events.iter().map(oob_json).collect::<Vec<_>>() }),
                ),
            );
        }

        Request::Write {
            device,
            data_b64,
            text,
            line_ending,
        } => {
            // Who's asking, and what they're currently allowed to do —
            // looked up fresh (never cached from the handshake) so a
            // `demote` mid-connection takes effect on the very next write
            // (`TASKS.md` T2.3 acceptance criterion 10).
            let Some((client_type, permission)) = shared.clients.type_and_permission(client_id)
            else {
                // Unreachable in practice: `client_id` is this same
                // connection's own registry row, registered before
                // `dispatch` is ever reached and only removed after
                // `reader_loop` returns. Handled explicitly rather than
                // unwrapping anyway, matching this module's no-panic-on-
                // any-input stance.
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::Internal, "client identity not found"),
                    ),
                );
                return;
            };

            // `tool`'s `LeaseOnly` permission has no byte-level write path
            // at all, gate or no gate (`TASKS.md` T4.1's client-type
            // policy: "tool 只能走 lease") — checked before ever decoding
            // the payload, same fail-fast-on-permission order this handler
            // has always used.
            if permission == Permission::LeaseOnly {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::PermissionDenied,
                            "tool clients have no byte-level write path — acquire a lease \
                             instead (see `serialwrap run --`)",
                        ),
                    ),
                );
                return;
            }

            // `data_b64` (used for `--hex` and raw/binary payloads) is sent
            // exactly as given, no line ending appended: a caller who
            // spelled out exact bytes wants exactly those bytes on the
            // wire. `text` gets `line_ending`'s bytes appended server-side
            // — the wire contract the Client-protocol wiki documents. This
            // decode step runs identically for every remaining permission
            // level and strictly *before* the gate ever sees anything
            // (`TASKS.md` T4.1 acceptance criterion 3, the hex-bypass
            // guard): `crate::gate::rules::RuleSet::evaluate` only ever
            // matches against these already-decoded bytes, never the wire
            // encoding a client chose — see that module's docs for why
            // that's what closes a `--hex`-encoded danger command sailing
            // past a rule that would catch its plain-text equivalent.
            let bytes = match (data_b64, text) {
                (Some(b64), _) => match BASE64.decode(&b64) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        send(
                            shared,
                            client_id,
                            tx,
                            err_reply(
                                Some(id),
                                WireError::new(
                                    ErrorCode::InvalidRequest,
                                    format!("invalid data_b64: {e}"),
                                ),
                            ),
                        );
                        return;
                    }
                },
                (None, Some(text)) => {
                    let mut bytes = text.into_bytes();
                    bytes.extend_from_slice(line_ending_bytes(line_ending));
                    bytes
                }
                (None, None) => {
                    send(
                        shared,
                        client_id,
                        tx,
                        err_reply(
                            Some(id),
                            WireError::new(
                                ErrorCode::InvalidRequest,
                                "write requires `data_b64` or `text`",
                            ),
                        ),
                    );
                    return;
                }
            };

            let dev = DeviceId(device.clone());

            if permission == Permission::ReadWrite {
                // Human bypass (`TASKS.md` T2.1, issue #8): per the
                // Security-model wiki's policy table, "human is the
                // authority the gate answers to; gating them only lets a
                // human turn the gate off" — always audited (the
                // `"human_rw"` gate label), never blocked, never routed
                // through the gate at all.
                write_and_reply(
                    &WriteReplyCtx {
                        shared,
                        client_id,
                        tx,
                        id,
                        dev: &dev,
                        device: &device,
                        changed_by,
                        client_type,
                    },
                    &bytes,
                    "human_rw",
                );
                return;
            }

            // Only `ReadGatedWrite` (an `agent`) can reach here: `LeaseOnly`
            // returned above, `ReadWrite` just returned too.
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let Some((name, pid, _)) = shared.clients.identity(client_id) else {
                // Same unreachable-in-practice defensive branch as the
                // `type_and_permission` lookup above.
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::Internal, "client identity not found"),
                    ),
                );
                return;
            };
            let session_request_no = shared.clients.next_write_attempt(client_id);
            // Preceding log context for the approval payload (`TASKS.md`
            // T4.2, issue #15) — fetched via the same per-device query
            // state `Tail`/`ReadSince`/`WaitFor` already share, *before*
            // the gate runs, so it's genuinely "the log right before this
            // request" and never includes this write's own eventual `tx`
            // record. A query failure here (e.g. a transient
            // `data_aged_out`, vanishingly unlikely against a fresh
            // `tail`) degrades to an empty context rather than failing the
            // whole write — this is supplementary operator context, not
            // something the gate's correctness depends on.
            let log_context = shared
                .queries
                .get_or_spawn(&dev, Arc::clone(&recorder))
                .tail(DEFAULT_LOG_CONTEXT_LINES, None)
                .map(|page| page.lines.into_iter().map(|l| l.text).collect())
                .unwrap_or_else(|e| {
                    eprintln!(
                        "serialwrapd: protocol: failed to fetch log context for a pending \
                         approval on {device}: {e:?}"
                    );
                    Vec::new()
                });

            let ctx = RequesterCtx {
                device: device.clone(),
                name,
                pid,
                client_type,
                session_request_no,
            };
            let (decision, rx) = shared
                .gate
                .submit_write(&recorder, &bytes, ctx, log_context);
            let matched_rule = match &decision {
                GateDecision::ForcePending { matched_rule, .. } => Some(matched_rule.clone()),
                GateDecision::Allow { .. } | GateDecision::Pending { .. } => None,
            };
            let resolution: Result<String, String> = match decision {
                GateDecision::Allow { reason } => Ok(reason),
                GateDecision::Pending { .. } | GateDecision::ForcePending { .. } => {
                    let rx = rx.expect(
                        "Gate::submit_write always returns a receiver for Pending/ForcePending",
                    );
                    match rx.await {
                        Ok(ApprovalDecision::Approved { approved_by }) => {
                            Ok(format!("approved_by:{approved_by}"))
                        }
                        Ok(ApprovalDecision::Denied { reason }) => Err(reason),
                        // The sender side is only ever dropped by
                        // `PendingQueue::decide` sending first — this arm
                        // is unreachable in practice, handled defensively
                        // rather than panicking a whole connection over it.
                        Err(_) => Err("approval channel closed unexpectedly".to_string()),
                    }
                }
            };

            match resolution {
                Ok(gate_label) => write_and_reply(
                    &WriteReplyCtx {
                        shared,
                        client_id,
                        tx,
                        id,
                        dev: &dev,
                        device: &device,
                        changed_by,
                        client_type,
                    },
                    &bytes,
                    &gate_label,
                ),
                Err(reason) => {
                    // Structured, never silent (`TASKS.md` T4.2 acceptance
                    // criterion 6): `reason` is a distinct field a caller
                    // can branch on programmatically (e.g. `"timeout_60s"`,
                    // or an operator's own denial text), separate from
                    // `message`'s human-readable sentence. `matched_rule` is
                    // only present when a danger rule is what forced this
                    // to approval in the first place.
                    let mut err =
                        WireError::new(ErrorCode::WriteDenied, format!("write denied: {reason}"))
                            .with("reason", reason);
                    if let Some(rule) = matched_rule {
                        err = err.with("matched_rule", rule);
                    }
                    send(shared, client_id, tx, err_reply(Some(id), err));
                }
            }
        }
        Request::LeaseAcquire {
            device,
            command,
            timeout_s,
        } => {
            // `pid` is this connection's own kernel-verified peer pid (the
            // same value `changed_by`'s trailing `:pid` component already
            // carries) — the process that *asked* for the lease, not
            // necessarily the pid of whatever it execs afterward, which the
            // daemon has no way to know at acquire time (see
            // `port::append_lease_start_event`'s docs). Permission is
            // deliberately unchecked here: every `ClientType`, including
            // `tool` (whose only permission, `LeaseOnly`, exists
            // specifically to act through a lease), is allowed to request
            // one — narrowing that is T4.x's rule engine's job, not this
            // task's (`TASKS.md` T2.2).
            let dev = DeviceId(device.clone());
            match shared
                .backend
                .acquire_lease(&dev, &command, peer_pid, timeout_s)
            {
                Ok(acquired) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({
                            "token": acquired.token,
                            "path": acquired.path.to_string_lossy(),
                        }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), lease_error_to_wire(&e, &device)),
                ),
            }
        }
        Request::LeaseRelease { token, exit_code } => {
            match shared.backend.release_lease(&token, exit_code) {
                Ok(released) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({ "duration_ms": released.duration_ms }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), lease_error_to_wire(&e, &token)),
                ),
            }
        }

        Request::ListClients => {
            let clients: Vec<_> = shared
                .clients
                .list()
                .iter()
                .map(|c| {
                    let activity = match &c.activity {
                        Activity::Idle => serde_json::json!({ "state": "idle" }),
                        Activity::WaitingFor {
                            device,
                            pattern,
                            deadline,
                        } => {
                            let remaining = deadline
                                .saturating_duration_since(Instant::now())
                                .as_secs_f64();
                            serde_json::json!({
                                "state": "waiting_for",
                                "device": device,
                                "pattern": pattern,
                                "remaining_s": remaining,
                            })
                        }
                    };
                    serde_json::json!({
                        "client_id": c.client_id,
                        "name": c.name,
                        "pid": c.pid,
                        "type": c.client_type,
                        "permission": c.permission,
                        "bytes_in": c.bytes_in,
                        "bytes_out": c.bytes_out,
                        "activity": activity,
                    })
                })
                .collect();
            send(
                shared,
                client_id,
                tx,
                ok_reply(id, serde_json::json!({ "clients": clients })),
            );
        }

        Request::Kick { client_id: target } => {
            // Snapshot the target's identity *before* kicking — `kick`
            // only notifies the connection's `kill` signal, it doesn't
            // remove the registry row (that happens once the connection
            // actually unwinds), but grabbing this first avoids any race
            // with that teardown.
            let target_snapshot = shared
                .clients
                .list()
                .into_iter()
                .find(|c| c.client_id == target);
            if shared.clients.kick(target) {
                // `TASKS.md` T2.3 acceptance criterion 9: "kick 後...記事
                // 件". A kick is about a *client*, not any one device —
                // this daemon's only event stream is per-device (see
                // `recorder.rs`), so the client_kicked event is broadcast
                // to every device this backend currently knows about
                // (harmless extra visibility on an operator-initiated,
                // infrequent action; never a gap for whichever device an
                // operator is actually watching).
                if let Some(snap) = &target_snapshot {
                    for summary in shared.backend.list_devices() {
                        let Some(recorder) = shared.backend.recorder(&summary.id) else {
                            continue;
                        };
                        let mut extra = serde_json::Map::new();
                        extra.insert("client_id".to_string(), target.into());
                        extra.insert("name".to_string(), snap.name.clone().into());
                        extra.insert("pid".to_string(), snap.pid.into());
                        extra.insert(
                            "client_type".to_string(),
                            serde_json::to_value(snap.client_type)
                                .unwrap_or(serde_json::Value::Null),
                        );
                        extra.insert("kicked_by".to_string(), changed_by.into());
                        if let Err(e) = recorder.append_event("client_kicked", extra) {
                            eprintln!(
                                "serialwrapd: protocol: failed to append client_kicked event for \
                                 {}: {e}",
                                summary.id.0
                            );
                        }
                    }
                }
                send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(id, serde_json::json!({ "kicked": target })),
                );
            } else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::InvalidRequest,
                            format!("no such client_id {target}"),
                        ),
                    ),
                );
            }
        }

        Request::Export {
            device,
            format,
            from,
            to,
            filter,
        } => {
            let dev = DeviceId(device.clone());
            let Some(recorder) = shared.backend.recorder(&dev) else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::DeviceNotFound,
                            format!("no such device: {device}"),
                        ),
                    ),
                );
                return;
            };
            let range = crate::export::ExportRange { from, to };
            match crate::export::export_range(&recorder, &range, format, filter.as_ref()) {
                Ok(result) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({
                            "format": result.format,
                            "data_b64": BASE64.encode(&result.bytes),
                            "record_count": result.record_count,
                            "last_seq": result.last_seq,
                            "truncated_start": result.truncated_start,
                        }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(Some(id), export_error_to_wire(&e)),
                ),
            }
        }

        Request::Demote {
            client_id: target,
            permission,
        } => {
            if shared.clients.demote(target, permission) {
                send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({ "client_id": target, "permission": permission }),
                    ),
                );
            } else {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::InvalidRequest,
                            format!("no such client_id {target}"),
                        ),
                    ),
                );
            }
        }

        Request::ApprovalsList => {
            // Same op `serialwrap approvals` (T4.2, issue #15) and the
            // future GUI approval card (T5.4) both call — see
            // `crate::gate`'s module docs.
            let approvals = shared.gate.list();
            send(
                shared,
                client_id,
                tx,
                ok_reply(id, serde_json::json!({ "approvals": approvals })),
            );
        }

        Request::ApprovalApprove { approval_id } => {
            // The approving identity is always this connection's own
            // kernel-verified `name:pid` (`changed_by`), never a
            // client-supplied field — same convention every other
            // `changed_by` use in this file already follows.
            match shared.gate.decide(
                approval_id,
                ApprovalDecision::Approved {
                    approved_by: changed_by.to_string(),
                },
            ) {
                // `"approval_id"`, not `"id"`: `ok_reply` always inserts
                // its own top-level `"id"` (the wire reply-correlation
                // id, echoing the request's own) into this body — a key
                // named `"id"` here would just get silently overwritten by
                // that insert, same collision `Request::ApprovalApprove`
                // itself is named `approval_id` to avoid (see that
                // variant's doc comment). Same reasoning every other
                // reply body in this file already follows (`Kick`'s
                // `"kicked"`, `Demote`'s `"client_id"` — never `"id"`).
                Ok(()) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({ "approval_id": approval_id, "decision": "approved" }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::InvalidRequest, e.to_string()),
                    ),
                ),
            }
        }

        Request::ApprovalDeny {
            approval_id,
            reason,
        } => {
            let reason = reason.unwrap_or_else(|| format!("denied_by_operator:{changed_by}"));
            match shared
                .gate
                .decide(approval_id, ApprovalDecision::Denied { reason })
            {
                Ok(()) => send(
                    shared,
                    client_id,
                    tx,
                    ok_reply(
                        id,
                        serde_json::json!({ "approval_id": approval_id, "decision": "denied" }),
                    ),
                ),
                Err(e) => send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(ErrorCode::InvalidRequest, e.to_string()),
                    ),
                ),
            }
        }
    }
}
