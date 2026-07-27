//! Integration tests for the T1.2 recorder against the real PTY-backed mock
//! device fixture (T0.2) and, for the crash test, a real killed OS process
//! — not simulated in-process state. These are the acceptance tests named
//! in `TASKS.md`/issue #4: byte-exact throughput, cross-segment
//! correctness, and `kill -9` chaos recovery. Cross-segment/rotation/
//! eviction/gap-free-seq/aged-out correctness are *also* covered by the
//! (much faster) unit tests in `src/recorder.rs`, which use tiny
//! configured segment/ring sizes instead of needing megabytes of real
//! traffic to exercise the same logic.
//!
//! # Two-tier throughput testing
//!
//! `cargo test` runtime is a cost every one of this project's remaining
//! tasks pays, every time, on every machine that runs it — a slow default
//! suite erodes the feedback loop for everyone downstream of this task,
//! and that cost compounds far past whatever a single 60-second run once
//! caught. So the byte-exactness acceptance criterion is split in two:
//!
//! - [`mock_device_byte_exact_across_several_segment_boundaries`] runs in
//!   the default suite, finishes in well under 5 seconds, and proves the
//!   same thing the 60s run does (byte-exact content, no dupes/gaps,
//!   correct across real segment-boundary crossings) by using a small
//!   segment size instead of a long wall-clock duration to force the
//!   rotations.
//! - [`mock_device_1mb_per_second_sustained_60s_is_byte_exact`] is
//!   `#[ignore]`d and reproduces the issue's literal "1MB/s for 60+
//!   seconds" wording verbatim. Run it explicitly with
//!   `cargo test -- --ignored` (wired into CI as a separate step so it
//!   still runs on every push, just not in the path a human is waiting
//!   on). See that test's own doc comment for why it's believed to add no
//!   coverage the fast test doesn't already have, and what would have to
//!   be true for that belief to be wrong.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use mock_device::MockDevice;
use serialwrapd::recorder::{Recorder, RecorderConfig};
use sha2::{Digest, Sha256};
use wrap_proto::Record;

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Deterministic, reproducible, non-trivial content — varies per chunk so
/// the test isn't just hashing one repeated block, without needing a
/// golden file.
fn make_chunk(chunk_index: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (((i as u64).wrapping_add(chunk_index.wrapping_mul(97))) % 256) as u8)
        .collect()
}

