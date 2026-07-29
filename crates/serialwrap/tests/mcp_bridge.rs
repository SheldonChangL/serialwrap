//! Integration tests for `serialwrap mcp` (`TASKS.md` T3.1, issue #12).
//!
//! Every test here drives the *actual compiled* `serialwrap` binary as a
//! subprocess (`env!("CARGO_BIN_EXE_serialwrap")`), talking real
//! newline-delimited JSON-RPC over its stdin/stdout — never by calling
//! `mcp::*` functions in-process — mirroring the discipline
//! `crates/serialwrap/tests/tail_cli.rs` already established one level down
//! (CLI, not library code) and `crates/serialwrapd/tests/protocol.rs`
//! established at the protocol layer itself.
//!
//! Devices are simulated the same way most of `protocol.rs`'s own tests do:
//! a real [`Recorder`] registered under [`TestBackend`], fed directly via
//! `append_rx`/`append_event` rather than a PTY. That's a deliberate,
//! precedented scope choice — device detection/PTY realism is already
//! covered elsewhere (T0.2/T1.1); what this file needs is *some* device
//! with a real recorded stream behind it to prove the MCP bridge's own
//! behavior, exactly the reasoning `protocol::backend`'s module docs give
//! for `TestBackend` existing at all.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use serialwrapd::gate::rules::RuleSet;
use serialwrapd::gate::Gate;
use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// A running test daemon: the socket clients should connect to, plus
/// whatever keeps it alive for the test's lifetime.
struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
}

/// Stand up a daemon with one simulated device (a real [`Recorder`], no PTY)
/// already registered — same shape as `tail_cli.rs`'s own
/// `start_daemon_with_device` helper.
async fn start_daemon_with_device(device_id: &str, recorder: Arc<Recorder>) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId(device_id.to_string()), recorder);
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(
        backend as Arc<dyn DeviceBackend>,
        "test",
        dir.path(),
    ));
    tokio::spawn(server::serve(listener, shared));
    TestDaemon {
        socket_path: path,
        _dir: dir,
    }
}

// ---- T4.4 (issue #17) write-path test helpers ----
//
// A short-timeout `Gate` plus a raw PTY pair registered as the device's
// writer -- so a `write` the gate ultimately allows can actually reach
// something a test can read back. Same shape `serialwrapd/tests/write_gate.rs`'s
// own `start_daemon` uses, duplicated here for the same reason every other
// helper in this file is duplicated rather than shared (each `tests/*.rs`
// file is its own crate).

fn short_timeout_gate(timeout: Duration) -> Gate {
    let mut rules = RuleSet::builtin();
    rules.timeout = timeout;
    Gate::new(rules, Arc::new(serialwrapd::gate::notify::DesktopNotifier))
}

fn open_raw_pty_pair() -> (std::fs::File, std::fs::File) {
    let pair = openpty(None, None).expect("openpty");
    let mut attrs = tcgetattr(&pair.slave).expect("tcgetattr");
    cfmakeraw(&mut attrs);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &attrs).expect("tcsetattr");
    (
        std::fs::File::from(pair.master),
        std::fs::File::from(pair.slave),
    )
}

async fn start_daemon_with_gate_and_pty(
    device_id: &str,
    gate: Gate,
) -> (TestDaemon, Arc<Recorder>, std::fs::File) {
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

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(
        Shared::new(backend as Arc<dyn DeviceBackend>, "test", dir.path()).with_gate(gate),
    );
    tokio::spawn(server::serve(listener, shared));
    // `tmp_data` must outlive the test (the recorder holds open fds into
    // it) -- leak its lifetime, same reasoning as `start_daemon_with_empty_device`
    // below.
    std::mem::forget(tmp_data);
    (
        TestDaemon {
            socket_path: path,
            _dir: dir,
        },
        recorder,
        master,
    )
}

/// Block (on a blocking-pool thread) until exactly `n` bytes have been read
/// from `file`, or `timeout` elapses -- same helper `write_gate.rs`/
/// `protocol.rs` both use.
async fn read_n_bytes(file: std::fs::File, n: usize, timeout: Duration) -> Vec<u8> {
    use std::io::Read as _;
    let task = tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut buf = vec![0u8; n];
        file.read_exact(&mut buf)
            .expect("read_exact from pty master");
        buf
    });
    tokio::time::timeout(timeout, task)
        .await
        .unwrap_or_else(|_| panic!("expected {n} bytes on the pty master within {timeout:?}"))
        .expect("blocking read task panicked")
}

