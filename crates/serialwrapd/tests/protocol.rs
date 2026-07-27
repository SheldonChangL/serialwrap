//! Protocol-layer integration tests (`TASKS.md` T1.4, issue #6) against the
//! real UDS server (`serialwrapd::protocol::server`), a real
//! [`Recorder`], and [`TestBackend`] standing in for a running
//! `HotplugDetector` (see `protocol::backend`'s module docs for why that
//! substitution is legitimate: hotplug detection itself is already
//! covered by `port_hotplug.rs`).
//!
//! Every test here drives the daemon exactly the way a real client would:
//! connect a `UnixStream`, send a `hello`, then newline-delimited JSON
//! requests — never by calling internal daemon functions directly. See
//! [`Client`] below.

use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use serialwrapd::gate::Gate;
use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// Bind and serve a fresh daemon instance on a tempdir-scoped socket.
/// Returns the socket path and the tempdir (kept alive for the caller's
/// whole test — dropping it removes the socket file).
async fn start_test_daemon(backend: Arc<dyn DeviceBackend>) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(backend, "test"));
    tokio::spawn(server::serve(listener, shared));
    (path, dir)
}

/// Same as [`start_test_daemon`], but with a caller-supplied [`Gate`]
/// instead of the default 60s-timeout builtin one — for a test that needs
/// a gated write to actually resolve (by timeout) without waiting out the
/// real production default (`TASKS.md` T4.1/T4.2, issues #14/#15).
async fn start_test_daemon_with_gate(
    backend: Arc<dyn DeviceBackend>,
    gate: Gate,
) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(backend, "test").with_gate(gate));
    tokio::spawn(server::serve(listener, shared));
    (path, dir)
}

/// A minimal hand-rolled protocol client: exactly what a real CLI/MCP
/// client will eventually be, kept deliberately dumb here so these tests
/// exercise the wire format itself rather than any client-side
/// convenience layer (none exists yet — that's T1.5/T3.1).
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &std::path::Path, name: &str, client_type: &str) -> (Self, Value) {
        let stream = UnixStream::connect(path)
            .await
            .expect("connect to daemon socket");
        let (r, w) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(r),
            writer: w,
        };
        let hello = json!({"op": "hello", "name": name, "type": client_type, "version": "0.1.0"});
        client.send_raw(format!("{hello}\n").as_bytes()).await;
        let ack = client.recv().await;
        (client, ack)
    }

    async fn send(&mut self, request: Value) {
        self.send_raw(format!("{request}\n").as_bytes()).await;
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .await
            .expect("write to daemon socket");
    }

    /// Like [`Self::send_raw`], but returns the write error instead of
    /// panicking — used for a deliberately-abusive payload where the
    /// daemon closing the connection *before* the whole write completes
    /// (a broken pipe on our end) is itself an acceptable, defensive
    /// outcome, not a bug.
    async fn try_send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes).await
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .expect("read from daemon socket");
        assert!(n > 0, "expected a reply line, got EOF");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("reply was not valid JSON: {e}: {line:?}"))
    }

    /// Like [`Self::recv`], but returns `Err` (rather than panicking) on
    /// EOF/closed connection — used where "the connection was simply
    /// closed" is an acceptable outcome alongside a structured error
    /// reply (see the malformed-input test).
    async fn try_recv(&mut self) -> std::io::Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        Ok(serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("reply was not valid JSON: {e}: {line:?}")))
    }
}

/// Block until `list_clients` (over a *separate* connection from `sock`)
/// reports some client genuinely `Activity::WaitingFor { device, pattern,
/// .. }` — i.e. until a `wait_for` call for that exact device/pattern has
/// actually reached `Request::WaitFor`'s handler and taken
/// `DeviceQueryState::wait_for`'s "checked" snapshot.
///
/// Exists for the same reason as `crates/serialwrap/tests/mcp_bridge.rs`'s
/// identically-named helper (issue #39): a test proving `wait_for` matches
/// a line that arrives *after* the call started must not just sleep a
/// guessed duration before appending that line and hope the daemon's own
/// request dispatch was faster — `wait_for` deliberately only matches lines
/// assembled from its own "checked" snapshot onward, so a guess that turns
/// out too short makes the append look like pre-existing history and the
/// call spuriously times out. `protocol::session`'s `WaitFor` handler calls
/// `shared.clients.set_activity(client_id, Activity::WaitingFor { .. })`
/// synchronously, with no `.await` in between, immediately before taking
/// that snapshot, so by the time this poll ever observes it, the snapshot
/// has already happened.
async fn wait_until_client_is_waiting_for(sock: &std::path::Path, device: &str, pattern: &str) {
    let (mut probe, _ack) = Client::connect(sock, "sync-probe", "human").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut id = 0u64;
    loop {
        id += 1;
        probe.send(json!({"id": id, "op": "list_clients"})).await;
        let reply = probe.recv().await;
        let waiting = reply["clients"].as_array().into_iter().flatten().any(|c| {
            c["activity"]["state"] == "waiting_for"
                && c["activity"]["device"] == device
                && c["activity"]["pattern"] == pattern
        });
        if waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never reported a client waiting_for device={device:?} pattern={pattern:?} \
             within 5s — real hang, not just a slow poll"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn tiny_recorder_config() -> RecorderConfig {
    // Small segments, effectively unbounded ring: forces rotation across
    // segment boundaries without ever evicting anything — matches
    // `recorder.rs`'s own `tiny_rotation_config` test helper, so a
    // protocol-layer test can force and verify the same segment-crossing
    // behavior recorder.rs already proves in isolation.
    RecorderConfig {
        segment_bytes: 300,
        ring_bytes: u64::MAX,
        checkpoint_every: 3,
        checkpoint_bytes: 100,
        fsync_interval: Duration::from_secs(3600),
    }
}

// ---- Acceptance criterion 1: 8 concurrent subscribers see identical data ----

#[tokio::test]
async fn eight_concurrent_subscribers_see_identical_seq_and_bytes() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    const SUBSCRIBERS: usize = 8;
    const LINE_COUNT: usize = 20;

    let mut clients = Vec::new();
    for i in 0..SUBSCRIBERS {
        let (mut c, ack) = Client::connect(&sock_path, &format!("sub-{i}"), "agent").await;
        assert_eq!(ack["ok"], true, "hello must succeed: {ack}");
        c.send(json!({"id": 1, "op": "subscribe", "device": "dev"}))
            .await;
        clients.push(c);
    }

    // Let every subscriber's task actually reach its snapshot-then-wait
    // point before any data exists, so none of them can miss line 0.
    tokio::time::sleep(Duration::from_millis(50)).await;

    for i in 0..LINE_COUNT {
        recorder
            .append_rx(format!("line-{i}\n").as_bytes())
            .unwrap();
    }
    // No explicit ingest call: the daemon's own background poller (5ms
    // interval) is what's under test here.

    let mut per_subscriber: Vec<Vec<(u64, String)>> = Vec::with_capacity(SUBSCRIBERS);
    for c in clients.iter_mut() {
        let mut got = Vec::new();
        while got.len() < LINE_COUNT {
            let msg = tokio::time::timeout(Duration::from_secs(2), c.recv())
                .await
                .expect("subscribe push within 2s");
            for l in msg["lines"].as_array().expect("lines array") {
                got.push((
                    l["seq"].as_u64().expect("seq"),
                    l["text"].as_str().expect("text").to_string(),
                ));
            }
        }
        per_subscriber.push(got);
    }

    let expected: Vec<(u64, String)> = (0..LINE_COUNT as u64)
        .map(|i| (i, format!("line-{i}")))
        .collect();
    for (i, got) in per_subscriber.iter().enumerate() {
        assert_eq!(
            got, &expected,
            "subscriber {i} diverged from the expected seq/text sequence"
        );
    }
    // Cross-check every subscriber against every other one directly too,
    // not just against `expected` — the actual acceptance wording ("收到
    // 的 seq/bytes 完全一致").
    for i in 1..SUBSCRIBERS {
        assert_eq!(
            per_subscriber[0], per_subscriber[i],
            "subscriber 0 vs subscriber {i} diverged"
        );
    }
    println!(
        "concurrency check: {SUBSCRIBERS} subscribers each received {LINE_COUNT} lines, byte-for-byte identical"
    );
}

