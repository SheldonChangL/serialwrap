//! CLI-level integration tests for `serialwrap write`/`config`/`clients`
//! (issue #8 / #10, `TASKS.md` T2.1/T2.3's acceptance criteria).
//!
//! Same discipline `tail_cli.rs` already established: every test here
//! drives the *actual compiled* `serialwrap` binary as a subprocess
//! against a real UDS protocol server backed by [`TestBackend`] — never by
//! calling `cli::*` functions in-process. The precise, byte-exact-per-
//! line-ending and protocol-identity assertions already live in
//! `serialwrapd/tests/protocol.rs` (which can drive the raw wire protocol
//! directly, including non-`human` client types the CLI never sends);
//! these tests instead prove the *CLI* itself — argument parsing, device
//! resolution, `--hex`/stdin handling, and rendered output — end to end.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
use tokio::process::Command;

use serialwrapd::gate::rules::RuleSet;
use serialwrapd::gate::Gate;
use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
}

fn open_raw_pty_pair() -> (File, File) {
    let pair = openpty(None, None).expect("openpty");
    let mut attrs = tcgetattr(&pair.slave).expect("tcgetattr");
    cfmakeraw(&mut attrs);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &attrs).expect("tcsetattr");
    (File::from(pair.master), File::from(pair.slave))
}

/// Stand up a daemon with one `TestBackend` device, plus a fresh raw PTY
/// pair registered as that device's writer. Returns the daemon, the data
/// tempdir (must outlive the test — see `serialwrapd/tests/protocol.rs`'s
/// `start_daemon_with_writable_device` doc comment for exactly why), and
/// the PTY master to read exact bytes back from.
async fn start_daemon_with_writable_device(
    device_id: &str,
) -> (TestDaemon, tempfile::TempDir, File) {
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

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(
        backend as Arc<dyn DeviceBackend>,
        "test",
        dir.path(),
    ));
    tokio::spawn(server::serve(listener, shared));

    (
        TestDaemon {
            socket_path: path,
            _dir: dir,
        },
        tmp_data,
        master,
    )
}

// ---- T4.3 (issue #16) `serialwrap audit` test helpers ----

fn short_timeout_gate(timeout: Duration) -> Gate {
    let mut rules = RuleSet::builtin();
    rules.timeout = timeout;
    Gate::new(rules, Arc::new(serialwrapd::gate::notify::DesktopNotifier))
}

/// Same shape as [`start_daemon_with_writable_device`], but with a
/// caller-supplied [`Gate`] (a short approval timeout, for tests that need
/// a force-pended write to actually resolve) and returning the `Recorder`
/// handle directly rather than the tempdir a caller would otherwise have to
/// keep alive separately.
async fn start_daemon_with_writable_device_and_gate(
    device_id: &str,
    gate: Gate,
) -> (TestDaemon, Arc<Recorder>, File) {
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
    // it); leaked rather than threaded through the return tuple, matching
    // `mcp_bridge.rs`'s own `start_daemon_with_empty_device` convention.
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

/// Minimal hand-rolled protocol client — same shape `write_gate.rs` uses —
/// needed here because the CLI's own `write` subcommand always connects as
/// `human`; only a raw connection can submit an `agent` write to exercise
/// the gated/denied path `serialwrap audit` needs to have something to
/// show.
struct AuditTestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl AuditTestClient {
    async fn connect(path: &Path, name: &str, client_type: &str) -> Self {
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
        client.recv().await;
        client
    }

    async fn send(&mut self, request: Value) {
        let line = format!("{request}\n");
        self.writer
            .write_all(line.as_bytes())
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
}

/// Submit `text` as an `agent` write and wait for its (denied-by-timeout)
/// reply — used to populate the audit trail with a force-pended, denied
/// write's `write_request`/`gate` records, with no `tx` record at all.
async fn agent_write_denied_by_timeout(socket: &Path, device: &str, text: &str) {
    let mut agent = AuditTestClient::connect(socket, "claude-code", "agent").await;
    agent
        .send(
            json!({"id": 1, "op": "write", "device": device, "text": text, "line_ending": "none"}),
        )
        .await;
    let reply = agent.recv().await;
    assert_eq!(
        reply["ok"], false,
        "expected the write to be denied: {reply}"
    );
    assert_eq!(reply["error"]["code"], "write_denied", "{reply}");
}

fn find_gate_deny_seq(recorder: &Recorder) -> u64 {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .find_map(|r| match r {
            Record::Gate { seq, action, .. } if action == "deny" => Some(seq),
            _ => None,
        })
        .expect("expected a gate deny record")
}

async fn start_plain_daemon(device_id: &str) -> (TestDaemon, tempfile::TempDir) {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), device_id, RecorderConfig::default())
            .expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId(device_id.to_string()), recorder);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(
        backend as Arc<dyn DeviceBackend>,
        "test",
        dir.path(),
    ));
    tokio::spawn(server::serve(listener, shared));

    (
        TestDaemon {
            socket_path: path,
            _dir: dir,
        },
        tmp_data,
    )
}