/// Poll the real, compiled `serialwrap approvals` CLI until it lists at
/// least one pending approval, then approve (or deny) the first one it
/// sees -- exactly what an operator watching from another terminal would
/// type. Every test that uses this only ever has one pending approval in
/// flight at a time, so "the first one" is unambiguous.
async fn decide_first_pending(socket: &Path, decision: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let id = loop {
        let output = Command::new(env!("CARGO_BIN_EXE_serialwrap"))
            .env("SERIALWRAP_SOCKET", socket)
            .arg("approvals")
            .output()
            .await
            .expect("run `serialwrap approvals`");
        assert!(
            output.status.success(),
            "serialwrap approvals failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let listing = String::from_utf8_lossy(&output.stdout).into_owned();
        if let Some(first_line) = listing.lines().find(|l| {
            l.split('\t')
                .next()
                .is_some_and(|id| id.parse::<u64>().is_ok())
        }) {
            let id: u64 = first_line
                .split('\t')
                .next()
                .expect("checked above")
                .parse()
                .expect("checked above");
            break id;
        }
        assert!(
            Instant::now() < deadline,
            "no pending approval ever appeared in `serialwrap approvals`: {listing:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let output = Command::new(env!("CARGO_BIN_EXE_serialwrap"))
        .env("SERIALWRAP_SOCKET", socket)
        .args(["approvals", decision, &id.to_string()])
        .output()
        .await
        .unwrap_or_else(|e| panic!("run `serialwrap approvals {decision} {id}`: {e}"));
    assert!(
        output.status.success(),
        "serialwrap approvals {decision} {id} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    id
}

fn tx_records(recorder: &Recorder) -> Vec<Record> {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .filter(|r| matches!(r, Record::Tx { .. }))
        .collect()
}

fn gate_records(recorder: &Recorder) -> Vec<Record> {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .filter(|r| matches!(r, Record::Gate { .. }))
        .collect()
}

async fn start_daemon_with_empty_device(device_id: &str) -> (TestDaemon, Arc<Recorder>) {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), device_id, RecorderConfig::default()).expect("recorder"),
    );
    // Leak the data tempdir's lifetime into the returned tuple via the
    // recorder holding no reference to it — the recorder itself owns its
    // open files, so dropping `data_dir` here would remove the directory
    // out from under it. Keep it alive by never dropping it: leak is fine,
    // this is test-only and short-lived.
    std::mem::forget(data_dir);
    let daemon = start_daemon_with_device(device_id, Arc::clone(&recorder)).await;
    (daemon, recorder)
}

/// Block until the daemon's own bookkeeping shows *some* connected client
/// registered as `Activity::WaitingFor { device, pattern, .. }` — i.e. until
/// a `wait_for` call for that exact device/pattern has actually reached
/// `protocol::session`'s `Request::WaitFor` handler and taken
/// `DeviceQueryState::wait_for`'s "checked" snapshot.
///
/// Why this exists (issue #39): a test that wants to prove `wait_for`
/// blocks and then matches a line that arrives *after* the call started
/// cannot just spawn a task that sleeps a guessed number of milliseconds
/// before appending that line and hope it's long enough — `wait_for`
/// deliberately only matches lines assembled from the moment its own
/// "checked" snapshot is taken onward (see `query::DeviceQueryState::wait_for`'s
/// docs), so if the append happens to land *before* that snapshot (e.g.
/// because spawning the `serialwrap mcp` subprocess, its IPC round trip to
/// the daemon, and request dispatch together happen to take longer than the
/// guessed delay — very plausible on a loaded, shared CI runner running a
/// debug build), the line is treated as pre-existing history and never
/// matches, and `wait_for` burns its entire timeout before reporting a
/// (spurious) timeout. That is exactly the failure mode issue #39 observed:
/// `"result":"timeout"` at ~3000ms elapsed, not a late-but-real match.
///
/// This polls the daemon directly over a *separate* raw protocol
/// connection (bypassing the MCP subprocess and its stdio bridge, which
/// don't expose `list_clients` at all) purely to detect that real,
/// already-observable event, rather than guessing how long it takes to
/// happen. `protocol::session`'s `WaitFor` handler calls
/// `shared.clients.set_activity(client_id, Activity::WaitingFor { .. })`
/// synchronously, with no `.await` in between, immediately before calling
/// `DeviceQueryState::wait_for` (whose first action is that "checked"
/// snapshot) — so by the time this poll ever observes `WaitingFor`, the
/// snapshot has already happened, and it is safe to append data right
/// after this returns.
async fn wait_until_client_is_waiting_for(socket: &Path, device: &str, pattern: &str) {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect sync-probe socket");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let hello = json!({"op": "hello", "name": "sync-probe", "type": "human", "version": "0.1.0"});
    write_half
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .expect("write hello");
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read hello ack");

    // Ceiling purely to turn a genuine hang/regression into a clear failure
    // instead of an infinite loop — not a guess at how long the real event
    // takes (see the doc comment above).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut id = 0u64;
    loop {
        id += 1;
        line.clear();
        write_half
            .write_all(format!("{}\n", json!({"id": id, "op": "list_clients"})).as_bytes())
            .await
            .expect("write list_clients");
        reader
            .read_line(&mut line)
            .await
            .expect("read list_clients reply");
        let reply: Value = serde_json::from_str(&line).expect("list_clients reply was valid JSON");
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

fn spawn_line_collector<R>(reader: R) -> (Arc<Mutex<Vec<String>>>, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let lines = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&lines);
    let task = tokio::spawn(async move {
        let mut lines_stream = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines_stream.next_line().await {
            collected.lock().unwrap().push(line);
        }
    });
    (lines, task)
}

/// A spawned `serialwrap mcp` process, talking JSON-RPC over stdio.
struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    _stdout_task: JoinHandle<()>,
    _stderr_task: JoinHandle<()>,
    next_id: u64,
    /// How many of `stdout`'s collected lines this client has already
    /// scanned past while looking for a specific reply — persists across
    /// calls so a later `request` doesn't re-match an earlier reply.
    read_idx: usize,
}

impl McpProcess {
    fn spawn(socket: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_serialwrap"))
            .env("SERIALWRAP_SOCKET", socket)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `serialwrap mcp`");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout_pipe = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let (stdout, stdout_task) = spawn_line_collector(stdout_pipe);
        let (stderr, stderr_task) = spawn_line_collector(stderr_pipe);
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            _stdout_task: stdout_task,
            _stderr_task: stderr_task,
            next_id: 0,
            read_idx: 0,
        }
    }

    async fn write_line(&mut self, value: &Value) {
        let mut s = value.to_string();
        s.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin
            .write_all(s.as_bytes())
            .await
            .expect("write to mcp stdin");
        stdin.flush().await.expect("flush mcp stdin");
    }

    /// Send a JSON-RPC request and block until its matching reply (by
    /// `id`) appears on stdout, or `timeout` elapses. Polls the
    /// background-collected stdout buffer on a short fixed interval purely
    /// as a "detect a hang" mechanism — the actual completion is
    /// event-driven (whatever line the subprocess emits, whenever it
    /// emits it), not a guessed fixed wait.
    async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;

        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.stdout.lock().unwrap().clone();
            while self.read_idx < snapshot.len() {
                let line = &snapshot[self.read_idx];
                self.read_idx += 1;
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    panic!("stdout line was not valid JSON: {line:?}");
                };
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    return value;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {timeout:?} waiting for a reply to {method} (id={id}); \
                     stdout so far: {snapshot:?}; stderr so far: {:?}",
                    self.stderr.lock().unwrap()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_with_timeout(method, params, Duration::from_secs(5))
            .await
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn initialize(&mut self) {
        let reply = self
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
            )
            .await;
        assert!(reply.get("result").is_some(), "initialize failed: {reply}");
        self.notify("notifications/initialized", json!({})).await;
    }

    /// Call a tool via `tools/call`, returning its `structuredContent`
    /// (falling back to parsing `content[0].text` if that's ever absent —
    /// it never should be for this bridge, but a clear panic beats a
    /// confusing one if it somehow were).
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = reply.get("result").unwrap_or_else(|| {
            panic!("tools/call {name} returned a JSON-RPC error, not a tool result: {reply}")
        });
        assert_eq!(
            result.get("isError"),
            Some(&Value::Bool(false)),
            "tool {name} reported isError: {result}"
        );
        result
            .get("structuredContent")
            .cloned()
            .unwrap_or_else(|| panic!("tool result missing structuredContent: {result}"))
    }

    /// Like [`Self::call_tool`], but for a call expected to fail — returns
    /// the error text instead of asserting success.
    async fn call_tool_expecting_error(&mut self, name: &str, arguments: Value) -> String {
        let reply = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = reply["result"].clone();
        assert_eq!(
            result["isError"], true,
            "expected an error result: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    async fn shutdown(mut self) -> std::process::ExitStatus {
        // Dropping stdin closes the pipe, which is what makes the
        // subprocess's own stdin-reading loop see EOF and exit cleanly —
        // the normal way an MCP host disconnects from a stdio server.
        self.stdin = None;
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("child exits within 5s of stdin closing")
            .expect("wait on child")
    }
}

