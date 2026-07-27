//! Integration tests for the write gate's approval workflow (`TASKS.md`
//! T4.2, issue #15) against the real UDS protocol server, a real
//! [`Recorder`], and [`TestBackend`] — same substitution `tests/protocol.rs`
//! already uses (hotplug detection itself is out of scope here; see that
//! file's module docs). T4.1's own rule-matching acceptance criteria
//! (priority matrix, regex boundaries, hex-bypass) are unit-tested directly
//! against [`serialwrapd::gate::rules::RuleSet`] in `src/gate/rules.rs` —
//! this file covers what only makes sense driven over the real wire
//! protocol: the pending queue, timeouts, concurrent resolution,
//! notification-failure isolation, and the log-context payload. It also
//! covers `Request::DtrPulse`'s own write-gate hookup (`TASKS.md` T4.4,
//! issue #17) — same daemon/gate/`Client` machinery, just gating a
//! `dtr_pulse` request instead of a `write` one.
//!
//! Every test here connects `agent` clients through the real gate — no
//! fixed `sleep` is ever used as a synchronization mechanism (see
//! `wait_until_pending`'s doc comment): the one test whose entire point is
//! measuring elapsed time (`timeout_denial_precision_is_within_one_second`)
//! is the deliberate exception the project's own testing conventions carve
//! out for "the duration itself is under test".

use std::collections::HashMap;
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use serialwrapd::gate::notify::Notifier;
use serialwrapd::gate::rules::RuleSet;
use serialwrapd::gate::Gate;
use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// Bind and serve a fresh daemon instance with a caller-supplied [`Gate`]
/// (short timeouts, custom rules, or a failing notifier — whatever this
/// test needs) on a tempdir-scoped socket. See `tests/protocol.rs`'s
/// `start_test_daemon` for the plain-default-gate counterpart.
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

/// A raw PTY pair in non-canonical ("raw") mode — same construction
/// `tests/protocol.rs`'s identical helper uses — so a test can register the
/// slave side as a device's writer and read the master side directly to
/// prove byte-exactness.
fn open_raw_pty_pair() -> (File, File) {
    let pair = openpty(None, None).expect("openpty");
    let mut attrs = tcgetattr(&pair.slave).expect("tcgetattr");
    cfmakeraw(&mut attrs);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &attrs).expect("tcsetattr");
    (File::from(pair.master), File::from(pair.slave))
}

/// Block (on a blocking-pool thread) until exactly `n` bytes have been read
/// from `file`, or `timeout` elapses — same helper `tests/protocol.rs` uses.
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

/// Minimal hand-rolled protocol client — same reasoning as
/// `tests/protocol.rs`'s identical helper: this exercises the wire format
/// itself, not a client-side convenience layer. Unlike that file's version,
/// replies are read into a per-`id` buffer ([`Client::recv_for_id`]) rather
/// than assumed to arrive in send order: several of these tests keep
/// multiple write requests in flight on one connection simultaneously (each
/// its own spawned daemon-side task — see `protocol::session`'s module
/// docs), and a pending write's reply only arrives once its approval
/// resolves, which can happen in any order relative to when the requests
/// were sent.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    buffered: HashMap<u64, Value>,
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
            buffered: HashMap::new(),
        };
        let hello = json!({"op": "hello", "name": name, "type": client_type, "version": "0.1.0"});
        client.send(hello).await;
        let ack = client.recv_raw().await;
        (client, ack)
    }

    async fn send(&mut self, request: Value) {
        let line = format!("{request}\n");
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write to daemon socket");
    }

    async fn recv_raw(&mut self) -> Value {
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

    /// Read replies (buffering any that don't match) until one with wire
    /// `id == want_id` arrives. See the struct docs for why replies can't
    /// be assumed to arrive in send order here.
    async fn recv_for_id(&mut self, want_id: u64) -> Value {
        if let Some(v) = self.buffered.remove(&want_id) {
            return v;
        }
        loop {
            let v = self.recv_raw().await;
            let got_id = v["id"].as_u64();
            if got_id == Some(want_id) {
                return v;
            }
            self.buffered.insert(got_id.unwrap_or(u64::MAX), v);
        }
    }
}