/// Fast, default-suite counterpart to the `#[ignore]`d 60s sustained-
/// throughput test below. Uses a small (1 MiB) configured segment size —
/// not a long wall-clock duration — to force real segment rotations, so a
/// few megabytes of mock-device traffic cross several segment boundaries
/// in well under 5 seconds while still proving the same thing: recorded
/// content is byte-exact against the source, both from `append_rx`'s
/// return values and from a fresh `read_since` re-read off disk.
#[test]
fn mock_device_byte_exact_across_several_segment_boundaries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = RecorderConfig {
        segment_bytes: 1024 * 1024,
        ..RecorderConfig::default()
    };
    let recorder =
        Arc::new(Recorder::open(tmp.path(), "boundary-dev", config).expect("open recorder"));

    let device = MockDevice::new().expect("open mock device");
    let reader = device.open_slave().expect("open slave");

    const CHUNK: usize = 64 * 1024;
    // Base64 inflates ~4/3 plus JSON/timestamp overhead per line, so this
    // comfortably produces several 1MiB segments without needing to tune
    // the exact ratio.
    const TOTAL: usize = 5 * 1024 * 1024;

    let writer = thread::spawn(move || {
        let mut hasher = Sha256::new();
        let mut written = 0usize;
        let mut chunk_index = 0u64;
        while written < TOTAL {
            let len = CHUNK.min(TOTAL - written);
            let chunk = make_chunk(chunk_index, len);
            hasher.update(&chunk);
            device.write_device_output(&chunk).expect("write chunk");
            written += chunk.len();
            chunk_index += 1;
        }
        // Drain barrier: see the sustained test's identical comment below
        // for why this matters (pty hangup on drop discards whatever the
        // reader hasn't drained yet).
        thread::sleep(Duration::from_millis(200));
        (written as u64, hasher.finalize())
    });

    let recorder_for_reader = Arc::clone(&recorder);
    let reader_thread = thread::spawn(move || {
        let mut reader = reader;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    recorder_for_reader.append_rx(&buf[..n]).expect("append_rx");
                    hasher.update(&buf[..n]);
                    total += n as u64;
                }
                Err(_) => break,
            }
        }
        (total, hasher.finalize())
    });

    let (source_total, source_hash) = writer.join().expect("writer thread panicked");
    let (recorded_total, recorded_hash) = reader_thread.join().expect("reader thread panicked");

    assert_eq!(
        recorded_total, source_total,
        "recorded byte count must exactly match source byte count"
    );
    assert_eq!(
        recorded_hash, source_hash,
        "recorded content hash must exactly match source content hash"
    );

    let segments_dir = tmp.path().join("devices/boundary-dev/segments");
    let segment_count = fs::read_dir(&segments_dir).unwrap().count();
    println!(
        "fast byte-exact boundary test: {source_total} bytes across {segment_count} segments \
         (1MiB cap each), sha256={}",
        to_hex(source_hash)
    );
    assert!(
        segment_count >= 3,
        "test needs to actually cross several segment boundaries to be meaningful, only got \
         {segment_count} segment(s) — TOTAL or segment_bytes above need adjusting"
    );

    // Independently re-derive from what's actually on disk via the query
    // interface, not just the in-memory return values of append_rx.
    let mut from_disk = Vec::with_capacity(source_total as usize);
    let mut cursor = 0u64;
    loop {
        let page = recorder
            .read_since(cursor, 512 * 1024)
            .expect("read_since over recorded boundary-crossing data");
        if page.records.is_empty() {
            break;
        }
        for record in &page.records {
            if let Record::Rx { data_b64, .. } = record {
                from_disk.extend(BASE64.decode(data_b64).expect("valid base64 data_b64"));
            }
        }
        assert!(
            page.next_cursor > cursor,
            "read_since must always make forward progress"
        );
        cursor = page.next_cursor;
    }
    assert_eq!(from_disk.len() as u64, source_total);
    let mut disk_hasher = Sha256::new();
    disk_hasher.update(&from_disk);
    assert_eq!(
        to_hex(disk_hasher.finalize()),
        to_hex(source_hash),
        "bytes read back from disk via read_since must be byte-exact against the source"
    );
}

/// Acceptance criterion 1, literal reproduction: mock device sustains at
/// least 1MB/s for at least 60 real seconds; recorded content must be
/// byte-exact against the source (hash comparison), verified both against
/// what `append_rx` returned *and* independently against what's actually
/// readable back off disk via `read_since` (so this also exercises
/// cross-segment `read_since` correctness at a realistic scale — the
/// default 64MB segment cap is crossed well before 60s at this rate).
///
/// `#[ignore]`d: not run by a plain `cargo test`. Run explicitly with
/// `cargo test -- --ignored` (CI does this as its own step, so it still
/// runs on every push — just not in the path a human waits on locally).
///
/// This is believed to add no *coverage* the fast
/// [`mock_device_byte_exact_across_several_segment_boundaries`] doesn't
/// already have — both force real segment rotations and check the same
/// byte-exactness properties; only the mechanism differs (small segments
/// vs. long wall-clock duration). For that belief to be wrong, there
/// would have to be a bug class that only manifests through elapsed *time*
/// specifically, independent of data volume or rotation/eviction/fsync
/// *cycle count* (which the fast test already exercises via small
/// segments) — e.g. some form of clock or counter drift that only
/// accumulates past what a few seconds can trigger. `t_mono`/`t_wall` are
/// read fresh per call with no accumulating state, `fsync` is a stateless
/// periodic check against `Instant::elapsed()`, and ring eviction is
/// purely byte-count-driven — so no such mechanism is known to exist in
/// this implementation today. Kept anyway because it's the literal
/// acceptance criterion as written and costs nothing once it's off the
/// path everyone waits on.
#[test]
#[ignore = "literal 60s/1MB/s acceptance-criterion reproduction; run via `cargo test -- --ignored` \
            (also wired into CI). See doc comment for why the fast \
            mock_device_byte_exact_across_several_segment_boundaries test above is believed to \
            cover the same ground in <5s."]