// ---- Acceptance criterion 1: wait_for -> tail flow, no sleep in the agent's own trace ----

// This test used to stand in for "the mock device's own boot latency" with
// a background task that slept a guessed 50ms before appending the boot
// lines. That's the same class of hazard fixed in
// `wait_for_matched_binary_line_carries_the_real_raw_hex_via_the_bridge`
// above (see its root-cause comment): the guess races the actual time it
// takes to spawn the `serialwrap mcp` subprocess and get `wait_for`'s
// request all the way to `DeviceQueryState::wait_for`'s "checked" snapshot,
// and if that ever takes longer than the guess, the append is (correctly,
// by `wait_for`'s own semantics) treated as pre-existing history and never
// matches. Confirming the real, already-observable "this client is now
// actually waiting" event removes the guess entirely — and, as a bonus,
// makes the "elapsed < 1500ms" check below an even sharper proof of
// event-driven completion (no artificial latency to hide behind).
#[tokio::test]
async fn wait_for_then_tail_completes_with_no_sleep_in_the_agents_tool_trace() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let writer_recorder = Arc::clone(&recorder);
    let socket_path = daemon.socket_path.clone();
    tokio::spawn(async move {
        wait_until_client_is_waiting_for(&socket_path, "dev", "boot ok").await;
        writer_recorder
            .append_rx(b"booting...\nboot ok\nstatus: nominal\n")
            .expect("append boot + status lines");
    });

    let call_start = Instant::now();

    // No sleep between this call and the next: `wait_for` itself is the
    // synchronization primitive. This is a real blocking read on the
    // subprocess's stdout for however long it actually takes -- there is
    // no polling loop, no guessed delay, nothing standing in for genuine
    // event-driven completion.
    let wait_result = mcp
        .call_tool(
            "wait_for",
            json!({"device": "dev", "pattern": "boot ok", "timeout_s": 3.0}),
        )
        .await;
    assert_eq!(
        wait_result["result"], "matched",
        "wait_for result: {wait_result}"
    );

    // Immediately follow up with `tail` -- again, no sleep in between.
    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 5}))
        .await;
    let elapsed = call_start.elapsed();

    let texts: Vec<String> = tail_result["lines"]
        .as_array()
        .expect("lines array")
        .iter()
        .map(|l| l["text"].as_str().unwrap().to_string())
        .collect();
    assert!(
        texts.iter().any(|t| t == "status: nominal"),
        "tail after wait_for must already see the status line written before wait_for matched: {texts:?}"
    );

    // Evidence the flow was event-driven, not "slept out the full
    // timeout then got lucky": total time for wait_for-match + tail is
    // dominated by the ~50ms simulated boot delay plus IPC, nowhere near
    // the 3s timeout_s a naive sleep-then-poll implementation would have
    // burned.
    assert!(
        elapsed < Duration::from_millis(1500),
        "flow took {elapsed:?}, suspiciously close to the 3s wait_for timeout -- looks like it \
         waited out a fixed delay instead of matching the moment the line appeared"
    );
    println!(
        "acceptance #1 — wait_for matched, then tail saw status line, total elapsed {elapsed:?}"
    );

    mcp.shutdown().await;
}

// ---- Acceptance criterion 2: wait_for timeout is a structured result ----

#[tokio::test]
async fn wait_for_timeout_returns_a_structured_result_never_a_hang_or_empty_reply() {
    let (daemon, _recorder) = start_daemon_with_empty_device("dev").await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let start = Instant::now();
    // Bounded well above the tool's own 150ms timeout_s, purely so a
    // regression that actually hangs fails the test instead of the whole
    // suite.
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        mcp.call_tool(
            "wait_for",
            json!({"device": "dev", "pattern": "never matches anything", "timeout_s": 0.15}),
        ),
    )
    .await
    .expect("wait_for must return well within 3s, never hang");
    let elapsed = start.elapsed();

    assert_eq!(result["result"], "timeout", "result: {result}");
    assert!(result["elapsed_ms"].as_f64().is_some(), "result: {result}");
    assert!(result["timeout_s"].as_f64().is_some(), "result: {result}");
    assert!(
        result.get("events").is_some(),
        "even a timed-out wait_for must carry an events array: {result}"
    );
    assert!(
        elapsed >= Duration::from_millis(140),
        "must not fire before its own deadline (elapsed {elapsed:?})"
    );
    println!("acceptance #2 — wait_for timeout: structured result {result}, elapsed {elapsed:?}");

    mcp.shutdown().await;
}

// ---- Acceptance criterion 3: disconnect surfaces in the next read tool's result ----