// ---- Acceptance criterion 2: read_since correctness across a segment boundary, through the protocol ----

#[tokio::test]
async fn read_since_is_correct_across_a_segment_boundary_through_the_protocol() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", tiny_recorder_config()).unwrap());
    for i in 0..60 {
        recorder
            .append_rx(format!("boundary-{i:03}\n").as_bytes())
            .unwrap();
    }
    let segments_dir = tmp_data.path().join("devices/dev/segments");
    assert!(
        std::fs::read_dir(&segments_dir).unwrap().count() >= 2,
        "test needs a real segment boundary to cross"
    );

    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "reader", "human").await;

    // No sleep needed: this `read_since` below is this device's first-ever
    // query, and `QueryRegistry::get_or_spawn` performs one synchronous
    // `ingest` the first time any query touches a device specifically so
    // its first caller can never observe less than what's already
    // (synchronously, durably — see `Recorder`'s module docs on fsync only
    // affecting crash durability, not readability) on disk.
    let mut cursor = 0u64;
    let mut collected = Vec::new();
    loop {
        c.send(json!({"id": 1, "op": "read_since", "device": "dev", "cursor": cursor, "max_bytes": 40}))
            .await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], true, "read_since failed: {reply}");
        let lines = reply["lines"].as_array().unwrap();
        if lines.is_empty() {
            break;
        }
        for l in lines {
            collected.push(l["text"].as_str().unwrap().to_string());
        }
        let next = reply["cursor"].as_u64().unwrap();
        assert!(
            next > cursor,
            "cursor must always advance (was {cursor}, got {next})"
        );
        cursor = next;
    }

    let expected: Vec<String> = (0..60).map(|i| format!("boundary-{i:03}")).collect();
    assert_eq!(
        collected, expected,
        "read_since across a segment boundary must be gap-free and dupe-free"
    );
}

// ---- Acceptance criterion 3: wait_for timeout precision <=100ms ----

#[tokio::test]
async fn wait_for_timeout_precision_is_within_100ms() {
    let tmp_data = tempfile::tempdir().unwrap();
    let backend = Arc::new(TestBackend::new());
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    backend.register(DeviceId("dev".to_string()), recorder);
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "waiter", "agent").await;

    let timeout_s = 0.3;
    let start = Instant::now();
    c.send(json!({"id": 1, "op": "wait_for", "device": "dev", "pattern": "never matches", "timeout_s": timeout_s}))
        .await;
    let reply = c.recv().await;
    let elapsed = start.elapsed();

    assert_eq!(reply["result"], "timeout", "reply: {reply}");
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    let expected_ms = timeout_s * 1000.0;
    let diff_ms = (elapsed_ms - expected_ms).abs();
    println!(
        "wait_for timeout precision: requested {expected_ms:.1}ms, actual {elapsed_ms:.1}ms, diff {diff_ms:.1}ms"
    );
    assert!(
        elapsed_ms >= expected_ms,
        "must never fire before the deadline (elapsed {elapsed_ms:.1}ms)"
    );
    assert!(
        diff_ms <= 100.0,
        "timeout precision {diff_ms:.1}ms exceeds the 100ms budget"
    );
}

// ---- Acceptance criterion 4: a half-line split across two chunks must not match early ----

#[tokio::test]
async fn wait_for_does_not_match_a_half_line_split_across_two_chunks() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "waiter", "agent").await;

    // First chunk: no trailing newline yet.
    recorder.append_rx(b"Temp: 25").unwrap();

    let wait_start = Instant::now();
    c.send(json!({"id": 1, "op": "wait_for", "device": "dev", "pattern": "Temp: 25", "timeout_s": 5.0}))
        .await;

    // The background poller runs every 5ms, so it has had ~20x that long
    // to have ingested the half-line by the time this check runs. No
    // reply must have arrived yet -- the core assertion this criterion is
    // about.
    let too_early = tokio::time::timeout(Duration::from_millis(100), c.recv()).await;
    assert!(
        too_early.is_err(),
        "wait_for must not match a half-line before its terminating newline arrives, got {too_early:?}"
    );

    // Complete the line 200ms after the first chunk, exactly the
    // acceptance criterion's own scenario.
    tokio::time::sleep(Duration::from_millis(100)).await; // ~200ms total since the first chunk
    recorder.append_rx(b".7 C\n").unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(2), c.recv())
        .await
        .expect("a match within 2s of the completing chunk");
    let elapsed_ms = wait_start.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(reply["result"], "matched", "reply: {reply}");
    assert_eq!(reply["line"], "Temp: 25.7 C");
    assert!(
        elapsed_ms >= 190.0,
        "match must not have happened before the completing chunk was sent at ~200ms (matched at {elapsed_ms:.1}ms)"
    );
    println!("half-line: no premature match; matched at {elapsed_ms:.1}ms (completing chunk sent at ~200ms)");
}