/// Set up one `TestBackend` device with a real [`Recorder`] and a raw PTY
/// pair registered as its writer. Returns the socket, both tempdirs that
/// must outlive the test (socket dir, recorder data dir — see
/// `tests/protocol.rs::start_daemon_with_writable_device`'s doc comment on
/// why the *recorder's* tempdir specifically must not drop early), the
/// recorder itself (for tests that inspect the on-disk audit trail), and
/// the PTY master.
async fn start_daemon(
    device_id: &str,
    gate: Gate,
) -> (
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<Recorder>,
    File,
) {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), device_id, RecorderConfig::default())
            .expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    let id = DeviceId(device_id.to_string());
    backend.register(id.clone(), Arc::clone(&recorder));
    let (master, slave) = open_raw_pty_pair();
    backend.register_writer(&id, slave);
    let (sock_path, sockdir) =
        start_test_daemon_with_gate(backend as Arc<dyn DeviceBackend>, gate).await;
    (sock_path, sockdir, tmp_data, recorder, master)
}

fn short_timeout_gate(timeout: Duration) -> Gate {
    let mut rules = RuleSet::builtin();
    rules.timeout = timeout;
    Gate::new(rules, Arc::new(serialwrapd::gate::notify::DesktopNotifier))
}

/// A [`Notifier`] that deliberately fails every call — a real subprocess
/// spawn against a path guaranteed not to exist (`ENOENT`), exercising the
/// actual failure path a broken `notify-send`/`osascript` install would hit
/// in production, not just a no-op stub (`TASKS.md` T4.2 acceptance
/// criterion 9).
struct FailingNotifier;

impl Notifier for FailingNotifier {
    fn notify(&self, _title: &str, _body: &str) {
        let _ = std::process::Command::new("/nonexistent/serialwrap-test-notifier-binary")
            .arg("this call is expected to fail")
            .status();
    }
}

/// Poll `serialwrap approvals`'s wire equivalent (`approvals_list`) until
/// `predicate` matches one of the current pending entries, returning it.
///
/// This is a poll, not a fixed `sleep`-as-synchronization: there is no push
/// notification for "a write just became pending" to await directly (the
/// daemon's per-request task is busy blocking on the gate's own oneshot
/// receiver at that point — see `protocol::session`'s `Request::Write`
/// handler), so observing the real, already-mutated state through the same
/// `ApprovalsList` op a human's CLI would use is the actual event this
/// waits for. The deadline exists purely to turn a genuine hang/regression
/// into a clear failure, not as a guess at how long submission normally
/// takes (same convention as this crate's other `wait_until_*` test
/// helpers — see `tests/protocol.rs`'s `wait_until_client_is_waiting_for`).
async fn wait_until_pending(
    admin: &mut Client,
    next_wire_id: &mut u64,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let wire_id = *next_wire_id;
        *next_wire_id += 1;
        admin
            .send(json!({"id": wire_id, "op": "approvals_list"}))
            .await;
        let reply = admin.recv_for_id(wire_id).await;
        let approvals = reply["approvals"].as_array().cloned().unwrap_or_default();
        if let Some(entry) = approvals.iter().find(|a| predicate(a)) {
            return entry.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no matching pending approval appeared within 5s (last approvals list: {approvals:?})"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn write_request(wire_id: u64, device: &str, text: &str) -> Value {
    json!({
        "id": wire_id, "op": "write", "device": device,
        "text": text, "line_ending": "none",
    })
}

fn hex_write_request(wire_id: u64, device: &str, data_b64: &str) -> Value {
    json!({
        "id": wire_id, "op": "write", "device": device,
        "data_b64": data_b64,
    })
}

// ---- T4.1 acceptance criterion 3 (integration level): hex-encoded danger ----
//
// The pure rule-matching version of this lives in `src/gate/rules.rs`'s
// `hex_decoded_danger_command_is_caught_exactly_like_the_plain_text_form`.
// This one proves the same guarantee end-to-end through the real
// `Request::Write` handler and wire protocol: a `--hex`-shaped payload
// (`data_b64` carrying the raw bytes of `"flash_erase"`) must land in the
// pending queue tagged with the danger rule, not sail through as an
// immediate `ok`.
#[tokio::test]
async fn hex_encoded_danger_command_is_force_pended_not_allowed() {
    let (sock_path, _sockdir, _datadir, _recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    let data_b64 = BASE64.encode(b"flash_erase");
    agent.send(hex_write_request(1, "dev", &data_b64)).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
        a["bytes_text"] == "flash_erase"
    })
    .await;
    assert_eq!(
        entry["matched_rule"], "danger:erase",
        "hex-encoded flash_erase must be caught by the danger rule, not bypass it: {entry}"
    );
    assert_eq!(entry["bytes_hex"], "66 6C 61 73 68 5F 65 72 61 73 65");

    // Deny it — the point of this test is that it was force-pended at all,
    // not what happens after.
    let approval_id = entry["id"].as_u64().expect("id");
    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_deny", "approval_id": approval_id}))
        .await;
    let deny_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(deny_reply["ok"], true, "{deny_reply}");

    let write_reply = agent.recv_for_id(1).await;
    assert_eq!(write_reply["ok"], false, "{write_reply}");
    assert_eq!(
        write_reply["error"]["code"], "write_denied",
        "{write_reply}"
    );
    assert_eq!(
        write_reply["error"]["matched_rule"], "danger:erase",
        "{write_reply}"
    );
}