#[tokio::test]
async fn disconnect_event_appears_in_the_next_read_tool_calls_result() {
    // Two independent scenarios (fresh daemon/device/process each), one
    // per code path this bridge uses to surface out-of-band events -- see
    // `events.rs`'s module docs on why a device's watermark, once a call
    // has surfaced an event, does not repeat it to a *later* call. Mixing
    // both paths against the same device+session would just prove that
    // dedup logic works, not that "the very next read tool call after the
    // disconnect sees it" holds for each path independently.
    tail_result_embeds_the_disconnect_event().await;
    get_config_fetches_the_disconnect_event_separately().await;
}

// Path 1: `tail`'s own daemon reply already embeds events (see
// `query::DeviceQueryState::tail`'s docs) -- the bridge must pass them
// through untouched.
async fn tail_result_embeds_the_disconnect_event() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    recorder
        .append_rx(b"normal line before disconnect\n")
        .expect("append rx");
    recorder
        .append_event("disconnect", serde_json::Map::new())
        .expect("append disconnect event");
    // No sleep needed before the first query against a fresh device:
    // `append_rx`/`append_event` are synchronous and durable the instant
    // they return (fsync only affects crash durability, not readability —
    // see `Recorder`'s module docs), and `QueryRegistry::get_or_spawn`
    // performs one synchronous `ingest` the very first time any query
    // touches a device specifically so its very first caller can never
    // observe less than what's already on disk. The `tail` call below is
    // that first call.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 10}))
        .await;
    let tail_events = tail_result["events"].as_array().expect("events array");
    assert!(
        tail_events.iter().any(|e| e["event"] == "disconnect"),
        "tail result missing the disconnect event: {tail_result}"
    );

    // Regression: the daemon's own `tail` reply always carries a device's
    // *entire* event history (see `query::DeviceQueryState::tail`'s docs),
    // not just what's new -- a second, immediate `tail` call must not
    // repeat the same disconnect event (both because `tail_description()`
    // promises "since your last call", and because blindly forwarding the
    // full history on every call would grow unboundedly over a long
    // session -- exactly the kind of context-flooding this project's MCP
    // bridge exists to prevent).
    let second_tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 10}))
        .await;
    let second_tail_events = second_tail_result["events"]
        .as_array()
        .expect("events array");
    assert!(
        !second_tail_events.iter().any(|e| e["event"] == "disconnect"),
        "a second tail call must not re-deliver an already-surfaced disconnect event: {second_tail_result}"
    );

    println!("acceptance #3 (path 1/2) — disconnect event present once in `tail`'s result, not repeated on the next call");

    mcp.shutdown().await;
}

// Path 2: `get_config` has no native `events` field from the daemon at
// all -- the bridge must fetch it separately (see
// `tools::ToolRegistry::fetch_new_events`) and still surface it.
async fn get_config_fetches_the_disconnect_event_separately() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    recorder
        .append_event("disconnect", serde_json::Map::new())
        .expect("append disconnect event");
    // No sleep needed -- same reasoning as `tail_result_embeds_the_disconnect_event`
    // above: `get_config`'s `fetch_new_events` issues a `QueryEvents`
    // request, which is this device's first-ever query and therefore gets
    // `QueryRegistry::get_or_spawn`'s synchronous first ingest.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let config_result = mcp.call_tool("get_config", json!({"device": "dev"})).await;
    let config_events = config_result["events"].as_array().expect("events array");
    assert!(
        config_events.iter().any(|e| e["event"] == "disconnect"),
        "get_config result missing the disconnect event: {config_result}"
    );
    println!("acceptance #3 (path 2/2) — disconnect event present in `get_config`'s result");

    mcp.shutdown().await;
}

// ---- Acceptance criterion 4: binary lines use the real raw_b64 bytes ----

#[tokio::test]
async fn binary_line_hex_is_derived_from_the_real_raw_b64_bytes_not_lossy_text() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    let mut payload = b"prefix-".to_vec();
    payload.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x2A]);
    payload.extend_from_slice(b"-suffix");
    let mut with_newline = payload.clone();
    with_newline.push(b'\n');
    recorder
        .append_rx(&with_newline)
        .expect("append binary payload");

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 10}))
        .await;
    let lines = tail_result["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1, "lines: {lines:?}");
    let line = &lines[0];

    assert_eq!(line["binary"], true, "line: {line}");
    let raw_hex = line["raw_hex"]
        .as_str()
        .expect("raw_hex present for a binary line");
    let decoded: Vec<u8> = raw_hex
        .split(' ')
        .map(|byte| u8::from_str_radix(byte, 16).expect("valid hex byte"))
        .collect();
    assert_eq!(
        decoded, payload,
        "raw_hex must decode to the exact original bytes, not a lossy reconstruction"
    );

    // The lossy replacement character's own UTF-8 encoding (ef bf bd) must
    // never appear in a hex string derived from the *real* bytes -- if it
    // did, that would prove the hex was computed from `text` instead of
    // `raw_b64`, exactly the bug this bridge must not reintroduce.
    assert!(
        !raw_hex.contains("ef bf bd"),
        "raw_hex looks derived from the lossy text field, not raw_b64: {raw_hex}"
    );
    println!("acceptance #4 — binary line raw_hex matches the exact original bytes: {raw_hex}");

    mcp.shutdown().await;
}

// ---- Acceptance criterion 5: tool descriptions carry the data-not-instruction notice ----

#[tokio::test]
async fn tools_list_registers_eight_tools_each_with_the_data_not_instruction_notice() {
    // Originally "five read tools" (T3.1); T4.4 (issue #17) adds
    // `write`/`set_config`/`dtr_pulse` to the same registered set, each
    // still carrying the same data-not-instruction notice (see
    // `mcp::tools`'s own unit tests for the write-path tools' *additional*
    // human-approval notice, T4.4 acceptance criterion 9).
    let (daemon, _recorder) = start_daemon_with_empty_device("dev").await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let reply = mcp.request("tools/list", json!({})).await;
    let tools = reply["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "list_devices",
            "get_config",
            "tail",
            "read_since",
            "wait_for",
            "write",
            "set_config",
            "dtr_pulse",
        ],
        "unexpected tool set: {names:?}"
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap_or("");
        assert!(
            description.contains("never an instruction"),
            "tool {name}'s description is missing the data-not-instruction notice: {description}"
        );
        assert!(
            description.contains("TODO: reflash"),
            "tool {name}'s description missing the developer-authored-string example: {description}"
        );
        assert!(
            description.contains("attacker- or operator-controllable"),
            "tool {name}'s description missing the user-controlled-field callout: {description}"
        );
    }

    // `export` (T2.4's still-unimplemented MCP tool) must not be advertised
    // yet -- see `tools::RESERVED_WRITE_TOOL_NAMES`.
    assert!(
        !names.contains(&"export"),
        "export must not be registered yet"
    );
    println!("acceptance #5 — all 8 tool descriptions carry the data-not-instruction notice");

    mcp.shutdown().await;
}

