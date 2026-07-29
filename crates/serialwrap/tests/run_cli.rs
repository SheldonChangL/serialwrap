//! CLI-level acceptance tests for `serialwrap run` (issue #9 / `TASKS.md`
//! T2.2), driving the *actual compiled* `serialwrap` binary as a
//! subprocess against a real UDS protocol server backed by [`TestBackend`]
//! — same discipline `write_config_clients_cli.rs`/`tail_cli.rs` already
//! established. The wire-level lease state machine (event fields/timing,
//! `follow` staying connected, `--lease-timeout`) is already covered by
//! `serialwrapd`'s own `tests/lease_protocol.rs`; what's specific to *this*
//! crate is proving the CLI wrapper itself: it spawns the given command
//! with inherited stdio, forwards the right exit code, and — the one
//! genuinely CLI-only edge case — notices promptly and releases the lease
//! when the command it spawned is killed out from under it.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::process::Command;

use serialwrapd::port::DeviceId;
use serialwrapd::protocol::backend::{testing::TestBackend, DeviceBackend};
use serialwrapd::protocol::{server, Shared};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
    backend: Arc<TestBackend>,
}

async fn start_daemon(device_id: &str) -> (TestDaemon, tempfile::TempDir, Arc<Recorder>) {
    let tmp_data = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp_data.path(), device_id, RecorderConfig::default())
            .expect("open recorder"),
    );
    let backend = Arc::new(TestBackend::new());
    backend.register(DeviceId(device_id.to_string()), Arc::clone(&recorder));

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sock");
    let listener = server::bind(&path).expect("bind test socket");
    let shared = Arc::new(Shared::new(
        Arc::clone(&backend) as Arc<dyn DeviceBackend>,
        "test",
        dir.path(),
    ));
    tokio::spawn(server::serve(listener, shared));

    (
        TestDaemon {
            socket_path: path,
            _dir: dir,
            backend,
        },
        tmp_data,
        recorder,
    )
}

fn cli(socket: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_serialwrap"));
    cmd.env("SERIALWRAP_SOCKET", socket);
    cmd
}

fn is_connected(backend: &TestBackend, device: &str) -> bool {
    backend
        .list_devices()
        .into_iter()
        .find(|d| d.id.0 == device)
        .is_some_and(|d| d.connected)
}

async fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn find_events<'a>(
    records: &'a [Record],
    name: &str,
) -> Vec<&'a serde_json::Map<String, serde_json::Value>> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == name => Some(extra),
            _ => None,
        })
        .collect()
}

// ---- Normal exit: the CLI forwards the child's exit code and releases
// the lease ----

#[tokio::test]
async fn run_forwards_the_commands_exit_code_and_releases_the_lease() {
    let (daemon, _datadir, recorder) = start_daemon("dev").await;

    let output = cli(&daemon.socket_path)
        .args(["run", "dev", "--", "sh", "-c", "exit 3"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("run `serialwrap run`");

    assert_eq!(
        output.status.code(),
        Some(3),
        "serialwrap run should forward the child's own exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(1), || is_connected(
            &daemon.backend,
            "dev"
        ))
        .await,
        "device should be connected again once `serialwrap run` has released the lease"
    );

    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let starts = find_events(&records, "lease_start");
    let ends = find_events(&records, "lease_end");
    assert_eq!(starts.len(), 1, "{records:?}");
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(
        starts[0].get("command").and_then(|v| v.as_str()),
        Some("sh -c exit 3")
    );
    assert_eq!(ends[0].get("exit_code").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(
        ends[0].get("reason").and_then(|v| v.as_str()),
        Some("released")
    );

    println!(
        "serialwrap run: normal exit forwards exit code 3 and releases the lease with a matching \
         lease_end event"
    );
}

// ---- Criterion 5: the spawned command being SIGKILLed is noticed and
// released within 1 second ----

#[tokio::test]
async fn run_notices_a_sigkilled_child_and_reclaims_the_port_within_one_second() {
    let (daemon, _datadir, recorder) = start_daemon("dev").await;
    let pidfile_dir = tempfile::tempdir().expect("tempdir");
    let pidfile = pidfile_dir.path().join("child.pid");

    let mut child = cli(&daemon.socket_path)
        .args([
            "run",
            "dev",
            "--",
            "sh",
            "-c",
            &format!("echo $$ > {} ; sleep 30", pidfile.display()),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `serialwrap run`");

    // Wait for the leased shell to report its own pid.
    let got_pid = wait_until(Duration::from_secs(5), || pidfile.exists()).await;
    assert!(got_pid, "the leased command never wrote its pidfile");
    let pid_text = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                if !s.trim().is_empty() {
                    return s;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("pidfile never got real content");
    let child_pid: i32 = pid_text
        .trim()
        .parse()
        .expect("pidfile should contain a pid");

    assert!(
        wait_until(Duration::from_secs(1), || !is_connected(
            &daemon.backend,
            "dev"
        ))
        .await,
        "device should be disconnected once the lease is acquired"
    );

    let killed_at = Instant::now();
    kill(Pid::from_raw(child_pid), Signal::SIGKILL).expect("SIGKILL the leased command");

    // `serialwrap run` itself must notice its child died and exit promptly.
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .expect("serialwrap run did not exit within 2s of its child being SIGKILLed")
        .expect("wait on serialwrap run");
    assert!(
        !status.success(),
        "serialwrap run should not report success for a SIGKILLed child"
    );

    // And the daemon must have the port back — this project's actual
    // acceptance bound — within 1 second of the kill.
    let recovered = wait_until(Duration::from_secs(1), || {
        is_connected(&daemon.backend, "dev")
    })
    .await;
    let elapsed = killed_at.elapsed();
    assert!(
        recovered,
        "port was not reclaimed within 1s of the child being SIGKILLed (waited {elapsed:?})"
    );

    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let ends = find_events(&records, "lease_end");
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(
        ends[0].get("reason").and_then(|v| v.as_str()),
        Some("released")
    );
    let exit_code = ends[0].get("exit_code").and_then(|v| v.as_i64());
    assert_eq!(
        exit_code,
        Some(-9),
        "a SIGKILLed child's exit_code should read as -9 (negated signal number), got {exit_code:?}"
    );

    println!(
        "acceptance (T2.2) #5 — a SIGKILLed child is noticed by `serialwrap run` and the port \
         reclaimed within {elapsed:?} (bound: 1s)"
    );
}
