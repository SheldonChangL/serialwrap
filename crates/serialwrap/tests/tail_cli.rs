//! CLI-level integration tests for `serialwrap devices`/`tail` (issue #7 /
//! `TASKS.md` T1.5's acceptance criteria).
//!
//! Every test here drives the *actual compiled* `serialwrap` binary as a
//! subprocess (`env!("CARGO_BIN_EXE_serialwrap")`) against a real UDS
//! protocol server — never by calling `cli::*` functions in-process. This
//! mirrors the discipline `crates/serialwrapd/tests/protocol.rs` already
//! established for the protocol layer itself ("drive it exactly the way a
//! real client would"), just one level up: this crate's own tests must
//! prove the *CLI*, not merely the library code behind it.
//!
//! Most tests stand up a daemon with [`TestBackend`] (a plain in-memory
//! device registry) rather than the real `HotplugDetector` — sanctioned by
//! `serialwrapd::protocol::backend`'s own module docs, since hotplug
//! detection itself is already covered by `serialwrapd`'s own
//! `port_hotplug.rs`. The one exception is the S1 scenario test below,
//! which specifically needs the real detector to be a faithful
//! reproduction of that exit scenario.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use mock_device::{script, MockDevice};
use serialwrapd::port::testing::ScriptedEnumerator;
use serialwrapd::port::{DeviceId, EnumeratedDevice, HotplugConfig, HotplugDetector, UsbMetadata};
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend, LiveBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// A running test daemon: the socket clients should connect to, plus
/// whatever keeps it alive for the test's lifetime (the socket's own
/// tempdir — dropping it removes the socket file, but the daemon task
/// keeps running against the already-bound listener regardless).
struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
}

/// Stand up a daemon (a real `serialwrapd::protocol` UDS server) with one
/// device already registered against `recorder`, via [`TestBackend`].
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

