//! Protocol-layer acceptance tests for T2.2's lease mode (issue #9) against
//! [`TestBackend`] — the same substitution `tests/protocol.rs` already uses
//! for everything that isn't hotplug detection itself (see that file's
//! module docs). `tests/lease.rs` covers the two things that specifically
//! need the *real* fd lifecycle instead (the shared-fd fix, and residual-
//! lease recovery across a simulated daemon restart); everything else about
//! the lease state machine — acquire/release, event fields and timing,
//! `follow` staying connected across a lease, and `--lease-timeout`'s
//! daemon-side safety net — is exercised here, driven over the real wire
//! protocol exactly the way a CLI client would.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// Bind and serve a fresh daemon instance on a tempdir-scoped socket. See
/// `tests/protocol.rs`'s identical helper.
async fn start_test_daemon(backend: Arc<dyn DeviceBackend>) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(backend, "test", dir.path()));
    tokio::spawn(server::serve(listener, shared));
    (path, dir)
}

/// Minimal hand-rolled protocol client — see `tests/protocol.rs`'s
/// identical helper for why it's kept this dumb.
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
        client.send(hello).await;
        let ack = client.recv().await;
        (client, ack)
    }

    async fn send(&mut self, request: Value) {
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write to daemon socket");
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

    /// Like [`Self::recv`], bounded so a test that expects the connection
    /// to stay alive (issue #9's "follow doesn't disconnect" criterion)
    /// fails with a clear timeout instead of hanging forever if it's wrong.
    async fn recv_within(&mut self, timeout: Duration) -> Value {
        tokio::time::timeout(timeout, self.recv())
            .await
            .unwrap_or_else(|_| panic!("no reply/push within {timeout:?}"))
    }
}

fn tiny_recorder_config() -> RecorderConfig {
    RecorderConfig::default()
}

fn find_events<'a>(records: &'a [Record], name: &str) -> Vec<&'a serde_json::Map<String, Value>> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == name => Some(extra),
            _ => None,
        })
        .collect()
}