// ---- Acceptance criterion 5: peer pid is the kernel-reported one ----

#[tokio::test]
async fn hello_ack_reports_the_kernel_verified_peer_pid() {
    let backend = Arc::new(TestBackend::new());
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (_client, ack) = Client::connect(&sock_path, "identity-check", "human").await;

    assert_eq!(ack["ok"], true, "ack: {ack}");
    let reported_pid = ack["pid"].as_u64().expect("pid field") as u32;
    assert_eq!(
        reported_pid,
        std::process::id(),
        "the daemon must report *this test process's* real pid, not any client-claimed value"
    );
}

// ---- Acceptance criterion 6: out-of-band events survive an exclude-all filter ----

#[tokio::test]
async fn out_of_band_events_survive_a_filter_that_excludes_every_log_line() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    recorder.append_rx(b"normal log line\n").unwrap();
    recorder
        .append_event("disconnect", serde_json::Map::new())
        .unwrap();
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "reader", "human").await;

    tokio::time::sleep(Duration::from_millis(60)).await;

    c.send(json!({
        "id": 1,
        "op": "read_since",
        "device": "dev",
        "cursor": 0,
        "filter": {"pattern": ".*", "exclude": true},
    }))
    .await;
    let reply = c.recv().await;

    assert!(
        reply["lines"].as_array().unwrap().is_empty(),
        "an exclude-all filter must drop every log line: {reply}"
    );
    let events = reply["events"].as_array().unwrap();
    assert!(
        events.iter().any(|e| e["event"] == "disconnect"),
        "the disconnect event must survive a filter that excludes all log lines: {reply}"
    );
}

// ---- Acceptance criterion 7: malformed input never panics the daemon ----

#[tokio::test]
async fn malformed_input_returns_structured_errors_and_the_daemon_stays_alive() {
    let backend = Arc::new(TestBackend::new());
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    // Oversized line (over the 8MiB per-line cap).
    {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-oversized", "agent").await;
        let mut line = br#"{"id":1,"op":"tail","device":""#.to_vec();
        line.extend(std::iter::repeat_n(b'a', 9 * 1024 * 1024));
        line.extend_from_slice(b"\",\"n\":1}\n");
        // The daemon is allowed to notice the line is oversized and close
        // the connection *before* fully reading it — that can surface as
        // our own write failing partway through (broken pipe), which is a
        // legitimate defensive outcome here, not a test bug. A structured
        // `invalid_request` reply is also acceptable if the write
        // completed before the daemon reacted. Either way, "daemon 存活"
        // is the actual criterion, not "this exact abusive connection
        // gets a reply".
        if c.try_send_raw(&line).await.is_ok() {
            if let Ok(reply) = tokio::time::timeout(Duration::from_secs(2), c.try_recv())
                .await
                .expect("some outcome within 2s")
            {
                assert_eq!(reply["ok"], false);
                assert_eq!(reply["error"]["code"], "invalid_request");
            }
        }
    }

    // Non-UTF-8 bytes.
    {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-nonutf8", "agent").await;
        c.send_raw(&[0xFF, 0xFE, 0xFD, b'\n']).await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], false, "reply: {reply}");
        assert_eq!(reply["error"]["code"], "invalid_request");
    }

    // Invalid JSON.
    {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-badjson", "agent").await;
        c.send_raw(b"{not json at all\n").await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], false, "reply: {reply}");
        assert_eq!(reply["error"]["code"], "invalid_request");
    }

    // Unknown op.
    {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-unknownop", "agent").await;
        c.send(json!({"id": 1, "op": "not_a_real_op"})).await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], false, "reply: {reply}");
        assert_eq!(reply["error"]["code"], "invalid_request");
    }

    // Missing required field (`device` for `tail`).
    {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-missingfield", "agent").await;
        c.send(json!({"id": 1, "op": "tail", "n": 1})).await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], false, "reply: {reply}");
        assert_eq!(reply["error"]["code"], "invalid_request");
    }

    // Pathological `timeout_s` values on `wait_for`: `Duration::from_secs_f64`
    // panics on a negative, `NaN`, or non-finite input, and `serde_json`
    // happily parses an overflowing literal like `1e400` to `f64::INFINITY`
    // rather than rejecting it -- exactly the kind of value this op must
    // clamp rather than hand straight to `Duration::from_secs_f64`.
    for timeout_s in ["1e400", "-5.0", "NaN"] {
        let (mut c, _ack) = Client::connect(&sock_path, "fuzz-timeout", "agent").await;
        let raw = format!(
            r#"{{"id":1,"op":"wait_for","device":"no-such-device","pattern":"x","timeout_s":{timeout_s}}}"#
        );
        c.send_raw(format!("{raw}\n").as_bytes()).await;
        // `NaN` isn't valid JSON syntax, so this one is expected to be
        // rejected at parse time; `1e400`/`-5.0` are valid JSON numbers, so
        // this reaches `wait_for` itself and must come back as a clean
        // `device_not_found` (this device doesn't exist), never a hang or
        // a crashed connection.
        let reply = tokio::time::timeout(Duration::from_secs(2), c.recv())
            .await
            .unwrap_or_else(|_| panic!("timeout_s={timeout_s} must not hang the daemon"));
        assert_eq!(
            reply["ok"], false,
            "reply for timeout_s={timeout_s}: {reply}"
        );
    }

    // The daemon itself must still be alive and responsive after all of
    // the above.
    let (mut c, _ack) = Client::connect(&sock_path, "sanity-check", "human").await;
    c.send(json!({"id": 1, "op": "list_devices"})).await;
    let reply = c.recv().await;
    assert_eq!(
        reply["ok"], true,
        "daemon did not survive the fuzz inputs: {reply}"
    );
    println!("fuzz inputs (oversized/non-utf8/invalid-json/unknown-op/missing-field/pathological-timeout_s): daemon stayed alive and responsive");
}

// ---- Acceptance criterion 8: wait_for must not block other requests on the same connection ----