// ---- T4.2 acceptance criterion 5: timeout precision ≤1s ----

#[tokio::test]
async fn timeout_denial_precision_is_within_one_second() {
    // The one test in this file where real elapsed time is the thing under
    // test, not a synchronization mechanism to avoid — see the module
    // docs. 1 second is long enough to measure meaningfully while keeping
    // `cargo test --all`'s total budget comfortably under 10s.
    let configured = Duration::from_secs(1);
    let (sock_path, _sockdir, _datadir, _recorder, _master) =
        start_daemon("dev", short_timeout_gate(configured)).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    let started = Instant::now();
    agent.send(write_request(1, "dev", "reboot")).await;
    let reply = agent.recv_for_id(1).await;
    let elapsed = started.elapsed();

    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"]["code"], "write_denied", "{reply}");
    assert_eq!(reply["error"]["reason"], "timeout_1s", "{reply}");

    // The acceptance criterion allows ≤1s of error; asserted here with a
    // much tighter margin (150ms) so this is a meaningful regression test
    // of the timeout mechanism's own precision, not just confirmation that
    // it's within the maximum the spec would tolerate anyway.
    let error = elapsed.as_secs_f64() - configured.as_secs_f64();
    assert!(
        (0.0..=0.15).contains(&error),
        "timeout fired {elapsed:?} after a configured {configured:?} (error {error:.3}s) — \
         expected within [0, 150ms] of the configured value"
    );
    eprintln!("timeout precision: configured={configured:?} actual={elapsed:?} error={error:.4}s");
}

// ---- T4.2 acceptance criterion 6: structured deny reason ----

#[tokio::test]
async fn denied_write_carries_a_structured_non_empty_reason() {
    let (sock_path, _sockdir, _datadir, _recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(write_request(1, "dev", "reboot")).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
        a["bytes_text"] == "reboot"
    })
    .await;
    let approval_id = entry["id"].as_u64().expect("id");

    let wire_id = next_wire_id;
    admin
        .send(json!({
            "id": wire_id, "op": "approval_deny", "approval_id": approval_id, "reason": "not right now",
        }))
        .await;
    let deny_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(deny_reply["ok"], true, "{deny_reply}");

    let write_reply = agent.recv_for_id(1).await;
    assert_eq!(write_reply["ok"], false, "{write_reply}");
    assert_eq!(
        write_reply["error"]["code"], "write_denied",
        "{write_reply}"
    );
    // Structured: a caller can branch on `reason` programmatically — it is
    // present, non-empty, and distinct from the free-form `message`
    // string, never a bare error string or an empty field.
    let reason = write_reply["error"]["reason"]
        .as_str()
        .expect("reason must be a present, typed string field");
    assert_eq!(reason, "not right now");
    assert!(!reason.is_empty());
    assert!(
        write_reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not right now"),
        "{write_reply}"
    );
}

// ---- T4.2 acceptance criterion 7: approved write executes, tx tagged approved_by ----