/// Build (but don't yet spawn) an invocation of the compiled `serialwrap`
/// binary, pointed at `socket` via `SERIALWRAP_SOCKET`.
fn cli(socket: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_serialwrap"));
    cmd.env("SERIALWRAP_SOCKET", socket);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Continuously drain `reader`'s lines into a shared buffer on a
/// background task, returning that buffer plus the task handle. This is
/// what lets a `-f` test wait for *actual evidence* a client has made
/// progress (e.g. "it already printed its initial history") instead of
/// guessing a fixed sleep is long enough — the latter is exactly what
/// makes a test flaky on a CI machine running many other tests'
/// subprocesses at the same time (see this file's `-f` tests, which used
/// to rely on fixed sleeps and were observed to fail under full-workspace
/// `cargo test --all` load even though they passed reliably in isolation).
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

/// A spawned `serialwrap` CLI process whose stdout/stderr are
/// continuously drained in the background, so a test can wait for a
/// specific line to actually appear — real synchronization against the
/// process's own progress — rather than a wall-clock guess.
struct RunningTail {
    child: Child,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

fn spawn_tail(socket: &Path, args: &[&str]) -> RunningTail {
    let mut child = cli(socket, args).spawn().expect("spawn cli");
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let (stdout, stdout_task) = spawn_line_collector(stdout_pipe);
    let (stderr, stderr_task) = spawn_line_collector(stderr_pipe);
    RunningTail {
        child,
        stdout,
        stderr,
        stdout_task,
        stderr_task,
    }
}

impl RunningTail {
    /// Block until some collected stdout line contains `needle`, or panic
    /// after `timeout` — the event-driven replacement for "sleep and hope
    /// it's ready by then".
    async fn wait_until_stdout_contains(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .stdout
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains(needle))
            {
                return;
            }
            if Instant::now() >= deadline {
                let so_far = self.stdout.lock().unwrap().clone();
                panic!(
                    "timed out after {timeout:?} waiting for stdout to contain {needle:?}; \
                     collected so far: {so_far:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Send SIGINT, wait for the process and both line-collector tasks to
    /// finish (guaranteeing every line it ever printed has been drained),
    /// and return its exit status plus everything it printed.
    async fn sigint_and_join(mut self) -> (std::process::ExitStatus, String, String) {
        kill(
            Pid::from_raw(self.child.id().expect("child has a pid") as i32),
            Signal::SIGINT,
        )
        .expect("send SIGINT");
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("child exits within 5s of SIGINT")
            .expect("wait on child");
        let _ = tokio::time::timeout(Duration::from_secs(2), self.stdout_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.stderr_task).await;
        let stdout = self.stdout.lock().unwrap().join("\n");
        let stderr = self.stderr.lock().unwrap().join("\n");
        (status, stdout, stderr)
    }
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---- Acceptance criterion 1: two concurrent `tail -f` clients agree ----

#[tokio::test]
async fn two_concurrent_tail_f_clients_produce_identical_output() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), "dev", RecorderConfig::default()).expect("open recorder"),
    );
    recorder.append_rx(b"boot ok\n").expect("append boot line");
    let daemon = start_daemon_with_device("dev", Arc::clone(&recorder)).await;

    let c1 = spawn_tail(&daemon.socket_path, &["tail", "-f", "-n", "5", "dev"]);
    let c2 = spawn_tail(&daemon.socket_path, &["tail", "-f", "-n", "5", "dev"]);

    // Wait for *actual evidence* both clients have already printed their
    // initial `-n 5` history before appending anything else — this is
    // what makes "which of the two clients' code paths (initial history
    // vs. the live `subscribe` push that follows it) picks up a given
    // line" deterministic, rather than assuming a fixed sleep is long
    // enough (which was observed to be flaky under a fully loaded
    // `cargo test --all` run).
    let history_timeout = Duration::from_secs(5);
    c1.wait_until_stdout_contains("boot ok", history_timeout)
        .await;
    c2.wait_until_stdout_contains("boot ok", history_timeout)
        .await;

    for i in 0..5 {
        recorder
            .append_rx(format!("line-{i}\n").as_bytes())
            .expect("append line");
    }
    recorder
        .append_event("config_change", serde_json::Map::new())
        .expect("append event");

    let follow_timeout = Duration::from_secs(5);
    c1.wait_until_stdout_contains("config_change", follow_timeout)
        .await;
    c2.wait_until_stdout_contains("config_change", follow_timeout)
        .await;

    let (status1, text1, err1) = c1.sigint_and_join().await;
    let (status2, text2, err2) = c2.sigint_and_join().await;
    assert!(status1.success(), "client 1 exit status: {status1:?}");
    assert!(status2.success(), "client 2 exit status: {status2:?}");
    assert!(err1.is_empty(), "client 1 stderr: {err1}");
    assert!(err2.is_empty(), "client 2 stderr: {err2}");

    for needle in [
        "boot ok", "line-0", "line-1", "line-2", "line-3", "line-4", "# ",
    ] {
        assert!(
            text1.contains(needle),
            "client 1 missing {needle:?}:\n{text1}"
        );
        assert!(
            text2.contains(needle),
            "client 2 missing {needle:?}:\n{text2}"
        );
    }

    // Both clients read from the same underlying assembled records (the
    // daemon computes each line's timestamp once, server-side — see
    // `cli::render`'s docs on why `t_wall` is reused rather than
    // per-client receive time), so their full rendered output must be
    // byte-for-byte identical, not merely "contains the same substrings".
    assert_eq!(
        text1, text2,
        "two concurrent `tail -f` clients against the same device diverged"
    );
    println!(
        "acceptance #1 — two concurrent `tail -f` clients, comparison result: IDENTICAL \
         ({} lines each)\n--- client 1 ---\n{text1}\n--- client 2 ---\n{text2}",
        text1.lines().count()
    );
}

// ---- Acceptance criterion 2: Ctrl-C exits cleanly, daemon/other clients unaffected ----

#[tokio::test]
async fn ctrl_c_exits_cleanly_without_affecting_daemon_or_other_client() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), "dev", RecorderConfig::default()).expect("open recorder"),
    );
    // A guaranteed first line, so both clients' `-n 1` history has
    // something to print — used below as real evidence that each client
    // has already connected and is about to enter its follow loop, rather
    // than assuming a fixed sleep is long enough.
    recorder.append_rx(b"ready\n").expect("append ready line");
    let daemon = start_daemon_with_device("dev", Arc::clone(&recorder)).await;

    let victim = spawn_tail(&daemon.socket_path, &["tail", "-f", "-n", "1", "dev"]);
    let survivor = spawn_tail(&daemon.socket_path, &["tail", "-f", "-n", "1", "dev"]);

    let ready_timeout = Duration::from_secs(5);
    victim
        .wait_until_stdout_contains("ready", ready_timeout)
        .await;
    survivor
        .wait_until_stdout_contains("ready", ready_timeout)
        .await;

    let (victim_status, _victim_stdout, victim_stderr) = victim.sigint_and_join().await;
    assert!(
        victim_status.success(),
        "Ctrl-C'd client should exit with status 0, got {victim_status:?} (stderr: {victim_stderr})"
    );

    // The daemon itself must still be alive: a brand-new client can still
    // connect and list devices.
    let devices_check = cli(&daemon.socket_path, &["devices"])
        .output()
        .await
        .expect("run `devices` after the victim's Ctrl-C");
    assert!(
        devices_check.status.success(),
        "daemon did not survive the other client's Ctrl-C: {}",
        stderr_text(&devices_check)
    );
    let devices_text = stdout_text(&devices_check);
    assert!(
        devices_text.contains("dev"),
        "daemon lost its device after the other client's Ctrl-C: {devices_text}"
    );

    // The surviving client must keep receiving data appended *after* the
    // victim was Ctrl-C'd.
    recorder
        .append_rx(b"still alive\n")
        .expect("append after the other client's Ctrl-C");
    survivor
        .wait_until_stdout_contains("still alive", Duration::from_secs(5))
        .await;

    let (_survivor_status, survivor_text, survivor_stderr) = survivor.sigint_and_join().await;
    assert!(
        survivor_stderr.is_empty(),
        "survivor stderr: {survivor_stderr}"
    );
    assert!(
        survivor_text.contains("still alive"),
        "surviving client stopped receiving data after the other client's Ctrl-C: {survivor_text}"
    );

    println!(
        "acceptance #2 — victim exit status: {victim_status:?}; daemon answered `devices` \
         afterward: {devices_text:?}; surviving client's output after victim's Ctrl-C:\n{survivor_text}"
    );
}