#[tokio::test]
async fn wait_for_does_not_block_other_requests_on_the_same_connection() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), recorder);
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "busy", "agent").await;

    // A wait_for that will not resolve on its own for a long time.
    c.send(json!({"id": 1, "op": "wait_for", "device": "dev", "pattern": "never matches", "timeout_s": 5.0}))
        .await;

    // Immediately follow up on the SAME connection with an unrelated,
    // fast request.
    let start = Instant::now();
    c.send(json!({"id": 2, "op": "list_devices"})).await;

    let list_devices_reply = loop {
        let reply = tokio::time::timeout(Duration::from_millis(500), c.recv())
            .await
            .expect("some reply within 500ms");
        if reply["id"] == 2 {
            break reply;
        }
        // Anything else here would be id=1's wait_for outcome, which
        // shouldn't be possible this fast (nothing ever matches "never
        // matches" and the timeout is 5s) -- keep looking just in case.
    };
    let elapsed = start.elapsed();

    assert_eq!(
        list_devices_reply["ok"], true,
        "reply: {list_devices_reply}"
    );
    println!("list_devices answered in {elapsed:?} while a 5s wait_for was in flight on the same connection");
    assert!(
        elapsed < Duration::from_millis(500),
        "list_devices must not be blocked by the in-flight wait_for (took {elapsed:?})"
    );
}

// ---- Issue #32 acceptance criterion 1: byte-exact round trip over the wire ----

#[tokio::test]
async fn a_line_with_invalid_utf8_round_trips_byte_exact_via_raw_b64() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    // Ordinary text with a deliberately invalid UTF-8 run spliced in --
    // exactly what `String::from_utf8_lossy` folds into U+FFFD and cannot
    // reconstruct.
    let mut original = b"prefix-".to_vec();
    original.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x2A]);
    original.extend_from_slice(b"-suffix");
    let mut with_newline = original.clone();
    with_newline.push(b'\n');
    recorder.append_rx(&with_newline).unwrap();

    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "reader", "human").await;

    c.send(json!({"id": 1, "op": "tail", "device": "dev", "n": 10}))
        .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "tail failed: {reply}");
    let lines = reply["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1);
    let line = &lines[0];

    let raw_b64 = line["raw_b64"]
        .as_str()
        .unwrap_or_else(|| panic!("invalid-UTF-8 line must carry raw_b64: {line}"));
    let decoded = BASE64
        .decode(raw_b64)
        .expect("raw_b64 must be valid base64");
    assert_eq!(
        decoded, original,
        "decoded raw_b64 must be byte-for-byte identical to what was written"
    );

    let mut original_hasher = Sha256::new();
    original_hasher.update(&original);
    let mut decoded_hasher = Sha256::new();
    decoded_hasher.update(&decoded);
    assert_eq!(
        format!("{:x}", original_hasher.finalize()),
        format!("{:x}", decoded_hasher.finalize()),
        "sha256(original) must match sha256(decoded raw_b64)"
    );

    // The lossy `text` field is still present (as a display convenience)
    // but must not be mistaken for byte-exact — it contains the
    // replacement character where the invalid bytes were.
    assert!(
        line["text"].as_str().unwrap().contains('\u{FFFD}'),
        "text field: {line}"
    );

    println!("acceptance (issue #32) #1 — raw_b64 byte-exact round trip: sha256 matched");
}

#[tokio::test]
async fn a_valid_utf8_line_never_carries_a_redundant_raw_b64() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    recorder.append_rx(b"perfectly ordinary text\n").unwrap();
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "reader", "human").await;

    c.send(json!({"id": 1, "op": "tail", "device": "dev", "n": 10}))
        .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "tail failed: {reply}");
    let lines = reply["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].get("raw_b64").is_none(),
        "a line that's already valid UTF-8 must not pay the raw_b64 bandwidth cost: {}",
        lines[0]
    );
}

// ---- Issue #13 (T3.2): wait_for's matched line carries raw_b64 too ----
//
// T3.1 found (and documented as a known limitation) that
// `query::WaitForOutcome::Matched` only ever carried the matched line's
// lossy `text`, discarding `AssembledLine::raw` before it reached the wire
// — the same byte-fidelity gap issue #32 had already fixed for
// `tail`/`read_since`. These two tests are the wire-level proof of the fix:
// same `raw_b64`-present-only-when-not-valid-UTF-8 rule `line_json` already
// uses, now applied to `wait_for`'s own reply too.
//
// Both tests below used to `send` the `wait_for` request (fire-and-forget —
// `send` doesn't wait for a reply), sleep a guessed 30ms, then append the
// matching line -- the same class of hazard root-caused and fixed in
// `crates/serialwrap/tests/mcp_bridge.rs`'s
// `wait_for_matched_binary_line_carries_the_real_raw_hex_via_the_bridge`
// (issue #39): if the daemon takes longer than the guess to actually read,
// dispatch, and start waiting on the request, the append lands before
// `wait_for`'s "checked" snapshot and is (correctly, by its own semantics)
// never matched, so the call spuriously times out. This file talks straight
// to the daemon over one socket (no subprocess hop like the MCP bridge
// has), so the window is narrower and neither of these has actually been
// observed to flake -- but it's the identical anti-pattern, so it gets the
// identical, deterministic fix: confirm the real `Activity::WaitingFor`
// event via `wait_until_client_is_waiting_for` instead of guessing.

#[tokio::test]
async fn wait_for_matched_invalid_utf8_line_carries_raw_b64() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "waiter", "agent").await;

    // Ordinary text with a deliberately invalid UTF-8 run spliced in --
    // exactly the same fixture shape the `tail`/`read_since` byte-fidelity
    // tests above use.
    let mut original = b"status:".to_vec();
    original.extend_from_slice(&[0xFF, 0xFE, 0x80]);
    let mut with_newline = original.clone();
    with_newline.push(b'\n');

    c.send(
        json!({"id": 1, "op": "wait_for", "device": "dev", "pattern": "^status:", "timeout_s": 3.0}),
    )
    .await;
    wait_until_client_is_waiting_for(&sock_path, "dev", "^status:").await;
    recorder.append_rx(&with_newline).unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(2), c.recv())
        .await
        .expect("a match within 2s");
    assert_eq!(reply["ok"], true, "wait_for failed: {reply}");
    assert_eq!(reply["result"], "matched", "reply: {reply}");

    let raw_b64 = reply["raw_b64"]
        .as_str()
        .unwrap_or_else(|| panic!("invalid-UTF-8 matched line must carry raw_b64: {reply}"));
    let decoded = BASE64
        .decode(raw_b64)
        .expect("raw_b64 must be valid base64");
    assert_eq!(
        decoded, original,
        "raw_b64 must decode to the exact matched bytes, not a lossy reconstruction"
    );

    let mut original_hasher = Sha256::new();
    original_hasher.update(&original);
    let mut decoded_hasher = Sha256::new();
    decoded_hasher.update(&decoded);
    assert_eq!(
        format!("{:x}", original_hasher.finalize()),
        format!("{:x}", decoded_hasher.finalize()),
        "sha256(original) must match sha256(decoded raw_b64)"
    );
}

