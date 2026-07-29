//! `WS /api/stream` (`TASKS.md` T5.1, issue #18; extended by T5.2, issue
//! #19). Every connection gets a `hello` on connect, then a `heartbeat`
//! every [`HEARTBEAT_INTERVAL`] until it disconnects — the frontend
//! (`webui/src/lib/connection.ts`) never calls a bare WS `open` event
//! "connected", only an actual message does, and treats a stall in these
//! heartbeats as a disconnect too.
//!
//! # T5.2: `?device=`/`?since_cursor=` — live log pushes
//!
//! `/api/stream?device=<id>` additionally subscribes that connection to
//! `id`'s live record stream: every batch [`crate::query::DeviceQueryState::ingest`]
//! adds, this handler drains via [`crate::query::DeviceQueryState::drain_since`],
//! runs it through [`crate::presentation::present`] (the *same*
//! dedup-folding/binary-summarization layer T3.2 built and the initial
//! `GET /api/devices/:id/tail` page already uses — see `api.rs`'s module
//! doc comment), and pushes `{"type":"push","lines":[...],"events":[...]}`.
//! `since_cursor` (typically the `cursor` a prior `tail` call returned)
//! resolves the starting position exactly the way the UDS `Subscribe`
//! handler's own `since_cursor` does (`protocol::session`'s module docs
//! call this "closes the tail-then-subscribe gap": no gap, no duplicate).
//! Omitting `device` preserves T5.1's original hello/heartbeat-only
//! behavior byte-for-byte — `infrastructure.spec.ts` never passes it.
//!
//! Deliberately *not* implemented here: a client-sent `filter` narrowing
//! what gets pushed. The live log view's regex filter is a display-only
//! concern applied client-side over the full, unfiltered push stream (see
//! `webui/src/lib/liveLog.ts`) — out-of-band events must never disappear
//! just because a filter doesn't match them (same principle
//! `crate::query`'s own `Filter` semantics already establish for the UDS
//! `tail`/`read_since`), and re-subscribing on every filter keystroke
//! would either lose already-buffered scrollback or duplicate it.
//! Bidirectional messages from the client are otherwise ignored, same as
//! T5.1.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;

use crate::port::DeviceId;
use crate::presentation::{page_to_json, present, PresentationLimits};
use crate::protocol::Shared;
use crate::query::{DeviceQueryState, DrainCursor, QueryError};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

pub fn routes() -> Router<Arc<Shared>> {
    Router::new().route("/api/stream", get(upgrade))
}

#[derive(Debug, Deserialize)]
struct StreamParams {
    device: Option<String>,
    since_cursor: Option<u64>,
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(shared): State<Arc<Shared>>,
    Query(params): Query<StreamParams>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, shared, params))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Resolve `device`/`since_cursor` (if any) into the shared query state plus
/// a starting [`DrainCursor`] — or a wire-shaped error to push once. `None`
/// device is not an error; it's T5.1's original no-subscription behavior.
fn resolve_subscription(
    shared: &Arc<Shared>,
    params: &StreamParams,
) -> Result<Option<(Arc<DeviceQueryState>, DrainCursor)>, serde_json::Value> {
    let Some(device) = &params.device else {
        return Ok(None);
    };
    let dev = DeviceId(device.clone());
    let Some(recorder) = shared.backend.recorder(&dev) else {
        return Err(
            json!({ "type": "stream_error", "code": "device_not_found", "device": device }),
        );
    };
    let state = shared.queries.get_or_spawn(&dev, recorder);
    let start = match params.since_cursor {
        Some(since_cursor) => match state.cursor_from_seq(since_cursor) {
            Ok(idx) => idx,
            Err(e) => return Err(query_error_to_stream_error(e)),
        },
        None => (state.line_count(), state.event_count()),
    };
    Ok(Some((state, start)))
}

fn query_error_to_stream_error(e: QueryError) -> serde_json::Value {
    match e {
        QueryError::DataAgedOut {
            oldest_available_seq,
        } => json!({
            "type": "stream_error",
            "code": "data_aged_out",
            "oldest_available_seq": oldest_available_seq,
        }),
        QueryError::InvalidPattern(message) => json!({
            "type": "stream_error",
            "code": "invalid_request",
            "message": message,
        }),
    }
}