#[tokio::test]
async fn approved_write_is_sent_and_tx_record_is_tagged_approved_by() {
    let (sock_path, _sockdir, _datadir, recorder, master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(write_request(1, "dev", "ok")).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "sheldon", "human").await;
    let mut next_wire_id = 100u64;
    let entry =
        wait_until_pending(&mut admin, &mut next_wire_id, |a| a["bytes_text"] == "ok").await;
    let approval_id = entry["id"].as_u64().expect("id");

    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_approve", "approval_id": approval_id}))
        .await;
    let approve_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(approve_reply["ok"], true, "{approve_reply}");

    let write_reply = agent.recv_for_id(1).await;
    assert_eq!(
        write_reply["ok"], true,
        "write must succeed once approved: {write_reply}"
    );
    assert_eq!(write_reply["written"], 2);

    // The bytes actually reached the device.
    let (_master, drained) = read_n_bytes(master, 2, Duration::from_secs(2)).await;
    assert_eq!(drained, b"ok");

    // The tx record is tagged `approved_by`, carrying the approving
    // connection's own kernel-verified identity (`"sheldon:<pid>"`), never
    // a client-supplied value.
    let read = recorder.read_since(0, 1 << 20).expect("read_since");
    let tx = read
        .records
        .iter()
        .find(|r| matches!(r, Record::Tx { .. }))
        .unwrap_or_else(|| panic!("no tx record found: {:?}", read.records));
    match tx {
        Record::Tx { gate, client, .. } => {
            assert!(
                gate.starts_with("approved_by:sheldon:"),
                "tx record's gate field must be tagged approved_by, got {gate:?}"
            );
            // `client` is `changed_by`, the same `"name:pid"` convention
            // every other identity field in this schema uses — not the
            // bare self-reported name.
            assert!(
                client.starts_with("claude-code:"),
                "expected client to start with 'claude-code:', got {client:?}"
            );
        }
        other => panic!("expected Tx, got {other:?}"),
    }
}

// ---- T4.2 acceptance criterion 8: 5 concurrent pendings resolve independently ----

#[tokio::test]
async fn five_concurrent_pending_writes_resolve_independently() {
    let (sock_path, _sockdir, _datadir, _recorder, master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    for i in 0..5u64 {
        agent
            .send(write_request(i, "dev", &format!("req-{i}")))
            .await;
    }

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let mut approval_ids = [0u64; 5];
    for (i, slot) in approval_ids.iter_mut().enumerate() {
        let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
            a["bytes_text"] == format!("req-{i}")
        })
        .await;
        *slot = entry["id"].as_u64().expect("id");
    }
    // All 5 ids must be distinct — a bug that reused/aliased ids would
    // silently merge separate requests.
    let mut sorted: Vec<u64> = approval_ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        5,
        "expected 5 distinct pending ids: {approval_ids:?}"
    );

    // Resolve them out of submission order and alternating approve/deny,
    // specifically to prove independence rather than "happens to work
    // because it's also FIFO".
    let decisions = ["approve", "deny", "approve", "deny", "approve"];
    for i in [4usize, 1, 3, 0, 2] {
        let wire_id = next_wire_id;
        next_wire_id += 1;
        let op = format!("approval_{}", decisions[i]);
        admin
            .send(json!({"id": wire_id, "op": op, "approval_id": approval_ids[i]}))
            .await;
        let reply = admin.recv_for_id(wire_id).await;
        assert_eq!(reply["ok"], true, "decide request {i} failed: {reply}");
    }

    for (i, decision) in decisions.iter().enumerate() {
        let reply = agent.recv_for_id(i as u64).await;
        match *decision {
            "approve" => assert_eq!(
                reply["ok"], true,
                "request {i} should have been approved: {reply}"
            ),
            "deny" => {
                assert_eq!(
                    reply["ok"], false,
                    "request {i} should have been denied: {reply}"
                );
                assert_eq!(reply["error"]["code"], "write_denied", "{reply}");
            }
            _ => unreachable!(),
        }
    }

    // The approved writes' bytes (req-0, req-2, req-4 — 5 bytes each) must
    // have actually reached the device; order between distinct approved
    // writes isn't constrained by this test, only that all 15 bytes
    // arrive.
    let (_master, drained) = read_n_bytes(master, "req-0".len() * 3, Duration::from_secs(2)).await;
    for approved in ["req-0", "req-2", "req-4"] {
        let bytes = approved.as_bytes();
        assert!(
            drained.windows(bytes.len()).any(|w| w == bytes),
            "expected {approved:?} among the bytes written to the device: {drained:?}"
        );
    }
}