#[tokio::test]
async fn wait_for_matched_valid_utf8_line_never_carries_a_redundant_raw_b64() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "waiter", "agent").await;

    c.send(
        json!({"id": 1, "op": "wait_for", "device": "dev", "pattern": "boot ok", "timeout_s": 3.0}),
    )
    .await;
    wait_until_client_is_waiting_for(&sock_path, "dev", "boot ok").await;
    recorder.append_rx(b"boot ok\n").unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(2), c.recv())
        .await
        .expect("a match within 2s");
    assert_eq!(reply["ok"], true, "wait_for failed: {reply}");
    assert_eq!(reply["result"], "matched", "reply: {reply}");
    assert!(
        reply.get("raw_b64").is_none(),
        "a matched line that's already valid UTF-8 must not pay the raw_b64 bandwidth cost: {reply}"
    );
}

// ---- Issue #32 acceptance criterion 2: subscribe(since_cursor) has no gap ----

#[tokio::test]
async fn subscribe_since_cursor_has_no_gap_and_matches_read_since() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    for i in 0..10 {
        recorder
            .append_rx(format!("line-{i}\n").as_bytes())
            .unwrap();
    }
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    // tail "reads to seq 9" (10 lines, seq 0..=9) and gets back the cursor
    // to resume from — 10, one past the last seq it saw.
    let (mut reader, _ack) = Client::connect(&sock_path, "reader", "human").await;
    reader
        .send(json!({"id": 1, "op": "tail", "device": "dev", "n": 20}))
        .await;
    let tail_reply = reader.recv().await;
    assert_eq!(tail_reply["ok"], true, "tail failed: {tail_reply}");
    assert_eq!(tail_reply["lines"].as_array().unwrap().len(), 10);
    let cursor = tail_reply["cursor"].as_u64().expect("cursor");
    assert_eq!(cursor, 10, "cursor must be last_seq(9) + 1");

    // Subscribe from exactly that cursor, on a separate connection (this is
    // the realistic "tail, then hand the cursor to a follower" shape).
    let (mut subscriber, _ack) = Client::connect(&sock_path, "subscriber", "agent").await;
    subscriber
        .send(json!({"id": 1, "op": "subscribe", "device": "dev", "since_cursor": cursor}))
        .await;
    // Let the subscribe task actually reach its snapshot-then-wait point
    // before anything new is appended, so the append below can't race
    // ahead of the subscription being registered (same pattern
    // `eight_concurrent_subscribers_see_identical_seq_and_bytes` already
    // uses for the same reason).
    tokio::time::sleep(Duration::from_millis(50)).await;

    for i in 10..15 {
        recorder
            .append_rx(format!("line-{i}\n").as_bytes())
            .unwrap();
    }

    let mut pushed: Vec<(u64, String)> = Vec::new();
    while pushed.len() < 5 {
        let msg = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
            .await
            .expect("subscribe push within 2s");
        for l in msg["lines"].as_array().expect("lines array") {
            pushed.push((
                l["seq"].as_u64().expect("seq"),
                l["text"].as_str().expect("text").to_string(),
            ));
        }
    }

    assert_eq!(
        pushed[0],
        (10, "line-10".to_string()),
        "first pushed record must be seq cursor (=10), not a repeat of 0..=9 or a gap past it"
    );
    let expected: Vec<(u64, String)> = (10..15).map(|i| (i, format!("line-{i}"))).collect();
    assert_eq!(
        pushed, expected,
        "subscribe(since_cursor) must push exactly the new records, no gap, no dupe"
    );

    // And it must match `read_since(cursor)` called fresh, right now, over
    // the exact same range — proving "since_cursor 語意與 read_since 一致".
    reader
        .send(json!({"id": 2, "op": "read_since", "device": "dev", "cursor": cursor}))
        .await;
    let read_since_reply = reader.recv().await;
    assert_eq!(
        read_since_reply["ok"], true,
        "read_since: {read_since_reply}"
    );
    let read_since_lines: Vec<(u64, String)> = read_since_reply["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["seq"].as_u64().unwrap(),
                l["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        pushed, read_since_lines,
        "subscribe(since_cursor) and read_since(cursor) must agree exactly"
    );

    println!(
        "acceptance (issue #32) #2 — subscribe(since_cursor={cursor}) first push {:?}, matches read_since({cursor}): {read_since_lines:?}",
        pushed[0]
    );
}

#[tokio::test]
async fn subscribe_since_cursor_below_the_retained_floor_is_a_structured_data_aged_out_not_a_hang()
{
    let tmp_data = tempfile::tempdir().unwrap();
    // Tiny ring so a handful of appends actually evict old segments.
    let tiny = RecorderConfig {
        segment_bytes: 300,
        ring_bytes: 900,
        checkpoint_every: 3,
        checkpoint_bytes: 100,
        fsync_interval: Duration::from_secs(3600),
    };
    let recorder = Arc::new(Recorder::open(tmp_data.path(), "dev", tiny).unwrap());
    for i in 0..200 {
        recorder
            .append_rx(format!("payload-{i:04}\n").as_bytes())
            .unwrap();
    }
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "reader", "human").await;

    // No sleep needed: this `subscribe` is this device's first-ever query,
    // and `QueryRegistry::get_or_spawn` (called before the `since_cursor`
    // check below) performs one synchronous `ingest` first — including
    // resolving the `DataAgedOut` floor via `Recorder::read_since`'s own
    // resync path — so `oldest_seq` is already correct by the time
    // `since_cursor: 0` is checked against it.
    c.send(json!({"id": 1, "op": "subscribe", "device": "dev", "since_cursor": 0}))
        .await;
    let reply = tokio::time::timeout(Duration::from_secs(2), c.recv())
        .await
        .expect("an aged-out since_cursor must reply promptly, never hang");
    assert_eq!(reply["ok"], false, "reply: {reply}");
    assert_eq!(reply["error"]["code"], "data_aged_out", "reply: {reply}");
    assert!(
        reply["error"]["oldest_available_seq"].as_u64().unwrap() > 0,
        "reply: {reply}"
    );
    println!("acceptance (issue #32) #2 (edge case) — aged-out since_cursor: {reply}");
}