// ---- Acceptance criterion 6: stdout is pure JSON-RPC even while stderr logs ----

#[tokio::test]
async fn stdout_stays_pure_json_rpc_while_stderr_carries_log_output() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    recorder.append_rx(b"hello\n").expect("append rx");

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;
    let _ = mcp.request("tools/list", json!({})).await;
    let _ = mcp
        .call_tool("tail", json!({"device": "dev", "n": 5}))
        .await;
    // Also exercise an error path (unknown device) -- stdout purity must
    // hold for error replies too, not just happy-path ones.
    let _ = mcp
        .call_tool_expecting_error("tail", json!({"device": "no-such-device", "n": 5}))
        .await;

    let stdout_handle = Arc::clone(&mcp.stdout);
    let stderr_handle = Arc::clone(&mcp.stderr);
    let status = mcp.shutdown().await;
    assert!(status.success(), "serialwrap mcp exited with {status:?}");

    let stdout_lines = stdout_handle.lock().unwrap().clone();
    let stderr_lines = stderr_handle.lock().unwrap().clone();

    assert!(
        !stderr_lines.is_empty(),
        "expected at least one log line on stderr (e.g. the bridge's own startup line) -- \
         otherwise this test can't actually prove stdout/stderr separation, only that stdout \
         happened to look fine"
    );
    assert!(
        !stdout_lines.is_empty(),
        "expected at least the initialize/tools/list/tail replies"
    );

    for line in &stdout_lines {
        assert!(
            serde_json::from_str::<Value>(line).is_ok(),
            "a stdout line was not valid JSON -- the protocol channel is corrupted: {line:?}\n\
             all stdout lines: {stdout_lines:?}"
        );
    }
    println!(
        "acceptance #6 — {} stdout lines all valid JSON; {} stderr log lines present",
        stdout_lines.len(),
        stderr_lines.len()
    );
}

// ---- Structural: read_since's cursor semantics survive the bridge ----

#[tokio::test]
async fn read_since_cursor_from_tail_continues_without_gap_or_duplicate() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    for i in 0..5 {
        recorder
            .append_rx(format!("line-{i}\n").as_bytes())
            .expect("append rx");
    }
    // No sleep needed before this first `tail` query -- see
    // `tail_result_embeds_the_disconnect_event`'s comment on
    // `get_or_spawn`'s synchronous first ingest.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 20}))
        .await;
    assert_eq!(tail_result["lines"].as_array().unwrap().len(), 5);
    let cursor = tail_result["cursor"].as_u64().expect("cursor");

    recorder.append_rx(b"line-5\n").expect("append rx");
    // Unlike the first `tail` above, this is *not* the device's first-ever
    // query, so there's no synchronous ingest to lean on here -- this
    // relies on the background poller (`query::DEFAULT_POLL_INTERVAL`,
    // 5ms) to pick up `line-5`. Poll `read_since` until it actually shows
    // up rather than sleeping a guessed multiple of the poll interval and
    // hoping: a real device/CI runner's poll cadence isn't something a test
    // should have to predict.
    let deadline = Instant::now() + Duration::from_secs(2);
    let read_since_result = loop {
        let result = mcp
            .call_tool("read_since", json!({"device": "dev", "cursor": cursor}))
            .await;
        if !result["lines"].as_array().unwrap_or(&Vec::new()).is_empty() {
            break result;
        }
        assert!(
            Instant::now() < deadline,
            "read_since never saw line-5 within 2s of the background poller running"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let lines = read_since_result["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1, "lines: {lines:?}");
    assert_eq!(lines[0]["text"], "line-5");
    println!("structural — read_since(cursor from tail) picked up exactly the new line");

    mcp.shutdown().await;
}

// ---- T3.2 (issue #13): context-protection presentation layer ----
//
// The tests below exercise the presentation layer end to end: real daemon,
// real `serialwrap mcp` subprocess, wire round trip and all — not just
// `serialwrapd::presentation`'s own (much more exhaustive) unit tests. Those
// unit tests are the authoritative proof of the folding/binary/cursor
// invariants across many small and adversarial cases; these confirm the
// wiring (wire reconstruction -> `present` -> tool JSON) is correct for the
// literal acceptance-criterion scenarios.

/// Pull a `binary_summary` (if present) or the line's own `text` out of a
/// `tail`/`read_since` line-or-fold JSON entry, plus the raw seq range it
/// covers -- used by the cursor-equivalence test below to compare a
/// paginated read against a whole one regardless of exactly where fold
/// boundaries happened to fall.
fn expand_presented_lines(lines: &[Value]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for l in lines {
        let content_key = if let Some(summary) = l.get("binary_summary") {
            format!("bin:{}:{}", summary["length"], summary["hex_preview"])
        } else {
            format!("text:{}", l["text"])
        };
        if l.get("folded") == Some(&Value::Bool(true)) {
            let first = l["first_seq"].as_u64().expect("first_seq");
            let last = l["last_seq"].as_u64().expect("last_seq");
            for seq in first..=last {
                out.push((seq, content_key.clone()));
            }
        } else {
            out.push((l["seq"].as_u64().expect("seq"), content_key));
        }
    }
    out
}

// ---- Acceptance criterion 1: 1MB binary -> tail response <=8KB, with a
// length + hex-preview summary ----

#[tokio::test]
async fn binary_stream_over_1mb_is_summarized_into_a_capped_tail_response() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    // Deterministic 1MB "binary" stream: cycle every byte value 0..=255.
    // Byte value 0x0A (10) recurs roughly every 256 bytes and acts as this
    // stream's own line terminator -- exactly like a real mixed-encoding
    // device dump that happens to contain 0x0A among the noise.
    let payload: Vec<u8> = (0..1_000_000usize).map(|i| (i % 256) as u8).collect();
    recorder
        .append_rx(&payload)
        .expect("append 1MB binary payload");
    // No sleep needed before this first `tail` query -- see
    // `tail_result_embeds_the_disconnect_event`'s comment on
    // `get_or_spawn`'s synchronous first ingest.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 100_000}))
        .await;
    let response_bytes = serde_json::to_string(&tail_result).unwrap().len();
    let lines = tail_result["lines"].as_array().expect("lines array");
    assert!(
        !lines.is_empty(),
        "expected at least one assembled line from the 1MB payload"
    );
    let summary = lines
        .iter()
        .find_map(|l| l.get("binary_summary"))
        .unwrap_or_else(|| panic!("expected at least one binary_summary entry: {tail_result}"));
    let length = summary["length"].as_u64().expect("length field");
    let hex_preview = summary["hex_preview"].as_str().expect("hex_preview field");
    assert!(length > 0, "summary: {summary}");
    assert!(!hex_preview.is_empty(), "summary: {summary}");
    assert!(
        response_bytes <= 8192,
        "tail response was {response_bytes} bytes, expected <= 8192 (truncated={}, {} presented \
         line entries)",
        tail_result["truncated"],
        lines.len()
    );
    println!(
        "acceptance (T3.2 #1) — 1MB binary -> tail response {response_bytes} bytes (<=8192); \
         {} presented line entries; sample binary_summary length={length}",
        lines.len()
    );

    mcp.shutdown().await;
}

// ---- Acceptance criterion 2: 100k duplicate lines fold with the exact
// count ----

#[tokio::test]
#[ignore = "100k-line acceptance-criterion reproduction; run via `cargo test -- --ignored` \
            (also wired into CI). Small/fast folding coverage (including the fold-vs-event \
            boundary invariant) lives in serialwrapd::presentation's own unit tests."]
async fn one_hundred_thousand_duplicate_lines_fold_with_the_exact_count_via_tail() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    for _ in 0..100_000 {
        recorder.append_rx(b"read timeout\n").expect("append rx");
    }
    // No sleep needed before this first `tail` query -- see
    // `tail_result_embeds_the_disconnect_event`'s comment on
    // `get_or_spawn`'s synchronous first ingest.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 200_000}))
        .await;
    let lines = tail_result["lines"].as_array().expect("lines array");
    let fold = lines
        .iter()
        .find(|l| l.get("folded") == Some(&Value::Bool(true)))
        .unwrap_or_else(|| panic!("expected a folded entry: {tail_result}"));
    let count = fold["count"].as_u64().expect("count field");
    assert_eq!(
        count, 100_000,
        "fold count must exactly equal the number of injected duplicate lines"
    );
    let response_bytes = serde_json::to_string(&tail_result).unwrap().len();
    println!(
        "acceptance (T3.2 #2) — 100000 duplicate lines -> tail response {response_bytes} bytes, \
         folded count={count}, truncated={}",
        tail_result["truncated"]
    );

    mcp.shutdown().await;
}