/// Poll `ListDevices` (which also drives `TestBackend`'s lazy lease-timeout
/// check — see `protocol::backend::testing::TestBackend::maybe_expire_lease`)
/// until `device`'s `connected` field matches `want`, or `timeout` elapses.
async fn wait_for_connected(client: &mut Client, device: &str, want: bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        client.send(json!({"id": 1, "op": "list_devices"})).await;
        let reply = client.recv().await;
        let connected = reply["devices"]
            .as_array()
            .and_then(|devs| devs.iter().find(|d| d["id"] == device))
            .and_then(|d| d["connected"].as_bool());
        if connected == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "device {device:?} never reported connected={want} within {timeout:?} (last saw \
             connected={connected:?})"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---- Criteria 2 & 3: full acquire -> release lifecycle, event fields, and
// timing that matches reality ----

#[tokio::test]
async fn lease_acquire_release_lifecycle_has_correct_events_and_timing() {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), "dev", tiny_recorder_config()).expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));

    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "runner", "human").await;

    // Recording is normal *before* the lease: inject rx bytes directly
    // (standing in for the device producing output) and read them back.
    recorder
        .append_rx(b"before-lease-boot-log\n")
        .expect("append_rx");
    c.send(json!({"id": 1, "op": "tail", "device": "dev", "n": 10}))
        .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(
        reply["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|l| l["text"] == "before-lease-boot-log")),
        "expected the pre-lease line to already be recorded: {reply}"
    );

    let acquire_started = Instant::now();
    c.send(json!({
        "id": 2, "op": "lease_acquire", "device": "dev",
        "command": "esptool.py write_flash 0x0 firmware.bin",
    }))
    .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "lease_acquire failed: {reply}");
    let token = reply["token"].as_str().expect("token").to_string();
    assert!(!reply["path"].as_str().expect("path").is_empty());

    // While leased: the device must report disconnected — this is the same
    // signal `write`/`get_config`/etc. already key off of, and what makes a
    // concurrent human's `write` correctly fail during the gap instead of
    // racing whatever the leased tool is doing.
    wait_for_connected(&mut c, "dev", false, Duration::from_secs(1)).await;

    // Hold the lease open briefly so `duration_ms` is a real, nonzero,
    // measurable elapsed time rather than possibly rounding to 0.
    tokio::time::sleep(Duration::from_millis(60)).await;

    c.send(json!({"id": 3, "op": "lease_release", "token": token, "exit_code": 0}))
        .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "lease_release failed: {reply}");
    let reported_duration_ms = reply["duration_ms"].as_u64().expect("duration_ms");
    let actual_elapsed_ms = acquire_started.elapsed().as_millis() as u64;
    assert!(
        reported_duration_ms <= actual_elapsed_ms + 200,
        "duration_ms ({reported_duration_ms}) should not exceed the test's own measured elapsed \
         time ({actual_elapsed_ms}) by more than a small margin"
    );
    assert!(
        reported_duration_ms >= 50,
        "duration_ms ({reported_duration_ms}) should reflect the ~60ms the lease was actually \
         held, not read as ~0"
    );

    // Recording resumes after release.
    wait_for_connected(&mut c, "dev", true, Duration::from_secs(1)).await;
    recorder
        .append_rx(b"after-lease-boot-log\n")
        .expect("append_rx");
    // `tail`'s query state is fed by a background poller over the recorder
    // (see `query::DEFAULT_POLL_INTERVAL`), independent of this test
    // appending directly — poll until it catches up rather than asserting
    // on the very next tick.
    let deadline = Instant::now() + Duration::from_secs(1);
    let (found, last_reply) = loop {
        c.send(json!({"id": 4, "op": "tail", "device": "dev", "n": 10}))
            .await;
        let reply = c.recv().await;
        let found = reply["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|l| l["text"] == "after-lease-boot-log"));
        if found || Instant::now() >= deadline {
            break (found, reply);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(
        found,
        "expected the post-lease line to be recorded too: {last_reply}"
    );

    // Event fields: `lease_start` carries command/pid/token/timeout_s;
    // `lease_end` carries command/pid/token/exit_code/duration_ms/reason —
    // and lease_start's seq/t_wall precede lease_end's, bracketing the gap
    // in the right order.
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let starts = find_events(&records, "lease_start");
    let ends = find_events(&records, "lease_end");
    assert_eq!(starts.len(), 1, "{records:?}");
    assert_eq!(ends.len(), 1, "{records:?}");
    let start = starts[0];
    let end = ends[0];

    assert_eq!(
        start.get("command").and_then(|v| v.as_str()),
        Some("esptool.py write_flash 0x0 firmware.bin")
    );
    assert_eq!(
        start.get("pid").and_then(|v| v.as_u64()),
        Some(std::process::id() as u64),
        "lease_start's pid should be this connection's own kernel-verified peer pid"
    );
    assert_eq!(
        start.get("token").and_then(|v| v.as_str()),
        Some(token.as_str())
    );
    assert!(start.get("timeout_s").is_some_and(|v| v.is_null()));

    assert_eq!(
        end.get("command").and_then(|v| v.as_str()),
        Some("esptool.py write_flash 0x0 firmware.bin")
    );
    assert_eq!(
        end.get("pid").and_then(|v| v.as_u64()),
        Some(std::process::id() as u64)
    );
    assert_eq!(
        end.get("token").and_then(|v| v.as_str()),
        Some(token.as_str())
    );
    assert_eq!(end.get("exit_code").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(
        end.get("duration_ms").and_then(|v| v.as_u64()),
        Some(reported_duration_ms)
    );
    assert_eq!(end.get("reason").and_then(|v| v.as_str()), Some("released"));

    let start_record = records
        .iter()
        .find(|r| matches!(r, Record::Event { event, .. } if event == "lease_start"))
        .unwrap();
    let end_record = records
        .iter()
        .find(|r| matches!(r, Record::Event { event, .. } if event == "lease_end"))
        .unwrap();
    assert!(
        start_record.seq() < end_record.seq(),
        "lease_start must precede lease_end in the event stream"
    );
    assert!(
        start_record.t_wall() <= end_record.t_wall(),
        "lease_start's wall time must not be after lease_end's"
    );

    println!(
        "acceptance (T2.2) #2/#3 — full acquire/run/release lifecycle: recording normal before \
         and after, lease_start/lease_end carry command/pid/token/exit_code/duration_ms, and \
         reported duration_ms ({reported_duration_ms}ms) matches the ~60ms actually held"
    );
}

// ---- Criterion 4: another subscriber's follow gets events, not a
// disconnect, across the whole lease window ----

#[tokio::test]
async fn follow_receives_lease_events_instead_of_disconnecting() {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), "dev", tiny_recorder_config()).expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));

    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;

    let (mut subscriber, _ack) = Client::connect(&sock_path, "watcher", "agent").await;
    subscriber
        .send(json!({"id": 1, "op": "subscribe", "device": "dev"}))
        .await;
    // Let the subscribe task actually reach its snapshot-then-wait point
    // before the lease starts (same pattern `tests/protocol.rs`'s own
    // subscribe tests use).
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut runner, _ack) = Client::connect(&sock_path, "runner", "human").await;
    runner
        .send(json!({
            "id": 1, "op": "lease_acquire", "device": "dev", "command": "esptool.py write_flash",
        }))
        .await;
    let reply = runner.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");
    let token = reply["token"].as_str().expect("token").to_string();

    let push = subscriber.recv_within(Duration::from_secs(2)).await;
    assert_eq!(
        push["ok"], true,
        "subscriber got an error, not a push: {push}"
    );
    assert!(
        push["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e["event"] == "lease_start")),
        "expected the subscriber's push to contain a lease_start event, got: {push}"
    );

    runner
        .send(json!({"id": 2, "op": "lease_release", "token": token, "exit_code": 0}))
        .await;
    let reply = runner.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");

    let push = subscriber.recv_within(Duration::from_secs(2)).await;
    assert_eq!(
        push["ok"], true,
        "subscriber got an error, not a push: {push}"
    );
    assert!(
        push["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e["event"] == "lease_end")),
        "expected the subscriber's push to contain a lease_end event, got: {push}"
    );

    // The connection is still alive and still following: new rx data
    // arrives as a normal push, exactly as it would with no lease ever
    // having happened.
    recorder
        .append_rx(b"post-lease-data\n")
        .expect("append_rx after release");
    let push = subscriber.recv_within(Duration::from_secs(2)).await;
    assert_eq!(push["ok"], true, "{push}");
    assert!(
        push["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|l| l["text"] == "post-lease-data")),
        "expected the subscriber to keep receiving ordinary data after the lease ended: {push}"
    );

    println!(
        "acceptance (T2.2) #4 — a concurrent subscriber receives lease_start/lease_end as \
         ordinary pushes (never a disconnect/error) and keeps following normally afterward"
    );
}

// ---- Criterion 6: `--lease-timeout` reclaims the port even if nobody ever
// calls lease_release ----

#[tokio::test]
async fn lease_timeout_is_reclaimed_by_the_daemon_without_an_explicit_release() {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), "dev", tiny_recorder_config()).expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId("dev".to_string()), Arc::clone(&recorder));

    let (sock_path, _sockdir) = start_test_daemon(backend as Arc<dyn DeviceBackend>).await;
    let (mut c, _ack) = Client::connect(&sock_path, "runner", "human").await;

    let acquired_at = Instant::now();
    c.send(json!({
        "id": 1, "op": "lease_acquire", "device": "dev", "command": "stuck-tool",
        "timeout_s": 0.2,
    }))
    .await;
    let reply = c.recv().await;
    assert_eq!(reply["ok"], true, "{reply}");

    // No lease_release ever sent — the daemon's own deadline must reclaim
    // it, well within this task's 1-second bound.
    wait_for_connected(&mut c, "dev", true, Duration::from_secs(1)).await;
    let elapsed = acquired_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "port was not reclaimed within 1s of the lease-timeout deadline (took {elapsed:?})"
    );

    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let ends = find_events(&records, "lease_end");
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(
        ends[0].get("reason").and_then(|v| v.as_str()),
        Some("timeout")
    );
    assert!(
        ends[0].get("exit_code").is_some_and(|v| v.is_null()),
        "a timeout-reclaimed lease never learned a real exit status"
    );

    println!(
        "acceptance (T2.2) #6 — `timeout_s` is enforced by the daemon itself: the port is \
         reclaimed ({elapsed:?} after the deadline) with no lease_release ever sent"
    );
}
