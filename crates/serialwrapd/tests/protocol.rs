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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};

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

    // Let the background poller ingest everything already on disk.
    tokio::time::sleep(Duration::from_millis(80)).await;

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
    tokio::time::sleep(Duration::from_millis(30)).await;
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
    tokio::time::sleep(Duration::from_millis(30)).await;
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

    // Let the background poller ingest everything (and its own
    // `DeviceQueryState::oldest_seq` floor get set) before subscribing with
    // a since_cursor of 0, which is now guaranteed to be aged out.
    tokio::time::sleep(Duration::from_millis(80)).await;

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