fn mock_device_1mb_per_second_sustained_60s_is_byte_exact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let recorder = Arc::new(
        Recorder::open(tmp.path(), "throughput-dev", RecorderConfig::default())
            .expect("open recorder"),
    );

    let device = MockDevice::new().expect("open mock device");
    let reader = device.open_slave().expect("open slave");

    const CHUNK: usize = 64 * 1024;
    // Comfortably above the 1MB/s floor so scheduler jitter never drops the
    // *achieved* rate below it, while still finishing in a reasonable time.
    const TARGET_BYTES_PER_SEC: f64 = 1_200_000.0;
    const TEST_DURATION: Duration = Duration::from_secs(62);

    // Owns `device`; when this closure returns (after >=62s), `device`
    // drops, closing the PTY master, which is what lets the reader thread
    // below observe EOF/error once it has drained everything already
    // buffered — no separate "stop" signal needed.
    let writer = thread::spawn(move || {
        let start = Instant::now();
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut chunk_index = 0u64;
        while start.elapsed() < TEST_DURATION {
            let chunk = make_chunk(chunk_index, CHUNK);
            hasher.update(&chunk);
            device.write_device_output(&chunk).expect("write chunk");
            total += chunk.len() as u64;
            chunk_index += 1;

            let target_elapsed = Duration::from_secs_f64(total as f64 / TARGET_BYTES_PER_SEC);
            if let Some(remaining) = target_elapsed.checked_sub(start.elapsed()) {
                thread::sleep(remaining);
            }
        }
        // Drain barrier: `device` (and its PTY master) drops the instant
        // this closure returns, and a pty hangup discards whatever's still
        // sitting in the kernel's input queue rather than letting the
        // reader drain it. The recorder has consistently kept up with far
        // more than 1MB/s in practice (see mock-device's own throughput
        // test), so this is normally a no-op; it exists so a regression
        // that makes the recorder the bottleneck fails as a real assertion
        // below instead of as a flaky short read here.
        thread::sleep(Duration::from_millis(300));
        (total, hasher.finalize())
    });

    let recorder_for_reader = Arc::clone(&recorder);
    let reader_thread = thread::spawn(move || {
        let mut reader = reader;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    recorder_for_reader.append_rx(&buf[..n]).expect("append_rx");
                    hasher.update(&buf[..n]);
                    total += n as u64;
                }
                Err(_) => break,
            }
        }
        (total, hasher.finalize())
    });

    let (source_total, source_hash) = writer.join().expect("writer thread panicked");
    let (recorded_total, recorded_hash) = reader_thread.join().expect("reader thread panicked");

    let elapsed_secs = TEST_DURATION.as_secs_f64();
    println!(
        "byte-exact throughput: {source_total} bytes over {elapsed_secs:.1}s \
         ({:.2} MB/s), source sha256={}, recorded sha256={}",
        source_total as f64 / elapsed_secs / 1_000_000.0,
        to_hex(source_hash),
        to_hex(recorded_hash),
    );

    assert!(
        source_total as f64 / elapsed_secs >= 1_000_000.0,
        "test invariant: achieved rate must be >= 1MB/s floor, got {:.0} B/s",
        source_total as f64 / elapsed_secs
    );
    assert_eq!(
        recorded_total, source_total,
        "recorded byte count must exactly match source byte count"
    );
    assert_eq!(
        recorded_hash, source_hash,
        "recorded content hash must exactly match source content hash"
    );

    // Independently re-derive from what's actually on disk via the query
    // interface, not just the in-memory return values of append_rx.
    let mut from_disk = Vec::with_capacity(source_total as usize);
    let mut cursor = 0u64;
    let mut pages = 0u64;
    const MAX_PAGES: u64 = 1_000_000; // generous cap: a `next_cursor` regression must fail loudly, not hang
    loop {
        let page = recorder
            .read_since(cursor, 4 * 1024 * 1024)
            .expect("read_since over recorded throughput data");
        if page.records.is_empty() {
            break;
        }
        for record in &page.records {
            if let Record::Rx { data_b64, .. } = record {
                from_disk.extend(BASE64.decode(data_b64).expect("valid base64 data_b64"));
            }
        }
        assert!(
            page.next_cursor > cursor,
            "read_since must always make forward progress (cursor {cursor} -> {})",
            page.next_cursor
        );
        cursor = page.next_cursor;
        pages += 1;
        assert!(
            pages < MAX_PAGES,
            "read_since paging did not terminate within {MAX_PAGES} pages"
        );
    }
    assert_eq!(from_disk.len() as u64, source_total);
    let mut disk_hasher = Sha256::new();
    disk_hasher.update(&from_disk);
    assert_eq!(
        to_hex(disk_hasher.finalize()),
        to_hex(source_hash),
        "bytes read back from disk via read_since must be byte-exact against the source"
    );
}