// =====================================================================
// TASKS.md T2.1 (issue #8) / T2.3 (issue #10): `write`, `set_config`
// (already-implemented protocol layer, exercised here through the wire for
// the first time), `list_clients`, `kick`, `demote`.
// =====================================================================

/// Open a raw (no line-discipline surprises) PTY pair for the write-path
/// tests below: `master` is what a test reads to see exactly what the
/// daemon wrote via `TestBackend::write_bytes`; `slave` (registered via
/// `TestBackend::register_writer`) plays the role of "the physical device
/// fd" `write_bytes` opens fresh against (see `protocol::backend::LiveBackend`'s
/// doc comment on `write_bytes` for why it's a *second* fd rather than the
/// daemon's one shared fd).
///
/// This duplicates the handful of lines `mock_device::pty::open_raw_pty`
/// already has, rather than depending on it: that module is private to the
/// `mock-device` crate (not `pub mod pty`), and — separately — this test
/// specifically needs to read the *master* side directly, which
/// `mock_device::MockDevice`'s public API never exposes (its master is
/// permanently owned by its own background `Responder` thread, and that
/// thread's own line-based command matching strips exactly the CR/LF bytes
/// a byte-exact assertion needs to see — see the acceptance criteria this
/// file proves below).
fn open_raw_pty_pair() -> (File, File) {
    let pair = openpty(None, None).expect("openpty");
    let mut attrs = tcgetattr(&pair.slave).expect("tcgetattr");
    cfmakeraw(&mut attrs);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &attrs).expect("tcsetattr");
    (File::from(pair.master), File::from(pair.slave))
}

/// Block (on a blocking-pool thread, so the daemon's own tasks on this same
/// `#[tokio::test]` runtime keep making progress concurrently) until
/// exactly `n` bytes have been read from `file`, or `timeout` elapses.
/// Returns the file back alongside what was read, so a test can keep
/// reading from the same PTY master across several sequential writes.
async fn read_n_bytes(file: File, n: usize, timeout: Duration) -> (File, Vec<u8>) {
    let task = tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut buf = vec![0u8; n];
        file.read_exact(&mut buf)
            .expect("read_exact from pty master");
        (file, buf)
    });
    tokio::time::timeout(timeout, task)
        .await
        .unwrap_or_else(|_| panic!("expected {n} bytes on the pty master within {timeout:?}"))
        .expect("blocking read task panicked")
}

/// Stand up a daemon with one `TestBackend` device, plus a fresh raw PTY
/// pair registered as that device's writer — the fixture every write-path
/// test below starts from. Returns the socket path plus *both* tempdirs
/// that must outlive the test (the socket's own, and — easy to miss, and
/// exactly the bug this comment now documents — the recorder's data
/// directory: `Recorder::read_since` freshly `File::open`s each segment
/// per call rather than caching a handle, so dropping this tempdir early
/// makes every `read_since`/`tail`/`subscribe` call fail with a silent,
/// logged-not-propagated `NotFound` — segment appends still succeed
/// through the already-open fd, which is what made this so easy to
/// misattribute to the tx-event code path itself while first writing this
/// test), and the PTY master `File` a test reads from to prove
/// byte-exactness.
async fn start_daemon_with_writable_device(
    device_id: &str,
) -> (PathBuf, tempfile::TempDir, tempfile::TempDir, File) {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), device_id, RecorderConfig::default())
            .expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    let id = DeviceId(device_id.to_string());
    backend.register(id.clone(), recorder);
    let (master, slave) = open_raw_pty_pair();
    backend.register_writer(&id, slave);
    let (sock_path, sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    (sock_path, sockdir, tmp_data, master)
}

// ---- T2.1 acceptance criterion 1: four line endings, byte-exact ----

#[tokio::test]
async fn write_line_endings_are_byte_exact_on_the_wire() {
    let (sock_path, _sockdir, _datadir, mut master) =
        start_daemon_with_writable_device("dev").await;
    let (mut c, _ack) = Client::connect(&sock_path, "writer", "human").await;

    let cases: [(&str, &str, &[u8]); 4] = [
        ("lf", "PING", b"PING\n"),
        ("crlf", "PONG", b"PONG\r\n"),
        ("cr", "FOO", b"FOO\r"),
        ("none", "BAR", b"BAR"),
    ];
    for (line_ending, text, expected) in cases {
        c.send(json!({
            "id": 1, "op": "write", "device": "dev",
            "text": text, "line_ending": line_ending,
        }))
        .await;
        let reply = c.recv().await;
        assert_eq!(reply["ok"], true, "write({line_ending}) failed: {reply}");
        assert_eq!(
            reply["written"].as_u64(),
            Some(expected.len() as u64),
            "write({line_ending}) reported the wrong byte count: {reply}"
        );

        let (returned_master, got) =
            read_n_bytes(master, expected.len(), Duration::from_secs(2)).await;
        master = returned_master;
        assert_eq!(
            got, expected,
            "line_ending={line_ending}: bytes on the wire did not match byte-for-byte"
        );
    }
    println!("acceptance (T2.1) #1 — lf/crlf/cr/none all byte-exact on the wire");
}

// ---- T2.1 acceptance criterion 2: --hex-equivalent (`data_b64`) is exact, ignores line_ending ----

#[tokio::test]
async fn write_data_b64_sends_exact_bytes_and_ignores_line_ending() {
    let (sock_path, _sockdir, _datadir, master) = start_daemon_with_writable_device("dev").await;
    let (mut c, _ack) = Client::connect(&sock_path, "writer", "human").await;

    let payload = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
    let b64 = BASE64.encode(&payload);
    // `line_ending` is deliberately non-default here — it must be ignored
    // whenever `data_b64` is present (see the wire handler's docs: a caller
    // who spelled out exact bytes wants exactly those bytes, nothing
    // appended).
    c.send(json!({
        "id": 1, "op": "write", "device": "dev",
        "data_b64": b64, "line_ending": "crlf",
    }))
    .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["written"].as_u64(), Some(4));

    let (_master, got) = read_n_bytes(master, 4, Duration::from_secs(2)).await;
    assert_eq!(
        got, payload,
        "hex/data_b64 bytes must be exact, nothing appended"
    );
    println!("acceptance (T2.1) #2 — data_b64 (the `--hex` wire equivalent) is byte-exact");
}

// ---- T2.1 acceptance criterion 3: stdin is a CLI-layer concern; see
// crates/serialwrap/tests/write_cli.rs for that acceptance test — the
// protocol layer itself has no notion of "stdin" at all (it only ever sees
// `text`/`data_b64`, whichever the CLI decided to send).

