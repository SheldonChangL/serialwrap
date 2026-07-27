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

use wrap_proto::{ErrorCode, HelloAck, HelloRequest, LineEnding, Permission, Request, WireError};

use crate::port::DeviceId;
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

/// Bytes to append after `text` for a given [`LineEnding`] — the write
/// path's own encoding step (`TASKS.md` T2.1, issue #8). See the wiki:
/// sending the wrong line ending to a firmware CLI is "a classic source of
/// 'the board ignored my command'", which is exactly why this is a
/// parameter on the request rather than a client-side convention. Kept
/// local to this module (not added to `wrap_proto::LineEnding` itself)
/// since only the write path needs the actual byte sequence; `data_b64`
/// writes never go through this at all — see the `Request::Write` handler.
fn line_ending_bytes(line_ending: LineEnding) -> &'static [u8] {
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
                    handle_request(bytes, client_id, changed_by, shared, tx).await;
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

async fn handle_request(
    raw: Vec<u8>,
    client_id: u64,
    changed_by: String,
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

    dispatch(id, request, client_id, &changed_by, &shared, &tx).await;
}

async fn dispatch(
    id: u64,
    request: Request,
    client_id: u64,
    changed_by: &str,
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
            let dev = DeviceId(device.clone());
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

            // The one write-gate rule this task implements (`TASKS.md`
            // T2.1, issue #8): a `human` client's `ReadWrite` permission
            // passes straight through — per the Security-model wiki's
            // policy table, "human is the authority the gate answers to;
            // gating them only lets a human turn the gate off". Every
            // other permission level (`agent`'s `ReadGatedWrite`, `tool`'s
            // `LeaseOnly`) still gets exactly the same structured
            // `permission_denied` this endpoint has always returned — the
            // real whitelist/danger/pending rule engine is T4.1's job, not
            // this one's.
            if permission != Permission::ReadWrite {
                send(
                    shared,
                    client_id,
                    tx,
                    err_reply(
                        Some(id),
                        WireError::new(
                            ErrorCode::PermissionDenied,
                            "write gate not implemented yet (see TASKS.md T4.1)",
                        ),
                    ),
                );
                return;
            }

            // `data_b64` (used for `--hex` and raw/binary payloads) is sent
            // exactly as given, no line ending appended: a caller who
            // spelled out exact bytes wants exactly those bytes on the
            // wire. `text` gets `line_ending`'s bytes appended server-side
            // — the wire contract the Client-protocol wiki documents.
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
            match shared.backend.write_bytes(&dev, &bytes) {
                Ok(()) => {
                    // Record the tx event *after* the bytes are actually
                    // out the port, carrying this write's identity
                    // (`changed_by` is already this connection's
                    // `"name:pid"` string — the same kernel-verified-pid
                    // convention `config_change`'s `changed_by` uses),
                    // type, and the `"human_rw"` gate label the
                    // Security-model wiki documents for a human's
                    // always-audited-never-gated write. A failure to
                    // append is logged, not returned as an error to the
                    // client: the write itself already succeeded, and
                    // reporting it as failed would invite a duplicate
                    // retry that writes the same bytes to the device
                    // twice.
                    if let Some(recorder) = shared.backend.recorder(&dev) {
                        if let Err(e) =
                            recorder.append_tx(&bytes, changed_by, client_type, "human_rw")
                        {
                            eprintln!(
                                "serialwrapd: protocol: failed to append tx record for {device}: {e}"
                            );
                        }
                    }
                    send(
                        shared,
                        client_id,
                        tx,
                        ok_reply(id, serde_json::json!({ "written": bytes.len() })),
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
        Request::LeaseAcquire { .. } => send(
            shared,
            client_id,
            tx,
            err_reply(
                Some(id),
                WireError::new(
                    ErrorCode::PermissionDenied,
                    "lease acquisition not implemented yet (see TASKS.md T2.2)",
                ),
            ),
        ),
        Request::LeaseRelease { .. } => send(
            shared,
            client_id,
            tx,
            err_reply(
                Some(id),
                WireError::new(
                    ErrorCode::PermissionDenied,
                    "lease release not implemented yet (see TASKS.md T2.2)",
                ),
            ),
        ),

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
    }
}
