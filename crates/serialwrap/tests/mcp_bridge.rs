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

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};

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
    let shared = Arc::new(Shared::new(backend as Arc<dyn DeviceBackend>, "test"));
    tokio::spawn(server::serve(listener, shared));
    TestDaemon {
        socket_path: path,
        _dir: dir,
    }
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

#[tokio::test]
async fn wait_for_then_tail_completes_with_no_sleep_in_the_agents_tool_trace() {
    let (daemon, recorder) = start_daemon_with_empty_device("dev").await;
    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    // Simulates the mock device's own boot latency -- this sleep lives in
    // a background task standing in for the *device's* timing, never in
    // the agent-facing call sequence below (wait_for -> tail), which is
    // the thing acceptance criterion 1 actually constrains.
    let writer_recorder = Arc::clone(&recorder);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
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
    tokio::time::sleep(Duration::from_millis(80)).await;

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
    tokio::time::sleep(Duration::from_millis(80)).await;

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
async fn tools_list_registers_five_read_tools_each_with_the_data_not_instruction_notice() {
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
            "wait_for"
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

    // The write-path tools must not be advertised yet (T3.1 is read-only
    // tools only -- see `tools::RESERVED_WRITE_TOOL_NAMES`).
    for reserved in ["write", "set_config", "dtr_pulse", "export"] {
        assert!(
            !names.contains(&reserved),
            "{reserved} must not be registered yet"
        );
    }
    println!("acceptance #5 — all 5 tool descriptions carry the data-not-instruction notice");

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
    tokio::time::sleep(Duration::from_millis(60)).await;

    let mut mcp = McpProcess::spawn(&daemon.socket_path);
    mcp.initialize().await;

    let tail_result = mcp
        .call_tool("tail", json!({"device": "dev", "n": 20}))
        .await;
    assert_eq!(tail_result["lines"].as_array().unwrap().len(), 5);
    let cursor = tail_result["cursor"].as_u64().expect("cursor");

    recorder.append_rx(b"line-5\n").expect("append rx");
    tokio::time::sleep(Duration::from_millis(60)).await;

    let read_since_result = mcp
        .call_tool("read_since", json!({"device": "dev", "cursor": cursor}))
        .await;
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
    tokio::time::sleep(Duration::from_millis(150)).await;

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
    tokio::time::sleep(Duration::from_millis(300)).await;

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
    tokio::time::sleep(Duration::from_millis(100)).await;

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
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
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
