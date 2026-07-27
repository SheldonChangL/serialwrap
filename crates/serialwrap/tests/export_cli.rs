//! CLI-level integration tests for `serialwrap export` (`TASKS.md` T2.4,
//! issue #11's acceptance criteria).
//!
//! Same discipline `tail_cli.rs`/`write_config_clients_cli.rs` already
//! established: every test here drives the *actual compiled* `serialwrap`
//! binary as a subprocess against a real UDS protocol server backed by
//! [`TestBackend`] and a real [`Recorder`] — never by calling `cli::*`
//! functions in-process. The core format/range/filter guarantees
//! (byte-exactness, round-trip losslessness, aged-out truncation, segment
//! boundaries, the S5 scenario) are unit-tested directly against
//! `serialwrapd::export::export_range` (see that module's own tests) since
//! that's where the logic actually lives; these tests instead prove the
//! *CLI* end to end: argument parsing/validation, `--boot`/`--last`
//! resolution, stdout-vs-file output, and the tty-safety refusal for `bin`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use nix::pty::openpty;
use serde_json::Map as JsonMap;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::{ClientType, Record};

struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
}

/// Stand up a daemon with one device already registered against `recorder`
/// — mirrors `tail_cli.rs`'s helper of the same shape.
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

/// A populated recorder with a representative mix of every record kind,
/// including one binary (invalid-UTF-8) `rx` chunk — the "S5" scenario the
/// wiki/`TASKS.md` names explicitly.
fn populated_recorder(dir: &std::path::Path) -> Recorder {
    let recorder = Recorder::open(dir, "dev", RecorderConfig::default()).expect("open recorder");
    recorder.append_rx(b"boot ok\n").unwrap();
    recorder.append_event("connect", JsonMap::new()).unwrap();
    let mut binary = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
    binary.push(b'\n');
    recorder.append_rx(&binary).unwrap();
    recorder
        .append_tx(b"cmd\n", "agent:1", ClientType::Agent, "whitelist")
        .unwrap();
    recorder.append_gate("allow", "whitelist_match", 3).unwrap();
    recorder.append_rx(b"after\n").unwrap();
    recorder
}

// ---- Acceptance 1: bin byte-exact, end to end through the CLI ----

#[tokio::test]
async fn bin_export_cli_writes_byte_exact_rx_only_content_to_a_file() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = populated_recorder(tmp_data.path());
    let all_records = recorder.read_since(0, usize::MAX).unwrap().records;
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("out.bin");

    let output = cli(
        &daemon.socket_path,
        &[
            "export",
            "dev",
            "--format",
            "bin",
            "-o",
            out_path.to_str().unwrap(),
        ],
    )
    .output()
    .await
    .expect("run export");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    let mut expected = Vec::new();
    for r in &all_records {
        if let Record::Rx { data_b64, .. } = r {
            expected.extend_from_slice(&BASE64.decode(data_b64).unwrap());
        }
    }
    let got = fs::read(&out_path).expect("read exported bin file");
    assert_eq!(got, expected, "bin export must be byte-exact");
    assert_eq!(
        format!("{:x}", Sha256::digest(&got)),
        format!("{:x}", Sha256::digest(&expected))
    );
}

#[tokio::test]
async fn bin_export_cli_rejects_filter_with_an_explicit_error() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = populated_recorder(tmp_data.path());
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let output = cli(
        &daemon.socket_path,
        &["export", "dev", "--format", "bin", "--filter", "boot"],
    )
    .output()
    .await
    .expect("run export");
    assert!(
        !output.status.success(),
        "bin + filter must be rejected, not silently ignored"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("filter") && stderr.contains("bin"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
async fn bin_export_cli_to_a_pty_stdout_is_refused_not_silently_written() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = populated_recorder(tmp_data.path());
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let pair = openpty(None, None).expect("openpty");
    let slave = std::fs::File::from(pair.slave);

    // Deliberately `.spawn()` + `wait_with_output()`, not `.output()`:
    // tokio's `Command::output()` unconditionally overwrites stdout/stderr
    // to `Stdio::piped()` internally (see `tokio::process::Command::output`'s
    // own source), which would silently discard the pty slave assigned
    // below and defeat the whole point of this test.
    let mut cmd = cli(&daemon.socket_path, &["export", "dev", "--format", "bin"]);
    cmd.stdout(Stdio::from(slave));
    let child = cmd.spawn().expect("spawn export");
    let output = child
        .wait_with_output()
        .await
        .expect("wait for export to exit");
    assert!(
        !output.status.success(),
        "bin export to a tty must be refused"
    );
    let stderr = stderr_text(&output);
    assert!(stderr.contains("terminal"), "stderr: {stderr}");
}

// ---- Acceptance 2: jsonl round-trip, end to end through the CLI ----

#[tokio::test]
async fn jsonl_export_cli_writes_every_record_verbatim() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = populated_recorder(tmp_data.path());
    let all_records = recorder.read_since(0, usize::MAX).unwrap().records;
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("out.jsonl");
    let output = cli(
        &daemon.socket_path,
        &[
            "export",
            "dev",
            "--format",
            "jsonl",
            "-o",
            out_path.to_str().unwrap(),
        ],
    )
    .output()
    .await
    .expect("run export");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    let text = fs::read_to_string(&out_path).expect("read exported jsonl file");
    let replayed: Vec<Record> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl record"))
        .collect();
    assert_eq!(replayed, all_records);
}