// ---- T2.1 acceptance criterion 4: tx event visible to another subscriber, correct identity ----

#[tokio::test]
async fn write_appends_a_tx_event_visible_to_another_subscriber_with_correct_identity() {
    let (sock_path, _sockdir, _datadir, master) = start_daemon_with_writable_device("dev").await;

    let (mut writer, writer_ack) = Client::connect(&sock_path, "agent-writer", "human").await;
    let writer_pid = writer_ack["pid"].as_u64().expect("pid");

    let (mut subscriber, _ack) = Client::connect(&sock_path, "watcher", "human").await;
    subscriber
        .send(json!({"id": 1, "op": "subscribe", "device": "dev"}))
        .await;
    // Let the subscribe task reach its snapshot-then-wait point before the
    // write happens (same pattern the file's other subscribe tests use).
    tokio::time::sleep(Duration::from_millis(50)).await;

    writer
        .send(json!({
            "id": 1, "op": "write", "device": "dev",
            "text": "status", "line_ending": "lf",
        }))
        .await;
    let write_reply = writer.recv().await;
    assert_eq!(write_reply["ok"], true, "{write_reply}");

    let expected_bytes = b"status\n".to_vec();
    let (_master, got) = read_n_bytes(master, expected_bytes.len(), Duration::from_secs(2)).await;
    assert_eq!(got, expected_bytes, "bytes actually written to the device");

    let push = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("a subscribe push containing the tx event within 2s");
    let events = push["events"].as_array().expect("events array");
    let tx_event = events
        .iter()
        .find(|e| e["kind"] == "tx")
        .unwrap_or_else(|| panic!("expected a tx event in the subscriber's push: {push}"));

    assert_eq!(tx_event["gate"], "human_rw", "tx event: {tx_event}");
    assert_eq!(tx_event["client_type"], "human", "tx event: {tx_event}");
    let client_field = tx_event["client"]
        .as_str()
        .unwrap_or_else(|| panic!("tx event missing `client`: {tx_event}"));
    // `client` carries "name:pid" — the same convention `changed_by` uses
    // for `config_change` — so both the self-reported name *and* the
    // kernel-verified pid travel with the event.
    assert_eq!(
        client_field,
        format!("agent-writer:{writer_pid}"),
        "tx event identity must be the writer's real name and kernel-verified pid: {tx_event}"
    );
    let decoded = BASE64
        .decode(tx_event["data_b64"].as_str().expect("data_b64"))
        .expect("valid base64");
    assert_eq!(
        decoded, expected_bytes,
        "tx event's recorded bytes: {tx_event}"
    );
    println!(
        "acceptance (T2.1) #4 — tx event visible to another subscriber, identity={client_field:?}"
    );
}

// ---- T2.3 client-type policy: `tool` has no byte-level write path at all ----
//
// This test used to also assert an `agent` write was flatly
// `permission_denied` here, "pending the T4.1 rule engine" — that stub is
// exactly what T4.1/T4.2 (issues #14/#15) replace: an `agent` write now
// goes through the real gate (whitelist/danger/default-pending) instead of
// a blanket denial. That behavior has its own dedicated, fast-running
// coverage in `tests/write_gate.rs` — asserting a *pending* or *denied*
// gate outcome here would mean waiting out a real approval timeout, which
// belongs to the gate's own test suite, not this general protocol-dispatch
// one. `tool`'s denial, by contrast, is unconditional (no byte-level write
// path exists for `tool` at all — see the Security-model wiki's policy
// table: "tool 只能走 lease") and still belongs here.
#[tokio::test]
async fn tool_writes_have_no_byte_level_write_path() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), recorder);
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    let (mut c, _ack) = Client::connect(&sock_path, "flasher", "tool").await;
    c.send(json!({
        "id": 1, "op": "write", "device": "dev",
        "text": "status", "line_ending": "lf",
    }))
    .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], false, "tool write must be denied: {reply}");
    assert_eq!(reply["error"]["code"], "permission_denied", "{reply}");
    println!("acceptance (T2.3) — tool client has no byte-level write path, only a lease");
}

// ---- T2.3 acceptance criterion (demote): a demoted client's next write is denied ----