// ---------------------------------------------------------------------
// Acceptance criterion 2: kill -9 mid-write, recover with bounded loss.
//
// This spawns a *real* child process (by re-executing this very test
// binary with a name filter — the standard trick for testing crash
// behavior in Rust, since `cargo test` doesn't give each test its own
// process) that does nothing but append_rx in a tight loop and
// periodically fsyncs its own progress marker to a side file. The parent
// waits briefly, sends a real SIGKILL, then reopens a fresh Recorder over
// the same directory (triggering startup recovery) and checks: the file
// is readable, loss (if any) is small, and any `recovery` event's
// `discarded_bytes` is small.
// ---------------------------------------------------------------------

const CHILD_ENV: &str = "SERIALWRAP_RECORDER_CRASH_CHILD";
const CHILD_DIR_ENV: &str = "SERIALWRAP_RECORDER_CRASH_DIR";
const CHILD_STATUS_ENV: &str = "SERIALWRAP_RECORDER_CRASH_STATUS_FILE";
const CHILD_DEVICE_ID: &str = "crash-dev";
/// Generous bound on how much can be missing/discarded after a kill -9.
/// Real loss in this design is expected to be ~0 (writes are synchronous,
/// unbuffered at the Rust level, so a completed `append_rx` call is
/// already on disk regardless of the fsync window) — this bound exists so
/// the test fails loudly instead of flaking if that ever regresses.
const MAX_ACCEPTABLE_LOSS_BYTES: u64 = 65_536;