// ---- Acceptance 3: txt shape, end to end through the CLI ----

#[tokio::test]
async fn txt_export_cli_annotates_events_and_binary_content() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = populated_recorder(tmp_data.path());
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let output = cli(&daemon.socket_path, &["export", "dev", "--format", "txt"])
        .output()
        .await
        .expect("run export");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);

    assert!(text.contains("boot ok"), "text: {text}");
    assert!(
        text.contains("# ") && text.contains("connect"),
        "text: {text}"
    );
    assert!(text.contains("[4 bytes binary]"), "text: {text}");
    assert!(text.contains("de ad be ef"), "text: {text}");
    assert!(text.contains("gate action=allow"), "text: {text}");
    assert!(text.contains("tx client=agent:1"), "text: {text}");
}

// ---- --boot resolution, end to end through the CLI ----

#[tokio::test]
async fn export_cli_boot_flag_starts_from_the_most_recent_connect_event() {
    let tmp_data = tempfile::tempdir().unwrap();
    let recorder = Recorder::open(tmp_data.path(), "dev", RecorderConfig::default()).unwrap();
    recorder
        .append_rx(b"before boot, must not appear\n")
        .unwrap(); // seq 0
    recorder.append_event("connect", JsonMap::new()).unwrap(); // seq 1 — the boot marker
    let boot_seq = recorder.read_since(0, usize::MAX).unwrap().records.len() as u64 - 1;
    recorder.append_rx(b"after boot, must appear\n").unwrap(); // seq 2
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    // Structural check (jsonl carries an explicit `seq` per record; a
    // literal substring check against jsonl wouldn't work for `rx` content
    // anyway, since payloads are base64-encoded on the wire/on disk).
    let output = cli(
        &daemon.socket_path,
        &["export", "dev", "--format", "jsonl", "--boot"],
    )
    .output()
    .await
    .expect("run export");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let text = stdout_text(&output);
    let seqs: Vec<u64> = text
        .lines()
        .map(|line| {
            let record: Record = serde_json::from_str(line).unwrap();
            record.seq()
        })
        .collect();
    assert!(
        !seqs.contains(&0),
        "pre-boot record (seq 0) leaked into --boot export: {seqs:?}"
    );
    assert_eq!(
        seqs.first().copied(),
        Some(boot_seq),
        "--boot must start exactly at the connect event's own seq: {seqs:?}"
    );
    assert!(seqs.contains(&2), "seqs: {seqs:?}");

    // Content check (txt renders plain text, so this directly exercises
    // what a human would actually see).
    let txt_output = cli(
        &daemon.socket_path,
        &["export", "dev", "--format", "txt", "--boot"],
    )
    .output()
    .await
    .expect("run export (txt)");
    assert!(
        txt_output.status.success(),
        "stderr: {}",
        stderr_text(&txt_output)
    );
    let txt_text = stdout_text(&txt_output);
    assert!(
        !txt_text.contains("must not appear"),
        "pre-boot content leaked into --boot export: {txt_text}"
    );
    assert!(
        txt_text.contains("after boot, must appear"),
        "text: {txt_text}"
    );
}

// ---- Aged-out ranges: warning, not silence, end to end through the CLI ----

#[tokio::test]
async fn export_cli_warns_on_stderr_when_the_range_has_partially_aged_out() {
    let tmp_data = tempfile::tempdir().unwrap();
    let config = RecorderConfig {
        segment_bytes: 300,
        ring_bytes: 900,
        checkpoint_every: 3,
        checkpoint_bytes: 100,
        fsync_interval: std::time::Duration::from_secs(3600),
    };
    let recorder = Recorder::open(tmp_data.path(), "dev", config).unwrap();
    for i in 0..200u64 {
        recorder
            .append_rx(format!("payload-{i:04}\n").as_bytes())
            .unwrap();
    }
    let daemon = start_daemon_with_device("dev", Arc::new(recorder)).await;

    let output = cli(
        &daemon.socket_path,
        &["export", "dev", "--format", "jsonl", "--from", "0"],
    )
    .output()
    .await
    .expect("run export");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("warning") && stderr.contains("truncated"),
        "stderr must warn about the aged-out truncation, not stay silent: {stderr}"
    );
    // Still produced a real, non-empty result — not an empty file passed
    // off as success.
    assert!(
        !stdout_text(&output).trim().is_empty(),
        "truncated export must still contain the surviving records"
    );
}