// ---- Acceptance criterion 3: exit scenario S1 ----

fn rx_bytes(recorder: &Recorder) -> Vec<u8> {
    let mut out = Vec::new();
    for record in recorder
        .read_since(0, usize::MAX)
        .expect("read_since")
        .records
    {
        if let Record::Rx { data_b64, .. } = record {
            out.extend(BASE64.decode(data_b64).expect("valid base64 data_b64"));
        }
    }
    out
}

#[tokio::test]
async fn s1_boot_banner_is_visible_via_plain_tail_with_no_client_present_during_boot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("CLI-S1-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        HotplugConfig {
            poll_interval: Duration::from_millis(10),
            recorder_config: RecorderConfig::default(),
        },
    );
    // Same wiring order as `serialwrapd::run()`'s own production path:
    // build the backend from the detector's accessors, *then* consume the
    // detector via `spawn()`.
    let backend = Arc::new(LiveBackend::new(
        detector.port_config_api(),
        detector.recorders(),
    ));
    let handle = detector.spawn();

    // The device "powers on" and prints its boot banner immediately,
    // racing the daemon's own hotplug detection on purpose — S1's literal
    // framing ("插入裝置，不做任何操作"). No client connects at any point
    // during this.
    let banner = script::boot_banner();
    let writer = {
        let banner = banner.clone();
        std::thread::spawn(move || {
            let result = device.write_device_output(&banner);
            // Keep the mock device (and its PTY master) alive a little
            // longer so the daemon's reader thread has time to actually
            // drain what was written before the fd closes — same
            // drain-barrier pattern (and rationale) as
            // `serialwrapd`'s own `port_hotplug.rs` S1 test.
            std::thread::sleep(Duration::from_millis(300));
            result
        })
    };

    // Wait until the banner is actually durably recorded — inspecting the
    // detector's own `Recorder` handle directly, which is not a wire
    // client and so doesn't violate "no client operation during boot".
    // Detection+open+first-read racing the banner write is inherently
    // non-deterministic in wall time, hence poll-until rather than a
    // fixed sleep (mirrors `port_hotplug.rs`'s own S1 test).
    let recorders = handle.recorders();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let has_it = {
            let guard = recorders.lock().unwrap();
            guard
                .get(&id)
                .map(|r| rx_bytes(r).len() >= banner.len())
                .unwrap_or(false)
        };
        if has_it {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "boot banner was never recorded within 5s"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    writer
        .join()
        .expect("writer thread panicked")
        .expect("write boot banner");

    // Only now — after the banner is already durably recorded, with no
    // client having connected at all up to this point — bind the protocol
    // server and run the compiled `tail` CLI against it.
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("test.sock");
    let listener = server::bind(&socket_path).expect("bind test socket");
    let shared = Arc::new(Shared::new(backend as Arc<dyn DeviceBackend>, "test"));
    tokio::spawn(server::serve(listener, shared));

    let output = cli(&socket_path, &["tail", "-n", "50", &id.0])
        .output()
        .await
        .expect("run tail");
    handle.stop();

    assert!(
        output.status.success(),
        "tail exited with {:?}: stderr={}",
        output.status,
        stderr_text(&output)
    );
    let text = stdout_text(&output);
    let banner_first_line = String::from_utf8_lossy(&banner).trim_end().to_string();
    assert!(
        text.contains(&banner_first_line),
        "boot banner's first line missing from `tail` output: {text:?}"
    );
    println!(
        "acceptance #3 — S1 scenario, `serialwrap tail -n 50 {}` output:\n{text}",
        id.0
    );
}

// ---- Acceptance criterion 4: event rows are `# `-prefixed, data rows are not ----