#[test]
fn kill_minus_9_mid_write_recovers_with_bounded_loss() {
    if std::env::var(CHILD_ENV).is_ok() {
        run_crash_child();
        return; // unreachable in the intended path: the child gets SIGKILLed first.
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let status_path = tmp.path().join("progress.status");
    let exe = std::env::current_exe().expect("current_exe (self-reexec for the crash child)");

    let mut child = Command::new(&exe)
        .arg("kill_minus_9_mid_write_recovers_with_bounded_loss")
        .arg("--exact")
        .arg("--test-threads=1")
        .env(CHILD_ENV, "1")
        .env(CHILD_DIR_ENV, tmp.path())
        .env(CHILD_STATUS_ENV, &status_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crash-writer child process");

    // Let it accumulate a solid amount of real, fsynced progress before
    // pulling the plug.
    thread::sleep(Duration::from_millis(700));

    let kill_ret = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        kill_ret,
        0,
        "kill(2) failed: {}",
        std::io::Error::last_os_error()
    );

    let status = child.wait().expect("reap killed child");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "child must have been terminated by SIGKILL, got {status:?} \
             (if this is `Some(2)`/success, the child outran the parent's \
             kill — see the child's own 20s safety-net deadline)"
        );
    }

    let confirmed_bytes: u64 = fs::read_to_string(&status_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        confirmed_bytes > 0,
        "child must have confirmed (fsynced) at least some progress before being killed"
    );

    // Recovery runs inside this call.
    let recorder = Recorder::open(tmp.path(), CHILD_DEVICE_ID, RecorderConfig::default())
        .expect("recorder must reopen cleanly after kill -9 — file must be readable");

    let result = recorder
        .read_since(0, usize::MAX)
        .expect("read back everything after recovery");

    let mut recovered_rx_bytes: u64 = 0;
    let mut recovery_discarded_bytes: Option<u64> = None;
    for record in &result.records {
        match record {
            Record::Rx { data_b64, .. } => {
                recovered_rx_bytes += BASE64
                    .decode(data_b64)
                    .expect("every stored rx record must be valid base64 post-recovery")
                    .len() as u64;
            }
            Record::Event { event, extra, .. } if event.as_str() == "recovery" => {
                recovery_discarded_bytes = extra.get("discarded_bytes").and_then(|v| v.as_u64());
            }
            _ => {}
        }
    }

    println!(
        "kill -9 recovery: child confirmed {confirmed_bytes} bytes before SIGKILL; \
         recovered {recovered_rx_bytes} rx bytes on reopen; \
         recovery event discarded_bytes={recovery_discarded_bytes:?}"
    );

    assert!(
        recovered_rx_bytes + MAX_ACCEPTABLE_LOSS_BYTES >= confirmed_bytes,
        "lost too much data: last confirmed {confirmed_bytes} bytes, only {recovered_rx_bytes} \
         recovered (bound: {MAX_ACCEPTABLE_LOSS_BYTES} bytes)"
    );
    if let Some(discarded) = recovery_discarded_bytes {
        assert!(
            discarded < MAX_ACCEPTABLE_LOSS_BYTES,
            "recovery discarded suspiciously much: {discarded} bytes"
        );
    }

    // The recovered file must not just be readable but still writable and
    // gap-free going forward.
    let next = recorder
        .append_rx(b"post-recovery")
        .expect("append after recovery must succeed");
    assert_eq!(
        next.seq(),
        result.next_cursor,
        "seq must continue with no gap right after the recovered tail"
    );
}

/// The crash child's entire body: open a recorder over the directory the
/// parent gave it, then hammer `append_rx` as fast as possible (no
/// pacing — we want to maximize the chance of genuinely being mid-write
/// when SIGKILL lands) while periodically fsyncing a progress marker the
/// parent can trust as a lower bound.
fn run_crash_child() {
    let dir = std::env::var(CHILD_DIR_ENV).expect("child: dir env var");
    let status_path = std::env::var(CHILD_STATUS_ENV).expect("child: status env var");

    let recorder = Recorder::open(&dir, CHILD_DEVICE_ID, RecorderConfig::default())
        .expect("child: open recorder");

    const CHUNK: usize = 4096;
    const STATUS_EVERY_BYTES: u64 = 16 * 1024;
    let payload = vec![0xABu8; CHUNK];
    let status_tmp_path = format!("{status_path}.tmp");

    // Safety net only: if the parent's kill somehow never arrives, exit
    // distinctly (not success) so the parent's assertion on the exit
    // signal fails loudly instead of the test hanging forever.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut total: u64 = 0;
    let mut since_status = 0u64;
    while Instant::now() < deadline {
        recorder.append_rx(&payload).expect("child: append_rx");
        total += CHUNK as u64;
        since_status += CHUNK as u64;
        if since_status >= STATUS_EVERY_BYTES {
            // Write-then-rename instead of truncating the status file in
            // place: `File::create` truncates to 0 *before* `write_all`
            // runs, so a SIGKILL landing in that window would otherwise
            // leave the parent reading an empty/truncated status file
            // (spurious `confirmed_bytes == 0` failure). `rename` within
            // the same directory is atomic, so the parent only ever sees
            // either the previous complete value or the new one.
            if let Ok(mut f) = File::create(&status_tmp_path) {
                if f.write_all(total.to_string().as_bytes()).is_ok() && f.sync_all().is_ok() {
                    let _ = fs::rename(&status_tmp_path, &status_path);
                }
            }
            since_status = 0;
        }
    }
    std::process::exit(2);
}