#[tokio::test]
async fn demote_denies_a_subsequent_write_from_a_previously_allowed_human_client() {
    // A short gate timeout, not the 60s production default: after demote,
    // the write below is no longer a bypassed `human` write but a gated
    // one (`read+gated_write`) that matches neither whitelist nor danger,
    // so it default-pends — this test only cares that it's denied, not how
    // long that takes, and shouldn't have to wait 60 real seconds to see
    // it (`TASKS.md` T4.1/T4.2, issues #14/#15).
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    let id = DeviceId("dev".to_string());
    backend.register(id.clone(), recorder);
    let (master, slave) = open_raw_pty_pair();
    backend.register_writer(&id, slave);
    let mut rules = serialwrapd::gate::rules::RuleSet::builtin();
    rules.timeout = Duration::from_millis(300);
    let gate = Gate::new(rules, Arc::new(serialwrapd::gate::notify::DesktopNotifier));
    let (sock_path, _sockdir) =
        start_test_daemon_with_gate(backend as Arc<dyn DeviceBackend>, gate).await;

    let (mut writer, _ack) = Client::connect(&sock_path, "human-1", "human").await;
    writer
        .send(json!({
            "id": 1, "op": "write", "device": "dev",
            "text": "ok", "line_ending": "none",
        }))
        .await;
    let first = writer.recv().await;
    assert_eq!(
        first["ok"], true,
        "first write (still ReadWrite) failed: {first}"
    );
    let (_master, drained) = read_n_bytes(master, 2, Duration::from_secs(2)).await;
    assert_eq!(drained, b"ok");

    let (mut admin, _ack) = Client::connect(&sock_path, "admin", "human").await;
    admin.send(json!({"id": 1, "op": "list_clients"})).await;
    let list_reply = admin.recv().await;
    let target_id = list_reply["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .find(|c| c["name"] == "human-1")
        .expect("writer client must appear in list_clients")["client_id"]
        .as_u64()
        .expect("client_id");

    admin
        .send(json!({
            "id": 2, "op": "demote", "client_id": target_id, "permission": "read+gated_write",
        }))
        .await;
    let demote_reply = admin.recv().await;
    assert_eq!(demote_reply["ok"], true, "demote failed: {demote_reply}");

    writer
        .send(json!({
            "id": 2, "op": "write", "device": "dev",
            "text": "no", "line_ending": "none",
        }))
        .await;
    let second = writer.recv().await;
    assert_eq!(
        second["ok"], false,
        "write after demote must be denied: {second}"
    );
    // Demoted to `read+gated_write` (agent-equivalent): the write now goes
    // through the real gate instead of a blanket `permission_denied`.
    // `"no"` matches neither this test's (empty) whitelist nor any
    // built-in danger pattern, so it default-pends and is then auto-denied
    // once the short timeout above elapses. What this test actually
    // protects — demote takes effect on the very next write from the same
    // connection — holds regardless of exactly which gate outcome that
    // write meets: what must never happen is it succeeding as if the
    // connection were still `human`.
    assert_eq!(second["error"]["code"], "write_denied", "{second}");
    assert!(
        second["error"]["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("timeout_")),
        "{second}"
    );
    println!(
        "acceptance (T2.3) — demote takes effect on the very next write from the same connection"
    );
}

// ---- T2.3 acceptance criterion (kick): target's connection closes and an event is recorded ----

#[tokio::test]
async fn kick_closes_the_targets_connection_and_records_a_client_kicked_event() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    let (mut victim, victim_ack) = Client::connect(&sock_path, "victim", "agent").await;
    let victim_pid = victim_ack["pid"].as_u64().expect("pid");

    let (mut admin, _ack) = Client::connect(&sock_path, "admin", "human").await;
    admin.send(json!({"id": 1, "op": "list_clients"})).await;
    let list_reply = admin.recv().await;
    let target_id = list_reply["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .find(|c| c["name"] == "victim")
        .expect("victim must appear in list_clients")["client_id"]
        .as_u64()
        .expect("client_id");

    admin
        .send(json!({"id": 2, "op": "kick", "client_id": target_id}))
        .await;
    let kick_reply = admin.recv().await;
    assert_eq!(kick_reply["ok"], true, "kick failed: {kick_reply}");

    let victim_outcome = tokio::time::timeout(Duration::from_secs(2), victim.try_recv())
        .await
        .expect("kick must take effect within 2s");
    assert!(
        victim_outcome.is_err(),
        "expected the victim's connection to close, got {victim_outcome:?}"
    );

    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let kicked_extra = records
        .iter()
        .find_map(|r| match r {
            Record::Event { event, extra, .. } if event == "client_kicked" => Some(extra.clone()),
            _ => None,
        })
        .expect("expected a client_kicked event to be recorded");
    assert_eq!(
        kicked_extra.get("client_id").and_then(|v| v.as_u64()),
        Some(target_id)
    );
    assert_eq!(
        kicked_extra.get("name").and_then(|v| v.as_str()),
        Some("victim")
    );
    assert_eq!(
        kicked_extra.get("pid").and_then(|v| v.as_u64()),
        Some(victim_pid)
    );
    assert_eq!(
        kicked_extra.get("client_type").and_then(|v| v.as_str()),
        Some("agent")
    );
    println!("acceptance (T2.3) — kick closed the victim's connection and recorded client_kicked: {kicked_extra:?}");
}

// ---- T2.3 acceptance criterion (config): config_change event with old/new,
// and prior rx records stay byte-for-byte untouched ----

#[tokio::test]
async fn set_config_over_the_wire_produces_a_config_change_event_and_never_touches_prior_rx_records(
) {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder =
        Arc::new(Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap());
    recorder.append_rx(b"boot ok\n").unwrap();
    let before = recorder.read_since(0, usize::MAX).unwrap().records;

    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "operator", "human").await;

    c.send(json!({"id": 1, "op": "set_config", "device": "dev", "baud": 74880}))
        .await;
    let set_reply = c.recv().await;
    assert_eq!(set_reply["ok"], true, "set_config failed: {set_reply}");
    assert_eq!(set_reply["config"]["baud"].as_u64(), Some(74880));

    // No sleep needed: `set_config`'s handler appends the `config_change`
    // event synchronously (`device_profile::append_config_change_event`)
    // before it ever replies, and this `read_since` is this device's
    // first-ever query -- `QueryRegistry::get_or_spawn`'s synchronous first
    // ingest already covers it (`set_config` itself never touches the query
    // machinery, so there's no earlier query to have "used up" that
    // guarantee).
    c.send(json!({"id": 2, "op": "read_since", "device": "dev", "cursor": 0}))
        .await;
    let read_reply = c.recv().await;
    assert_eq!(read_reply["ok"], true, "read_since failed: {read_reply}");
    let config_change = read_reply["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|e| e["event"] == "config_change")
        .unwrap_or_else(|| panic!("expected a config_change event in {read_reply}"));
    assert_eq!(config_change["new"]["baud"].as_u64(), Some(74880));
    assert!(
        config_change["old"].is_object(),
        "expected an `old` config snapshot: {config_change}"
    );

    // "先前錄的資料不重新解讀" — the rx record from before the config
    // change must be byte-for-byte identical afterward.
    let after = recorder.read_since(0, usize::MAX).unwrap().records;
    assert_eq!(
        before[0], after[0],
        "changing baud must never alter a previously recorded rx record"
    );
    println!("acceptance (T2.3) — config_change carries old/new over the wire; prior rx record unchanged: {config_change}");
}

// ---- T2.3 acceptance criterion (clients): the triple — name, verified pid, type, permission ----

#[tokio::test]
async fn list_clients_reports_name_verified_pid_type_permission_and_traffic() {
    let backend = Arc::new(TestBackend::new());
    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, ack) = Client::connect(&sock_path, "human-op", "human").await;
    let my_pid = ack["pid"].as_u64().expect("pid");

    c.send(json!({"id": 1, "op": "list_clients"})).await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");
    let me = reply["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .find(|cl| cl["name"] == "human-op")
        .expect("self must appear in list_clients");

    assert_eq!(
        me["pid"].as_u64(),
        Some(my_pid),
        "pid must be the kernel-verified value, never a client claim: {me}"
    );
    assert_eq!(me["type"], "human", "{me}");
    assert_eq!(me["permission"], "read+write", "{me}");
    assert!(
        me["bytes_in"].as_u64().unwrap_or(0) > 0,
        "this connection's own list_clients request should already count as bytes_in: {me}"
    );
    assert!(me.get("bytes_out").is_some(), "{me}");
    println!("acceptance (T2.3) — clients triple (name/verified pid/type) plus permission and traffic: {me}");
}