/// One drain-and-present-and-push attempt. Returns `Ok(Some(next_cursor))`
/// if there was something to push (and it sent successfully), `Ok(None)`
/// if there was nothing new, `Err(())` if the send failed (socket gone) or
/// the drain itself errored (already pushed a `stream_error` in that case).
async fn drain_and_push(
    socket: &mut WebSocket,
    state: &DeviceQueryState,
    from: DrainCursor,
) -> Result<Option<DrainCursor>, ()> {
    let drained = match state.drain_since(from, None) {
        Ok(d) => d,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    query_error_to_stream_error(e).to_string().into(),
                ))
                .await;
            return Err(());
        }
    };
    if drained.lines.is_empty() && drained.events.is_empty() {
        return Ok(None);
    }
    // `present()` is the exact same context-protection layer `api.rs`'s
    // `tail` handler applies to the initial page (see this module's doc
    // comment) — applied per push batch here rather than once over the
    // whole history, so a run of identical lines that lands within one
    // `ingest` tick still folds, at the cost of never folding *across* two
    // separate push batches. `full_cursor` only has to be internally
    // consistent for `present`'s own truncation bookkeeping.
    let max_seq = drained
        .lines
        .last()
        .map(|l| l.seq)
        .into_iter()
        .chain(drained.events.last().map(|e| e.seq))
        .max()
        .unwrap_or(0);
    let page = present(
        &drained.lines,
        &drained.events,
        max_seq + 1,
        &PresentationLimits::default(),
    );
    let mut push = page_to_json(&page);
    push["type"] = json!("push");
    if socket
        .send(Message::Text(push.to_string().into()))
        .await
        .is_err()
    {
        return Err(());
    }
    Ok(Some(next_drain_cursor(from, &drained, &page)))
}

/// The resume cursor for the *next* drain, given what `present()` actually
/// included in the just-pushed page — **not** `drained.next` (the full,
/// unpresented drain result). `present()`'s own
/// `PresentationLimits::max_result_bytes` (8KB default) can truncate a
/// large batch (this is exactly what T5.2's 5,000-lines/sec throughput path
/// hits every poll tick), and blindly resuming from `drained.next` would
/// permanently skip every record `present()` left out of this page —
/// discovered via a real repro: 80 lines injected in one burst landed
/// right at the 8KB boundary, and every line past it silently never
/// arrived (see this module's tests and the PR report's "known issues
/// found and fixed" section).
///
/// `page.cursor` is "one past the last included item's highest seq" (see
/// `presentation.rs`'s module docs); since `drained.lines`/`drained.events`
/// are each already in ascending-seq order (`query.rs`'s `drain_since`
/// never reorders), counting how many of each have `seq < page.cursor` — a
/// prefix, by `present()`'s own "ranges are disjoint and truncation keeps
/// a whole prefix" invariant — gives exactly how far into *this* drain the
/// presented page actually reached, in the same `(line_idx, event_idx)`
/// terms `drained.next` uses. When nothing was truncated this lands on the
/// same value `drained.next` already had.
fn next_drain_cursor(
    from: DrainCursor,
    drained: &crate::query::DrainResult,
    page: &crate::presentation::PresentedPage,
) -> DrainCursor {
    let included_lines = drained
        .lines
        .iter()
        .take_while(|l| l.seq < page.cursor)
        .count();
    let included_events = drained
        .events
        .iter()
        .take_while(|e| e.seq < page.cursor)
        .count();
    (from.0 + included_lines, from.1 + included_events)
}