// ---- Acceptance criterion 3: cursor pagination with folding+truncation
// both enabled matches a whole read exactly ----

#[tokio::test]
async fn cursor_pagination_with_folding_and_truncation_matches_a_whole_read() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    for i in 0..5 {
        recorder
            .append_rx(format!("alpha-{i}\n").as_bytes())
            .expect("append rx");
    }
    for _ in 0..6 {
        recorder.append_rx(b"dup-a\n").expect("append rx");
    }
    recorder
        .append_event("disconnect", serde_json::Map::new())
        .expect("append event");
    for _ in 0..4 {
        recorder.append_rx(b"dup-b\n").expect("append rx");
    }
    for i in 0..5 {
        recorder
            .append_rx(format!("beta-{i}\n").as_bytes())
            .expect("append rx");
    }
    // No sleep needed before the first `tail` query below -- see
    // `tail_result_embeds_the_disconnect_event`'s comment on
    // `get_or_spawn`'s synchronous first ingest.

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    // "Whole read": one `tail` call, generous enough that nothing needs
    // truncating (the wiki's own default 8KB cap is already far more than
    // this small dataset needs).
    let whole = mcp
        .call_tool("tail", json!({"device": "dev", "n": 1000}))
        .await;
    assert_eq!(whole["truncated"], false, "whole read: {whole}");
    let whole_trace = expand_presented_lines(whole["lines"].as_array().unwrap());
    let whole_event_count = whole["events"].as_array().unwrap().len();
    assert_eq!(whole_event_count, 1, "whole read events: {whole}");

    // Paginated: a tiny `max_result_bytes` forces many `read_since` round
    // trips, with folding still enabled throughout.
    let mut cursor = 0u64;
    let mut paginated_trace = Vec::new();
    let mut paginated_event_count = 0usize;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 500, "must terminate; possible infinite loop");
        let page = mcp
            .call_tool(
                "read_since",
                json!({"device": "dev", "cursor": cursor, "max_result_bytes": 40}),
            )
            .await;
        let lines = page["lines"].as_array().expect("lines array");
        let events = page["events"].as_array().expect("events array");
        if lines.is_empty() && events.is_empty() {
            break;
        }
        paginated_trace.extend(expand_presented_lines(lines));
        paginated_event_count += events.len();
        let next_cursor = page["cursor"].as_u64().expect("cursor");
        assert!(
            next_cursor > cursor,
            "cursor must always advance to make progress: page {page}"
        );
        cursor = next_cursor;
    }

    assert_eq!(
        paginated_trace, whole_trace,
        "paginated per-seq content trace must exactly match the whole read — no gap, no \
         duplicate, regardless of where the tiny cap forced fold boundaries to fall"
    );
    assert_eq!(
        paginated_event_count, whole_event_count,
        "paginated events must exactly match the whole read"
    );
    println!(
        "acceptance (T3.2 #3, MCP level) — {pages} read_since pages reconstruct exactly the \
         whole tail read: {} line-seqs, {paginated_event_count} events",
        paginated_trace.len()
    );

    mcp.shutdown().await;
}