fn cli(socket: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_serialwrap"));
    cmd.env("SERIALWRAP_SOCKET", socket);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn read_n_bytes(file: File, n: usize, timeout: Duration) -> Vec<u8> {
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

// ---- T2.1 acceptance criteria, via the actual compiled `write` subcommand ----

#[tokio::test]
async fn write_cli_sends_text_with_the_default_lf_line_ending() {
    let (daemon, _datadir, master) = start_daemon_with_writable_device("dev").await;

    let output = cli(&daemon.socket_path, &["write", "dev", "hello"])
        .output()
        .await
        .expect("run write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert!(
        stdout_text(&output).contains("wrote 6 bytes"),
        "stdout: {}",
        stdout_text(&output)
    );

    let got = read_n_bytes(master, 6, Duration::from_secs(2)).await;
    assert_eq!(got, b"hello\n");
}

#[tokio::test]
async fn write_cli_line_ending_flag_selects_crlf() {
    let (daemon, _datadir, master) = start_daemon_with_writable_device("dev").await;

    let output = cli(&daemon.socket_path, &["write", "dev", "PING", "-e", "crlf"])
        .output()
        .await
        .expect("run write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    let got = read_n_bytes(master, 6, Duration::from_secs(2)).await;
    assert_eq!(got, b"PING\r\n");
}

#[tokio::test]
async fn write_cli_hex_flag_sends_exact_bytes() {
    let (daemon, _datadir, master) = start_daemon_with_writable_device("dev").await;

    let output = cli(
        &daemon.socket_path,
        &["write", "dev", "--hex", "DE AD BE EF"],
    )
    .output()
    .await
    .expect("run write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert!(
        stdout_text(&output).contains("wrote 4 bytes"),
        "stdout: {}",
        stdout_text(&output)
    );

    let got = read_n_bytes(master, 4, Duration::from_secs(2)).await;
    assert_eq!(got, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[tokio::test]
async fn write_cli_reads_the_payload_from_stdin_when_no_text_is_given() {
    let (daemon, _datadir, master) = start_daemon_with_writable_device("dev").await;

    let mut child = cli(&daemon.socket_path, &["write", "dev"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn write");
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(b"ping\n").await.expect("write stdin");
        // Drop closes stdin, signaling EOF.
    }
    let output = child.wait_with_output().await.expect("wait for write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    // Exactly one trailing newline from the piped input is stripped, then
    // the default `-e lf` re-appends it — net effect: still "ping\n".
    let got = read_n_bytes(master, 5, Duration::from_secs(2)).await;
    assert_eq!(got, b"ping\n");
}

#[tokio::test]
async fn write_cli_reports_an_actionable_error_when_the_daemon_is_not_running() {
    let socket_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = socket_dir.path().join("nothing-here.sock");

    let output = cli(&socket_path, &["write", "dev", "hello"])
        .output()
        .await
        .expect("run write");
    assert!(!output.status.success());
    let stderr = stderr_text(&output);
    assert!(stderr.contains("serialwrap daemon"), "stderr: {stderr}");
    assert!(stderr.contains("isn't running"), "stderr: {stderr}");
}

// ---- T2.3 acceptance criteria, via the actual compiled `config`/`clients` subcommands ----

#[tokio::test]
async fn config_cli_read_shows_defaults_and_unavailable_error_counts() {
    let (daemon, _datadir) = start_plain_daemon("dev").await;

    let output = cli(&daemon.socket_path, &["config", "dev"])
        .output()
        .await
        .expect("run config");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);
    assert!(text.contains("baud=9600"), "text: {text}");
    assert!(text.contains("error counts: unavailable"), "text: {text}");
}

#[tokio::test]
async fn config_cli_write_updates_baud_and_prints_the_new_config() {
    let (daemon, _datadir) = start_plain_daemon("dev").await;

    let output = cli(
        &daemon.socket_path,
        &[
            "config", "dev", "--baud", "74880", "--parity", "none", "--data", "8", "--stop", "1",
            "--flow", "none",
        ],
    )
    .output()
    .await
    .expect("run config");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);
    assert!(text.contains("config updated"), "text: {text}");
    assert!(text.contains("baud=74880"), "text: {text}");

    // A follow-up read must show the change persisted.
    let output = cli(&daemon.socket_path, &["config", "dev"])
        .output()
        .await
        .expect("run config (read-back)");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert!(
        stdout_text(&output).contains("baud=74880"),
        "text: {}",
        stdout_text(&output)
    );
}

#[tokio::test]
async fn clients_cli_lists_a_connected_client_with_verified_pid_and_type() {
    let (daemon, _datadir) = start_plain_daemon("dev").await;

    // A long-lived `tail -f` process gives something for `clients` to see.
    let mut tail_child = cli(&daemon.socket_path, &["tail", "-f", "dev"])
        .spawn()
        .expect("spawn tail -f");
    let tail_pid = tail_child.id().expect("tail child has a pid");

    // Poll `clients` until the tail process actually shows up (it connects
    // asynchronously; no fixed sleep).
    let deadline = Instant::now() + Duration::from_secs(5);
    let listing = loop {
        let output = cli(&daemon.socket_path, &["clients"])
            .output()
            .await
            .expect("run clients");
        assert!(output.status.success(), "stderr: {}", stderr_text(&output));
        let listing = stdout_text(&output);
        if listing.contains("serialwrap-tail") {
            break listing;
        }
        assert!(
            Instant::now() < deadline,
            "tail -f never appeared in `clients`: {listing}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let tail_line = listing
        .lines()
        .find(|l| l.contains("serialwrap-tail"))
        .unwrap_or_else(|| panic!("expected a serialwrap-tail line in: {listing}"));
    assert!(
        tail_line.contains(&format!("pid={tail_pid}")),
        "clients listing must show the kernel-verified pid ({tail_pid}): {tail_line}"
    );
    assert!(tail_line.contains("human"), "{tail_line}");
    assert!(tail_line.contains("read+write"), "{tail_line}");

    let _ = tail_child.start_kill();
    let _ = tail_child.wait().await;
}

#[tokio::test]
async fn clients_cli_kick_closes_the_targets_connection() {
    let (daemon, _datadir) = start_plain_daemon("dev").await;

    let tail_child = cli(&daemon.socket_path, &["tail", "-f", "dev"])
        .spawn()
        .expect("spawn tail -f");

    // Find the tail client's daemon-assigned client_id.
    let deadline = Instant::now() + Duration::from_secs(5);
    let target_id = loop {
        let output = cli(&daemon.socket_path, &["clients"])
            .output()
            .await
            .expect("run clients");
        let listing = stdout_text(&output);
        if let Some(line) = listing.lines().find(|l| l.contains("serialwrap-tail")) {
            let id: u64 = line
                .split('\t')
                .next()
                .expect("client_id column")
                .parse()
                .expect("client_id must be numeric");
            break id;
        }
        assert!(
            Instant::now() < deadline,
            "tail -f never appeared in `clients`"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let kick_output = cli(
        &daemon.socket_path,
        &["clients", "kick", &target_id.to_string()],
    )
    .output()
    .await
    .expect("run clients kick");
    assert!(
        kick_output.status.success(),
        "stderr: {}",
        stderr_text(&kick_output)
    );
    assert!(stdout_text(&kick_output).contains("kicked"));

    // The kicked `tail -f` sees its connection closed out from under it —
    // unlike a deliberate Ctrl-C, this is `tail.rs`'s `follow` loop getting
    // an `UnexpectedEof` from `read_push`, which `cli::dispatch` correctly
    // surfaces as a non-zero exit plus an actionable stderr message (a
    // silent exit-0 here would hide "your monitoring session just got
    // kicked" from whoever depends on it).
    let output = tokio::time::timeout(Duration::from_secs(5), tail_child.wait_with_output())
        .await
        .expect("kicked tail -f must exit within 5s")
        .expect("wait on child");
    assert!(
        !output.status.success(),
        "kicked tail -f must exit non-zero (its connection was closed out from under it): {:?}",
        output.status
    );
    assert!(
        stderr_text(&output).contains("closed"),
        "stderr should say the connection was closed: {}",
        stderr_text(&output)
    );
}

// ---- T4.3 (issue #16) `serialwrap audit` acceptance criteria ----
//
// Criteria 1/2 (full traceability + denied payload), 3 (export format
// parity), and the `--actor` filter all share one setup (one allowed write,
// one denied write) -- combined into a single test so that setup (a fresh
// daemon, a PTY pair, one 200ms-gated deny) is paid for once instead of
// three times, keeping this file's contribution to `cargo test --all`'s
// ~10s budget (T4.3/T4.4 acceptance criterion 11) proportionate. Criterion
// 4 (±N context) needs its own distinct rx-line arrangement and stays
// separate below.

#[tokio::test]
async fn audit_listing_shows_full_traceability_matches_export_format_and_filters_by_actor() {
    // A short (200ms) gate timeout -- unlike `mcp_bridge.rs`'s S4 scenario,
    // this test doesn't need to assert the exact `timeout_<n>s` numeral
    // (only that a `deny` decision naming a timeout reason is recoverable
    // at all), so there's no reason to pay a full second of real wall time
    // just to let it fire.
    let (daemon, recorder, _master) = start_daemon_with_writable_device_and_gate(
        "dev",
        short_timeout_gate(Duration::from_millis(200)),
    )
    .await;

    // Allowed: a plain human write, via the real `serialwrap write` CLI.
    let output = cli(&daemon.socket_path, &["write", "dev", "ok"])
        .output()
        .await
        .expect("run write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    // Denied: an agent's `flash_erase`, force-pended by the built-in danger
    // rule, then denied by the fail-safe timeout.
    agent_write_denied_by_timeout(&daemon.socket_path, "dev", "flash_erase").await;

    // ---- T4.3 acceptance criteria 1 & 2: full traceability + denied payload ----

    let output = cli(&daemon.socket_path, &["audit", "dev"])
        .output()
        .await
        .expect("run audit");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);

    // The allowed write: requester, decision path, and bytes all visible.
    assert!(text.contains("tx client=serialwrap-write"), "text: {text}");
    assert!(text.contains("gate=human_rw"), "text: {text}");
    // The written bytes are fully recoverable -- `cli::render`'s existing
    // tx rendering treats the trailing `\n` `write`'s default line ending
    // appends as a control byte, so a short payload like this renders as a
    // `[N bytes binary — hex]` summary rather than plain text (same
    // behavior `tail` already has for a tx echo) -- the exact hex is still
    // the complete, unambiguous original bytes, not truncated.
    assert!(text.contains("6f 6b 0a"), "text: {text}");

    // The denied write: full traceability even though it never produced a
    // `tx` record -- requester identity, matched rule, and the *complete*
    // original payload (base64) must all still be recoverable.
    assert!(text.contains("requester_name=claude-code"), "text: {text}");
    assert!(text.contains("matched_rule=danger:erase"), "text: {text}");
    let expected_b64 = BASE64.encode(b"flash_erase");
    assert!(
        text.contains(&format!("bytes_b64={expected_b64}")),
        "the denied request's full original payload must be recoverable: text={text}"
    );
    assert!(
        text.contains("action=deny") && text.contains("timeout_"),
        "decision-maker (fail-safe timeout) must be recoverable: text={text}"
    );

    // Every printed row also carries its own seq -- "the record's own seq
    // *is* the log offset", no separate join needed.
    assert!(text.contains("seq="), "text: {text}");
    println!("T4.3 #1/#2 — audit listing: {text}");

    // ---- T4.3 acceptance criterion 3: export format matches `export` byte-for-byte ----

    let audit_output = cli(&daemon.socket_path, &["audit", "dev", "--export", "jsonl"])
        .output()
        .await
        .expect("run audit --export jsonl");
    assert!(
        audit_output.status.success(),
        "stderr: {}",
        stderr_text(&audit_output)
    );
    let audit_text = stdout_text(&audit_output);
    assert!(!audit_text.trim().is_empty(), "expected some audit records");

    let export_output = cli(&daemon.socket_path, &["export", "dev", "--format", "jsonl"])
        .output()
        .await
        .expect("run export --format jsonl");
    assert!(
        export_output.status.success(),
        "stderr: {}",
        stderr_text(&export_output)
    );
    let export_bytes = export_output.stdout;
    let export_text = String::from_utf8_lossy(&export_bytes);

    let mut audit_line_count = 0;
    for line in audit_text.lines() {
        if line.is_empty() {
            continue;
        }
        audit_line_count += 1;
        // Byte-for-byte identical to a line in `export`'s own output --
        // this is the *same* daemon-side renderer, not a re-serialization,
        // so this must be an exact substring match, never merely "close".
        assert!(
            export_text.contains(line),
            "audit --export jsonl line not found verbatim in export's own output:\n{line}\n\n\
             export output was:\n{export_text}"
        );
        // And every exported audit line must actually decode as a Tx/Gate/
        // audit-relevant Event -- never a bare `rx` line, which `export`
        // includes but audit must not.
        let record: Record = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("audit --export jsonl line isn't valid Record json: {e}: {line}")
        });
        assert!(
            !matches!(record, Record::Rx { .. }),
            "audit --export jsonl must never include a plain rx record: {line}"
        );
    }
    assert!(
        audit_line_count >= 2,
        "expected at least tx + gate/event rows, got {audit_line_count}"
    );
    println!("T4.3 #3 — {audit_line_count} audit jsonl lines all verified verbatim within export's own output");

    // ---- --actor filter ----

    let output = cli(
        &daemon.socket_path,
        &["audit", "dev", "--actor", "claude-code"],
    )
    .output()
    .await
    .expect("run audit --actor claude-code");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);
    assert!(text.contains("claude-code"), "text: {text}");
    assert!(
        !text.contains("serialwrap-write"),
        "the human write's tx record must be filtered out by --actor claude-code: {text}"
    );

    let output = cli(
        &daemon.socket_path,
        &["audit", "dev", "--actor", "serialwrap-write"],
    )
    .output()
    .await
    .expect("run audit --actor serialwrap-write");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);
    assert!(text.contains("serialwrap-write"), "text: {text}");
    assert!(
        !text.contains("claude-code"),
        "the agent's write_request event must be filtered out by --actor serialwrap-write: {text}"
    );
    println!("--actor filter narrows correctly in both directions");

    let _ = recorder; // kept alive only so its recorder's device dir/lock survive.
}

// ---- T4.3 acceptance criterion 4: ±N context around an arbitrary seq ----

#[tokio::test]
async fn audit_context_shows_the_surrounding_lines_around_a_gate_decision() {
    let (daemon, recorder, _master) = start_daemon_with_writable_device_and_gate(
        "dev",
        short_timeout_gate(Duration::from_millis(200)),
    )
    .await;

    recorder.append_rx(b"line-1\n").unwrap();
    recorder.append_rx(b"line-2\n").unwrap();
    agent_write_denied_by_timeout(&daemon.socket_path, "dev", "flash_erase").await;
    recorder.append_rx(b"line-3\n").unwrap();
    recorder.append_rx(b"line-4\n").unwrap();
    recorder.append_rx(b"line-5\n").unwrap();
    recorder.append_rx(b"line-6\n").unwrap();

    let target_seq = find_gate_deny_seq(&recorder);

    // `serialwrap audit --context` reads via `Request::ReadSince`, which is
    // served from this device's shared, daemon-side `DeviceQueryState` --
    // already created (and ingested once synchronously) earlier by the
    // `flash_erase` write's own log-context fetch, so this is *not* this
    // device's first-ever query (which would get a fresh synchronous
    // ingest -- see `registry::QueryRegistry::get_or_spawn`'s docs).
    // `line-3`..`line-6`, appended after that first query, only become
    // visible once the background poller (`query::DEFAULT_POLL_INTERVAL`,
    // 5ms) next ticks -- so poll `audit --context` itself until it reflects
    // them, rather than assuming one call already raced ahead of the
    // poller. Deadline exists purely to turn a genuine regression into a
    // clear failure, not as a guess at how long the poller normally takes.
    let deadline = Instant::now() + Duration::from_secs(5);
    let text = loop {
        let output = cli(
            &daemon.socket_path,
            &[
                "audit",
                "dev",
                "--context",
                &target_seq.to_string(),
                "--lines",
                "2",
            ],
        )
        .output()
        .await
        .expect("run audit --context");
        assert!(output.status.success(), "stderr: {}", stderr_text(&output));
        let text = stdout_text(&output);
        if text.contains("line-4") {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "audit --context never reflected line-4 within 5s (last output: {text:?})"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    // Within the ±2-record window around the deny decision.
    assert!(text.contains("line-3"), "text: {text}");
    assert!(text.contains("line-4"), "text: {text}");
    assert!(text.contains(&format!("seq={target_seq}")), "text: {text}");
    assert!(
        text.contains(">>"),
        "expected the target row to be marked: {text}"
    );

    // Well outside the window on both ends.
    assert!(!text.contains("line-1"), "text: {text}");
    assert!(!text.contains("line-5"), "text: {text}");
    assert!(!text.contains("line-6"), "text: {text}");
    println!("T4.3 #4 — audit --context window: {text}");
}