// ---- T4.2 acceptance criterion 9: notifier failure never blocks the flow ----

#[tokio::test]
async fn notifier_failure_does_not_block_the_approval_flow() {
    let mut rules = RuleSet::builtin();
    rules.timeout = Duration::from_secs(5);
    let gate = Gate::new(rules, Arc::new(FailingNotifier));
    let (sock_path, _sockdir, _datadir, _recorder, master) = {
        let tmp_data = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(
            Recorder::open(tmp_data.path(), "dev", RecorderConfig::default())
                .expect("open recorder"),
        );
        let backend = Arc::new(TestBackend::new());
        let id = DeviceId("dev".to_string());
        backend.register(id.clone(), Arc::clone(&recorder));
        let (master, slave) = open_raw_pty_pair();
        backend.register_writer(&id, slave);
        let (sock_path, sockdir) =
            start_test_daemon_with_gate(backend as Arc<dyn DeviceBackend>, gate).await;
        (sock_path, sockdir, tmp_data, recorder, master)
    };

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(write_request(1, "dev", "ok")).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    // Reaching this point at all already proves `submit_write` didn't
    // panic or hang while firing (and failing) the notification.
    let entry =
        wait_until_pending(&mut admin, &mut next_wire_id, |a| a["bytes_text"] == "ok").await;
    let approval_id = entry["id"].as_u64().expect("id");

    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_approve", "approval_id": approval_id}))
        .await;
    let approve_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(
        approve_reply["ok"], true,
        "approve must succeed even though the notifier backend fails: {approve_reply}"
    );

    let write_reply = agent.recv_for_id(1).await;
    assert_eq!(write_reply["ok"], true, "{write_reply}");

    let (_master, drained) = read_n_bytes(master, 2, Duration::from_secs(2)).await;
    assert_eq!(drained, b"ok");
}

// ---- T4.2 acceptance criterion 10: approval payload carries log context ----

#[tokio::test]
async fn approval_payload_includes_preceding_log_lines() {
    let (sock_path, _sockdir, _datadir, recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    // Log lines that exist *before* the gated write is ever sent.
    recorder
        .append_rx(b"ota: image invalid, rollback armed\n")
        .expect("append rx context line");
    recorder
        .append_rx(b"boot ok\n")
        .expect("append rx context line");

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(write_request(1, "dev", "flash_erase")).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
        a["bytes_text"] == "flash_erase"
    })
    .await;

    let context = entry["log_context"].as_array().cloned().unwrap_or_default();
    let context_lines: Vec<&str> = context.iter().filter_map(Value::as_str).collect();
    assert!(
        context_lines.contains(&"ota: image invalid, rollback armed"),
        "expected the ota-rollback line in log_context, got {context_lines:?}"
    );
    assert!(
        context_lines.contains(&"boot ok"),
        "expected the boot-ok line in log_context, got {context_lines:?}"
    );
    assert_eq!(
        entry["matched_rule"], "danger:erase",
        "sanity check this is really the flash_erase pending entry: {entry}"
    );

    // Clean up: deny it so the test doesn't leave a background timeout
    // task as the only thing resolving it.
    let approval_id = entry["id"].as_u64().expect("id");
    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_deny", "approval_id": approval_id}))
        .await;
    let deny_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(deny_reply["ok"], true, "{deny_reply}");
    let _ = agent.recv_for_id(1).await;
}

// ---- T4.4 (issue #17): `Request::DtrPulse`'s write-gate hookup ----
//
// `TestBackend::dtr_pulse` doesn't need a PTY (unlike a real write, it
// never calls `write_bytes`) — reuses this file's own `start_daemon`
// regardless, ignoring its `master` (kept alive only so the PTY pair it
// opens doesn't itself become a source of noise).

fn dtr_pulse_request(wire_id: u64, device: &str, duration_ms: u64) -> Value {
    json!({"id": wire_id, "op": "dtr_pulse", "device": device, "duration_ms": duration_ms})
}