#[tokio::test]
async fn event_lines_are_hash_prefixed_and_data_lines_are_not() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), "dev", RecorderConfig::default()).expect("open recorder"),
    );
    recorder.append_rx(b"plain data line\n").expect("append rx");
    recorder
        .append_event("disconnect", serde_json::Map::new())
        .expect("append event");
    let daemon = start_daemon_with_device("dev", recorder).await;

    let output = cli(&daemon.socket_path, &["tail", "-n", "10", "dev"])
        .output()
        .await
        .expect("run tail");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);

    let data_line = text
        .lines()
        .find(|l| l.contains("plain data line"))
        .unwrap_or_else(|| panic!("data line missing from output: {text}"));
    let event_line = text
        .lines()
        .find(|l| l.contains("disconnect"))
        .unwrap_or_else(|| panic!("event line missing from output: {text}"));

    assert!(
        !data_line.starts_with('#'),
        "data line wrongly `#`-prefixed: {data_line:?}"
    );
    assert!(
        event_line.starts_with("# "),
        "event line missing its `# ` prefix: {event_line:?}"
    );
    println!(
        "acceptance #4 — data line: {data_line:?}\n                event line: {event_line:?}"
    );
}

// ---- Acceptance criterion 5: binary content never pollutes the terminal ----

#[tokio::test]
async fn binary_content_renders_as_length_plus_hex_never_raw_control_bytes() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), "dev", RecorderConfig::default()).expect("open recorder"),
    );
    // A "line" (terminated by `\n`, so it does get assembled) containing a
    // raw ANSI escape sequence plus non-UTF-8 bytes — exactly the kind of
    // device output that must never be dumped straight to a terminal.
    let mut payload = vec![0x1b, b'[', b'3', b'1', b'm'];
    payload.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01]);
    payload.push(b'\n');
    recorder.append_rx(&payload).expect("append binary payload");
    let daemon = start_daemon_with_device("dev", recorder).await;

    let output = cli(&daemon.socket_path, &["tail", "-n", "10", "dev"])
        .output()
        .await
        .expect("run tail");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    assert!(
        !output.stdout.contains(&0x1bu8),
        "a raw ESC byte leaked into terminal output: {:?}",
        output.stdout
    );
    let text = stdout_text(&output);
    assert!(
        text.contains("bytes binary"),
        "expected a length+hex binary summary: {text:?}"
    );
    assert!(
        text.chars().any(|c| c.is_ascii_hexdigit()),
        "expected a hex preview in the binary summary: {text:?}"
    );
    println!("acceptance #5 — binary rendering:\n{text}");
}

// ---- Issue #32 acceptance criterion 3: hex preview is the real device bytes ----

#[tokio::test]
async fn binary_hex_preview_matches_the_exact_bytes_written_not_the_lossy_text() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(data_dir.path(), "dev", RecorderConfig::default()).expect("open recorder"),
    );
    // Same shape as the criterion-5 test (ESC sequence + non-UTF-8 bytes),
    // but this test checks the *exact* hex value, not just "some hex
    // appeared" — the whole point of issue #32's fix.
    let mut payload = vec![0x1b, b'[', b'3', b'1', b'm'];
    payload.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01]);
    let mut with_newline = payload.clone();
    with_newline.push(b'\n');
    recorder
        .append_rx(&with_newline)
        .expect("append binary payload");
    let daemon = start_daemon_with_device("dev", recorder).await;

    let output = cli(&daemon.socket_path, &["tail", "-n", "10", "dev"])
        .output()
        .await
        .expect("run tail");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);

    let expected_hex = payload
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains(&expected_hex),
        "rendered hex must be the exact bytes written ({expected_hex:?}), got: {text:?}"
    );
    assert!(
        text.contains(&format!("{} bytes binary", payload.len())),
        "expected the exact byte count ({}), got: {text:?}",
        payload.len()
    );
    // The lossy U+FFFD replacement text must never leak into the CLI's
    // rendered output.
    assert!(!text.contains('\u{FFFD}'), "rendered was: {text:?}");
    println!("acceptance (issue #32) #3 — exact hex preview:\n{text}");
}

// ---- Acceptance criterion 6: actionable error when the daemon isn't running ----

#[tokio::test]
async fn tail_reports_an_actionable_message_when_the_daemon_is_not_running() {
    let socket_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = socket_dir.path().join("nothing-here.sock");
    // No daemon ever bound at this path.

    let output = cli(&socket_path, &["tail", "dev"])
        .output()
        .await
        .expect("run tail");
    assert!(
        !output.status.success(),
        "expected a non-zero exit when the daemon isn't running"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("serialwrap daemon"),
        "stderr should name the concrete fix: {stderr}"
    );
    assert!(
        stderr.contains("isn't running"),
        "stderr should say why it can't connect: {stderr}"
    );
    println!("acceptance #6 — actionable error message: {stderr}");
}
