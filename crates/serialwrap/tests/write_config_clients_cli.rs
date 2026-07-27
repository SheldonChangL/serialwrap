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

use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use tokio::process::Command;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};

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
    let shared = Arc::new(Shared::new(backend as Arc<dyn DeviceBackend>, "test"));
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
    let shared = Arc::new(Shared::new(backend as Arc<dyn DeviceBackend>, "test"));
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