// ---- wait_for byte fidelity (T3.2's "順帶" fix, issue #13) ----

// ROOT CAUSE (Linux CI flake, issue #39): this test used to spawn a task
// that slept a guessed 30ms before appending the matching line, racing that
// guess against however long it actually takes to spawn the `serialwrap
// mcp` subprocess, complete its IPC round trip to this in-process daemon,
// and have `protocol::session` dispatch the `wait_for` request —
// `DeviceQueryState::wait_for` only ever matches lines assembled *from the
// moment its own "checked" snapshot is taken onward* (by design: see its
// docs), so if that whole chain took longer than 30ms — plausible on a
// loaded, shared, debug-build CI runner — the append landed *before* the
// snapshot, was treated as pre-existing history, and `wait_for` correctly
// (per its own semantics) never matched it, burning the full `timeout_s`
// before giving up. That is exactly what was observed:
// `"result":"timeout","elapsed_ms":3001.3` — a genuine, full-length
// timeout, not a late arrival.
//
// The fix is not a bigger delay (that just narrows the window without
// closing it) but confirming the real, already-observable event this test
// actually depends on: `wait_until_client_is_waiting_for` polls the
// daemon's own `list_clients` bookkeeping (over a separate connection —
// the MCP bridge doesn't expose that op) until it reports this client
// genuinely `Activity::WaitingFor` this device/pattern, which can only be
// true *after* the "checked" snapshot has already been taken (see that
// function's doc comment for why). Only then do we append.
#[tokio::test]
async fn wait_for_matched_binary_line_carries_the_real_raw_hex_via_the_bridge() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let mut payload = b"status:".to_vec();
    payload.extend_from_slice(&[0xFF, 0xFE, 0x80]);
    let mut with_newline = payload.clone();
    with_newline.push(b'\n');

    let writer_recorder = Arc::clone(&recorder);
    let socket_path = daemon.socket_path.clone();
    tokio::spawn(async move {
        wait_until_client_is_waiting_for(&socket_path, "dev", "^status:").await;
        writer_recorder
            .append_rx(&with_newline)
            .expect("append binary line");
    });

    let result = mcp
        .call_tool(
            "wait_for",
            json!({"device": "dev", "pattern": "^status:", "timeout_s": 3.0}),
        )
        .await;
    assert_eq!(result["result"], "matched", "result: {result}");
    assert_eq!(result["binary"], true, "result: {result}");
    let raw_hex = result["raw_hex"]
        .as_str()
        .expect("raw_hex present for a binary matched line");
    let decoded: Vec<u8> = raw_hex
        .split(' ')
        .map(|byte| u8::from_str_radix(byte, 16).expect("valid hex byte"))
        .collect();
    assert_eq!(
        decoded, payload,
        "raw_hex must decode to the exact original bytes, not a lossy reconstruction"
    );
    assert!(
        !raw_hex.contains("ef bf bd"),
        "raw_hex looks derived from the lossy text field, not raw_b64: {raw_hex}"
    );
    println!("acceptance (T3.2 wait_for fix) — matched binary line raw_hex: {raw_hex}");

    mcp.shutdown().await;
}

// ---- T4.4 (issue #17): the MCP bridge's write-path tools ----
//
// Same discipline as the rest of this file, plus (for the human-operator
// side of the write gate's approval flow) the *actual compiled*
// `serialwrap approvals` CLI subcommand, never an in-process shortcut --
// this is what a real "agent sends a dangerous command, a human approves it
// from another terminal" session actually looks like end to end.

// ---- T4.4 acceptance criterion 5: whitelisted command executes directly ----

#[tokio::test]
async fn agent_sends_a_whitelisted_command_and_it_executes_immediately() {
    // Not anchored with a trailing `$`: `write`'s default `line_ending`
    // appends a `\n` server-side (see `protocol::session`'s `Request::Write`
    // handler), so the bytes the gate actually evaluates are `"status\n"`,
    // not the bare `"status"` a `$`-anchored pattern would require.
    let toml_text = "[approval]\ntimeout_s = 5\n[[whitelist]]\npattern = \"^status\"\n";
    let tmp = tempfile::tempdir().unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, toml_text).unwrap();
    let rules = RuleSet::load(&rules_path).expect("test rules.toml is valid");
    let gate = Gate::new(rules, Arc::new(serialwrapd::gate::notify::DesktopNotifier));

    let (daemon, recorder, master) = start_daemon_with_gate_and_pty("dev", gate).await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let result = mcp
        .call_tool("write", json!({"device": "dev", "text": "status"}))
        .await;
    assert_eq!(result["result"], "allowed", "{result}");
    assert_eq!(result["written"], 7, "{result}"); // "status\n"

    let got = read_n_bytes(master, 7, Duration::from_secs(2)).await;
    assert_eq!(got, b"status\n");

    let txs = tx_records(&recorder);
    assert_eq!(txs.len(), 1, "{txs:?}");
    match &txs[0] {
        Record::Tx { gate, .. } => assert!(
            gate.starts_with("whitelist:"),
            "expected an immediate whitelist allow, got gate={gate:?}"
        ),
        other => panic!("expected Tx, got {other:?}"),
    }
    println!("acceptance #5 (T4.4) — whitelisted `status` executed immediately via MCP write");

    mcp.shutdown().await;
}

// ---- T4.4 acceptance criterion 6 (the headline scenario): S4 ----
//
// agent sends flash_erase -> blocked -> denied by timeout -> agent sends it
// again -> a human approves via the real `serialwrap approvals` CLI ->
// execution succeeds. Both decisions, both replies, and the audit trail
// must all be traceable.