async fn handle_socket(mut socket: WebSocket, shared: Arc<Shared>, params: StreamParams) {
    let hello = json!({
        "type": "hello",
        "server_version": shared.server_version,
        "device_count": shared.backend.list_devices().len(),
        "ts": now_ms(),
    });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut subscription = match resolve_subscription(&shared, &params) {
        Ok(sub) => sub,
        Err(error) => {
            if socket
                .send(Message::Text(error.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
            None
        }
    };

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    ticker.tick().await; // first tick fires immediately — skip it, `hello` just played that role.

    loop {
        // Drain immediately after establishing a subscription (and after
        // every wake below) rather than only ever waiting on `notified()`
        // first: data already ingested before this connection subscribed
        // must not wait for the *next* `ingest` to be pushed.
        if let Some((state, from)) = &subscription {
            match drain_and_push(&mut socket, state, *from).await {
                Ok(Some(next)) => {
                    subscription = Some((Arc::clone(state), next));
                    continue;
                }
                Ok(None) => {}
                Err(()) => return,
            }
        }

        let notified = subscription.as_ref().map(|(state, _)| state.notified());
        tokio::select! {
            _ = ticker.tick() => {
                let heartbeat = json!({
                    "type": "heartbeat",
                    "server_version": shared.server_version,
                    "device_count": shared.backend.list_devices().len(),
                    "ts": now_ms(),
                });
                if socket.send(Message::Text(heartbeat.to_string().into())).await.is_err() {
                    return;
                }
            }
            () = async {
                match notified {
                    Some(n) => n.await,
                    // No active subscription: never resolves, so this
                    // branch just never wins the select — equivalent to
                    // T5.1's original hello/heartbeat-only loop.
                    None => std::future::pending().await,
                }
            } => {}
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    // T5.2 is still server-push-only for the stream itself
                    // (see this module's doc comment on why filtering
                    // stays client-side); later tasks (T5.4 approval
                    // actions) give the client something to say here.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the pure resolution logic ([`resolve_subscription`],
    //! [`query_error_to_stream_error`]) — the part of this module that
    //! decides *what* to push and from *where*. The actual socket send/recv
    //! loop (`handle_socket`) needs a real bidirectional WS connection to
    //! exercise meaningfully; per this module's doc comment and
    //! `web::mod`'s own convention, that full-stack behavior (reconnect,
    //! live push cadence, follow/pause) is Playwright E2E's job
    //! (`webui/e2e/live-log.spec.ts`), not a second in-process fake here.

    use std::sync::Arc;

    use super::*;
    use crate::protocol::backend::testing::TestBackend;
    use crate::protocol::backend::DeviceBackend;
    use crate::protocol::Shared;

    fn shared_with_device(device_id: &str) -> (Arc<Shared>, tempfile::TempDir) {
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
        backend.register(DeviceId(device_id.to_string()), recorder);
        let shared = Arc::new(Shared::new(
            backend as Arc<dyn DeviceBackend>,
            "test-version",
            tmp.path(),
        ));
        (shared, tmp)
    }

    #[test]
    fn no_device_param_is_not_an_error_and_yields_no_subscription() {
        let (shared, _tmp) = shared_with_device("dev-1");
        let params = StreamParams {
            device: None,
            since_cursor: None,
        };
        let result = resolve_subscription(&shared, &params);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn unknown_device_is_a_structured_device_not_found_error() {
        let (shared, _tmp) = shared_with_device("dev-1");
        let params = StreamParams {
            device: Some("no-such-device".to_string()),
            since_cursor: None,
        };
        let err = match resolve_subscription(&shared, &params) {
            Err(e) => e,
            Ok(_) => panic!("expected a device_not_found error"),
        };
        assert_eq!(err["type"], "stream_error");
        assert_eq!(err["code"], "device_not_found");
        assert_eq!(err["device"], "no-such-device");
    }

    #[tokio::test]
    async fn known_device_with_no_since_cursor_starts_at_the_current_tip() {
        let (shared, _tmp) = shared_with_device("dev-1");
        let id = DeviceId("dev-1".to_string());
        let recorder = shared.backend.recorder(&id).unwrap();
        recorder.append_rx(b"already here\n").unwrap();
        let state = shared.queries.get_or_spawn(&id, recorder);
        state.ingest(&shared.backend.recorder(&id).unwrap());

        let params = StreamParams {
            device: Some("dev-1".to_string()),
            since_cursor: None,
        };
        let (_state, start) = resolve_subscription(&shared, &params).unwrap().unwrap();
        // Starting "at the tip" means draining from here sees nothing new
        // yet — exactly what a fresh subscribe with no backlog should do.
        let drained = _state.drain_since(start, None).unwrap();
        assert!(drained.lines.is_empty() && drained.events.is_empty());
    }

    #[tokio::test]
    async fn since_cursor_resolves_to_the_same_position_read_since_would_start_from() {
        let (shared, _tmp) = shared_with_device("dev-1");
        let id = DeviceId("dev-1".to_string());
        let recorder = shared.backend.recorder(&id).unwrap();
        for i in 0..5 {
            recorder
                .append_rx(format!("line-{i}\n").as_bytes())
                .unwrap();
        }
        let state = shared.queries.get_or_spawn(&id, recorder.clone());
        state.ingest(&recorder);

        // since_cursor=3: resume from seq 3 onward (lines 3 and 4).
        let params = StreamParams {
            device: Some("dev-1".to_string()),
            since_cursor: Some(3),
        };
        let (_state, start) = resolve_subscription(&shared, &params).unwrap().unwrap();
        let drained = _state.drain_since(start, None).unwrap();
        assert_eq!(drained.lines.len(), 2, "{:?}", drained.lines);
        assert_eq!(drained.lines[0].text, "line-3");
        assert_eq!(drained.lines[1].text, "line-4");
    }

    // `resolve_subscription`'s `DataAgedOut` branch (a `since_cursor` below
    // the retained floor) is exercised at the `cursor_from_seq` level by
    // `query.rs`'s own `cursor_from_seq_reports_data_aged_out_below_the_floor`
    // test (unmodified by T5.2 — out of this task's scope, see the report's
    // range restrictions); `query_error_to_stream_error_shapes_both_variants`
    // below covers this module's own mapping of that error into the wire
    // shape a client sees.

    #[test]
    fn query_error_to_stream_error_shapes_both_variants() {
        let aged = query_error_to_stream_error(QueryError::DataAgedOut {
            oldest_available_seq: 42,
        });
        assert_eq!(aged["code"], "data_aged_out");
        assert_eq!(aged["oldest_available_seq"], 42);

        let invalid = query_error_to_stream_error(QueryError::InvalidPattern("bad regex".into()));
        assert_eq!(invalid["code"], "invalid_request");
        assert_eq!(invalid["message"], "bad regex");
    }

    // ---- next_drain_cursor: regression coverage for the real bug this
    // function fixes (found via `live-log.spec.ts`: 80 lines injected in
    // one burst landed right at `present()`'s 8KB default cap, and every
    // line past it never arrived because the old code resumed from
    // `drained.next` — the *full* raw drain position — instead of where
    // the truncated *page* actually stopped). ----

    fn line_for_test(seq: u64, text: &str) -> crate::query::AssembledLine {
        crate::query::AssembledLine {
            raw: text.as_bytes().to_vec(),
            text: text.to_string(),
            seq,
            t_mono: seq as f64,
            t_wall: format!("t{seq}"),
            capped: false,
        }
    }

    fn event_for_test(seq: u64) -> crate::query::OobRecord {
        crate::query::OobRecord {
            seq,
            t_mono: seq as f64,
            t_wall: format!("t{seq}"),
            kind: wrap_proto::Kind::Event,
            name: Some("disconnect".to_string()),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn matches_drained_next_when_nothing_was_truncated() {
        let lines: Vec<_> = (0..5u64)
            .map(|i| line_for_test(i, &format!("line{i}")))
            .collect();
        let drained = crate::query::DrainResult {
            lines: lines.clone(),
            events: vec![],
            next: (5, 0),
        };
        let page = crate::presentation::present(
            &drained.lines,
            &drained.events,
            5,
            &crate::presentation::PresentationLimits::default(),
        );
        assert!(!page.truncated);
        assert_eq!(next_drain_cursor((0, 0), &drained, &page), (5, 0));
    }

    #[test]
    fn stops_exactly_where_the_truncated_page_stopped_never_skipping_the_rest() {
        // Tiny cap forces truncation, mirroring presentation.rs's own
        // truncation tests' pattern.
        let lines: Vec<_> = (0..50u64)
            .map(|i| line_for_test(i, &format!("line number {i} with some padding text")))
            .collect();
        let drained = crate::query::DrainResult {
            lines: lines.clone(),
            events: vec![],
            next: (50, 0),
        };
        let tight = crate::presentation::PresentationLimits {
            max_result_bytes: 200,
            ..crate::presentation::PresentationLimits::default()
        };
        let page = crate::presentation::present(&drained.lines, &drained.events, 50, &tight);
        assert!(
            page.truncated,
            "test fixture must actually trigger truncation"
        );

        let next = next_drain_cursor((0, 0), &drained, &page);

        // The core regression: must NOT jump straight to the raw drain's
        // own end (50) — that would silently skip everything `present()`
        // truncated out.
        assert!(next.0 < 50, "{next:?}");
        assert!(next.0 > 0, "{next:?}");
        // And it must match exactly how far the presented page reached.
        let last_included_seq = page.lines.iter().map(|l| l.last_seq()).max().unwrap();
        assert_eq!(next.0, last_included_seq as usize + 1);
    }

    #[test]
    fn accounts_for_a_nonzero_starting_offset_and_mixed_lines_and_events() {
        let lines: Vec<_> = (10..15u64)
            .map(|i| line_for_test(i, &format!("line{i}")))
            .collect();
        let events = vec![event_for_test(15), event_for_test(16)];
        let drained = crate::query::DrainResult {
            lines: lines.clone(),
            events: events.clone(),
            next: (10 + lines.len(), 3 + events.len()),
        };
        let page = crate::presentation::present(
            &drained.lines,
            &drained.events,
            17,
            &crate::presentation::PresentationLimits::default(),
        );
        assert!(!page.truncated);
        // Starting from (10, 3) (this connection's own prior position —
        // unrelated to the lines'/events' own seq numbering) plus all 5
        // lines and both events included.
        assert_eq!(next_drain_cursor((10, 3), &drained, &page), (15, 5));
    }
}