fn dtr_pulse_events(recorder: &Recorder) -> Vec<serde_json::Map<String, serde_json::Value>> {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == "dtr_pulse" => Some(extra),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn agent_dtr_pulse_is_gated_and_approving_it_actually_pulses() {
    let (sock_path, _sockdir, _datadir, recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(dtr_pulse_request(1, "dev", 75)).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
        a["bytes_text"]
            .as_str()
            .is_some_and(|t| t.starts_with("dtr_pulse"))
    })
    .await;
    assert_eq!(entry["requester_type"], "agent", "{entry}");
    assert_eq!(entry["bytes_text"], "dtr_pulse duration_ms=75", "{entry}");
    assert!(
        dtr_pulse_events(&recorder).is_empty(),
        "the device must not be touched while the approval is still pending"
    );

    let approval_id = entry["id"].as_u64().expect("id");
    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_approve", "approval_id": approval_id}))
        .await;
    let approve_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(approve_reply["ok"], true, "{approve_reply}");

    let reply = agent.recv_for_id(1).await;
    assert_eq!(
        reply["ok"], true,
        "dtr_pulse must succeed once approved: {reply}"
    );
    assert_eq!(reply["pulsed"], true, "{reply}");
    assert_eq!(reply["duration_ms"], 75, "{reply}");

    let events = dtr_pulse_events(&recorder);
    assert_eq!(
        events.len(),
        1,
        "expected exactly one dtr_pulse event: {events:?}"
    );
    assert_eq!(
        events[0].get("duration_ms").and_then(|v| v.as_u64()),
        Some(75)
    );
    let changed_by = events[0]
        .get("changed_by")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        changed_by.starts_with("claude-code:"),
        "changed_by must carry the agent's kernel-verified identity: {changed_by:?}"
    );
}

#[tokio::test]
async fn agent_dtr_pulse_denied_leaves_the_device_untouched() {
    let (sock_path, _sockdir, _datadir, recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut agent, _ack) = Client::connect(&sock_path, "claude-code", "agent").await;
    agent.send(dtr_pulse_request(1, "dev", 50)).await;

    let (mut admin, _ack) = Client::connect(&sock_path, "operator", "human").await;
    let mut next_wire_id = 100u64;
    let entry = wait_until_pending(&mut admin, &mut next_wire_id, |a| {
        a["bytes_text"]
            .as_str()
            .is_some_and(|t| t.starts_with("dtr_pulse"))
    })
    .await;
    let approval_id = entry["id"].as_u64().expect("id");

    let wire_id = next_wire_id;
    admin
        .send(json!({"id": wire_id, "op": "approval_deny", "approval_id": approval_id}))
        .await;
    let deny_reply = admin.recv_for_id(wire_id).await;
    assert_eq!(deny_reply["ok"], true, "{deny_reply}");

    let reply = agent.recv_for_id(1).await;
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"]["code"], "write_denied", "{reply}");

    assert!(
        dtr_pulse_events(&recorder).is_empty(),
        "a denied dtr_pulse must never actually reset the device"
    );
}

#[tokio::test]
async fn human_dtr_pulse_bypasses_the_gate_and_is_audited() {
    let (sock_path, _sockdir, _datadir, recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut human, _ack) = Client::connect(&sock_path, "sheldon", "human").await;
    human.send(dtr_pulse_request(1, "dev", 42)).await;
    let reply = human.recv_for_id(1).await;
    assert_eq!(reply["ok"], true, "human bypasses the gate: {reply}");
    assert_eq!(reply["pulsed"], true, "{reply}");

    let events = dtr_pulse_events(&recorder);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].get("duration_ms").and_then(|v| v.as_u64()),
        Some(42)
    );
}

#[tokio::test]
async fn tool_client_has_no_dtr_pulse_path() {
    let (sock_path, _sockdir, _datadir, recorder, _master) =
        start_daemon("dev", short_timeout_gate(Duration::from_secs(5))).await;

    let (mut tool, _ack) = Client::connect(&sock_path, "esptool", "tool").await;
    tool.send(dtr_pulse_request(1, "dev", 42)).await;
    let reply = tool.recv_for_id(1).await;
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"]["code"], "permission_denied", "{reply}");

    assert!(dtr_pulse_events(&recorder).is_empty());
}