#[tokio::test]
async fn s4_flash_erase_denied_by_timeout_then_approved_on_retry() {
    let (daemon, recorder, master) =
        start_daemon_with_gate_and_pty("dev", short_timeout_gate(Duration::from_secs(1))).await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    // --- First decision: force-pended by the built-in `erase` danger rule,
    // then denied by the 1s fail-safe timeout. This call genuinely blocks
    // for ~1s -- that's the real gate timeout firing, not a test sleep.
    let first = mcp
        .call_tool("write", json!({"device": "dev", "text": "flash_erase"}))
        .await;
    assert_eq!(first["result"], "denied", "{first}");
    assert_eq!(first["reason"], "timeout_1s", "{first}");
    assert_eq!(first["matched_rule"], "danger:erase", "{first}");

    // --- Second decision: the same command again, this time approved by a
    // human via the real, compiled `serialwrap approvals` CLI running
    // concurrently with the (blocked) MCP call.
    let approver_socket = daemon.socket_path.clone();
    let approver =
        tokio::spawn(async move { decide_first_pending(&approver_socket, "approve").await });

    let second = mcp
        .call_tool("write", json!({"device": "dev", "text": "flash_erase"}))
        .await;
    let approved_id = approver.await.expect("approver task panicked");

    assert_eq!(second["result"], "allowed", "{second}");
    assert_eq!(second["written"], "flash_erase\n".len(), "{second}");

    let got = read_n_bytes(master, "flash_erase\n".len(), Duration::from_secs(2)).await;
    assert_eq!(got, b"flash_erase\n");

    // --- Full traceability, per T4.4/T4.3: both decisions and the eventual
    // successful write are all recoverable from the one event stream.
    let gates = gate_records(&recorder);
    let (request_records, deny_records, approve_records): (Vec<_>, Vec<_>, Vec<_>) = {
        let mut req = Vec::new();
        let mut deny = Vec::new();
        let mut appr = Vec::new();
        for g in &gates {
            if let Record::Gate { action, .. } = g {
                match action.as_str() {
                    "request" => req.push(g.clone()),
                    "deny" => deny.push(g.clone()),
                    "approve" => appr.push(g.clone()),
                    _ => {}
                }
            }
        }
        (req, deny, appr)
    };
    assert_eq!(
        request_records.len(),
        2,
        "expected two separate gate requests (one per attempt): {gates:?}"
    );
    assert_eq!(
        deny_records.len(),
        1,
        "expected exactly one deny (the timeout): {gates:?}"
    );
    assert_eq!(
        approve_records.len(),
        1,
        "expected exactly one approve (the human decision): {gates:?}"
    );
    match &deny_records[0] {
        Record::Gate { reason, .. } => assert_eq!(reason, "timeout_1s"),
        _ => unreachable!(),
    }
    match &approve_records[0] {
        Record::Gate { reason, .. } => assert!(
            reason.starts_with("approved_by:"),
            "decision-maker recoverable from the approve record: {reason:?}"
        ),
        _ => unreachable!(),
    }

    // The final tx record (the only one that ever executes) is tagged
    // `approved_by`, matching the same `approved_id` a human just decided.
    let txs = tx_records(&recorder);
    assert_eq!(txs.len(), 1, "{txs:?}");
    match &txs[0] {
        Record::Tx { gate, client, .. } => {
            assert!(gate.starts_with("approved_by:"), "{gate}");
            // The MCP bridge self-identifies as "serialwrap-mcp" over its
            // daemon connection (see `mcp::tools::ToolRegistry::connected_daemon`)
            // -- distinct from whichever agent host/model is driving it.
            assert!(client.starts_with("serialwrap-mcp:"), "{client}");
        }
        other => panic!("expected Tx, got {other:?}"),
    }
    println!(
        "acceptance #6 (T4.4, S4) — flash_erase: denied by timeout (id irrelevant to caller), \
         then approved on retry (approval id {approved_id}), both decisions traceable"
    );

    mcp.shutdown().await;
}

// ---- T4.4 acceptance criterion 7: set_config takes effect and is logged ----

#[tokio::test]
async fn agent_changes_baud_and_it_takes_effect_and_is_logged() {
    let (daemon, recorder, _master) =
        start_daemon_with_gate_and_pty("dev", short_timeout_gate(Duration::from_secs(5))).await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let result = mcp
        .call_tool("set_config", json!({"device": "dev", "baud": 74880}))
        .await;
    assert_eq!(result["result"], "allowed", "{result}");
    assert_eq!(result["config"]["baud"], 74880, "{result}");

    // Immediately verifiable by the same agent, via the read-only
    // `get_config` tool -- exactly the "agent doubts the baud, checks its
    // own hypothesis" flow this feature exists for.
    let config = mcp.call_tool("get_config", json!({"device": "dev"})).await;
    assert_eq!(config["config"]["baud"], 74880, "{config}");

    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let config_change = records
        .iter()
        .find_map(|r| match r {
            Record::Event { event, extra, .. } if event == "config_change" => Some(extra.clone()),
            _ => None,
        })
        .expect("expected a config_change event");
    assert_eq!(
        config_change
            .get("new")
            .and_then(|v| v.get("baud"))
            .and_then(|v| v.as_u64()),
        Some(74880)
    );
    println!("acceptance #7 (T4.4) — agent's baud change took effect and was logged");

    mcp.shutdown().await;
}

// ---- T4.4 acceptance criterion 8: dtr_pulse requires approval ----
//
// The full gated/approved/denied/bypassed behavior behind `dtr_pulse` is
// already proven at the protocol level, against every permission class, in
// `serialwrapd/tests/write_gate.rs` (the daemon-side mechanism the MCP tool
// calls into verbatim -- `mcp::tools::ToolRegistry::dtr_pulse` does no
// translation beyond building the same `Request::DtrPulse` and reading the
// same `write_denied`/success reply shape `write`'s tool already handles,
// both covered by `mcp::tools`'s own unit tests, e.g.
// `dtr_pulse_requires_duration_ms`/`denied_result_carries_reason_and_matched_rule`).
// A dedicated MCP-subprocess-level reproduction of the same scenario would
// add real wall-clock cost (spawning `serialwrap mcp` plus polling
// `serialwrap approvals` via repeated subprocess spawns) without covering
// any code path the above two layers don't already exercise, so it's left
// out here to keep this file's contribution to `cargo test --all`'s ~10s
// budget (T4.3/T4.4 acceptance criterion 11) proportionate to what it
// actually adds.
