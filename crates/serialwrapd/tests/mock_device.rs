//! Integration tests for the T0.2 mock-device test fixture itself.
//!
//! These don't test any `serialwrapd` daemon logic yet (there isn't any —
//! T1.x fills that in). What they prove is that the fixture every later
//! milestone's tests will be built on actually behaves like a serial
//! device: scripted output round-trips byte-exact, commands get answered,
//! disconnect/reconnect works, and throughput is high enough that the
//! recorder (T1.2) won't be the bottleneck.
//!
//! Platform note: PTY behavior around a closed master differs between
//! macOS and Linux (clean EOF vs. an I/O error). Where that matters, tests
//! accept either outcome rather than assume one platform's behavior — see
//! `mock_device`'s crate docs and `docs/manual-checklist.md`.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use mock_device::{script, MockDevice, Pattern};

/// Read from `file` on a throwaway thread and wait for it, up to
/// `timeout`. Used for responses that arrive off the mock device's
/// background responder thread, so a genuine fixture bug (no response
/// ever sent) fails the test instead of hanging the whole suite.
fn read_with_deadline(mut file: File, timeout: Duration) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        if let Ok(n) = file.read(&mut buf) {
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    rx.recv_timeout(timeout).ok()
}

// NOTE ON TEST STRUCTURE: a PTY's kernel-side buffer is small (a few KB, and
// smaller than you'd guess — this was measured the hard way: an earlier
// version of `binary_chunk_round_trips_byte_exact_and_is_non_utf8` wrote its
// whole 8192-byte payload in one blocking call *before* reading any of it
// back, and deadlocked outright on macOS because the write couldn't
// complete until something drained the buffer, and nothing was reading
// yet). Every test below that pushes more than a trivial number of bytes
// therefore writes from a background thread *while* the main thread reads
// concurrently, exactly like a real daemon would (read continuously as the
// device produces output) rather than "write everything, then read".

// ROOT CAUSE (Linux CI flake, issue #39): this test used to spawn its
// writer with `thread::spawn(move || device.write_device_output(&banner))`,
// which *moves* `device` into the writer closure. The moment
// `write_device_output` returns, that closure's stack frame drops —
// including `device` itself, since nothing gives it back out (the closure's
// return value is only the `io::Result<()>` from the write call). Dropping
// `MockDevice` tears down the PTY: its `responder` field's `Drop` impl
// (`Responder::stop`) joins the responder thread, which lets go of *its*
// `Arc<OwnedFd>` clone of the master, and `device`'s own `master`/
// `spare_slave` fds close right along with it — so the master side of the
// PTY fully closes within roughly one responder-poll tick (<=50ms) of the
// write returning, often faster.
//
// That race the main thread's `reader.read_exact` on the slave side: on
// Linux, once every fd on the *master* side of a PTY closes while bytes the
// kernel accepted are still sitting unread in the line discipline's queue,
// a concurrent slave-side `read()` can come back with EIO/EOF instead of
// the buffered data — a kernel-level race, not a logic bug in this crate
// (see `pty::PtyPair`'s docs on the analogous, already-handled master-sees-
// hangup direction). macOS's PTY implementation is far more forgiving here,
// which is exactly why this only ever reproduced on ubuntu-latest: a
// single-write, no-delay test like this one gives the writer thread almost
// nothing else to do before it can drop `device`, so the teardown races the
// read as tightly as possible.
//
// The fix is not a bigger timeout or a retry — it's to stop tearing the
// device down while a read against it is still in flight. `thread::scope`
// lets the writer closure *borrow* `device` (it only needs `&self`) instead
// of owning it, so `device` stays alive in this function's own stack frame
// for as long as the function runs, regardless of when the writer thread
// finishes — it cannot be dropped mid-read no matter how CI schedules these
// threads.
#[test]
fn boot_banner_is_captured_as_the_first_bytes() {
    let device = MockDevice::new().expect("open mock device");
    let mut reader = device.open_slave().expect("open slave");
    let banner = script::boot_banner();

    thread::scope(|scope| {
        let writer = scope.spawn(|| device.write_device_output(&banner));

        let mut buf = vec![0u8; banner.len()];
        reader.read_exact(&mut buf).expect("read boot banner");
        writer
            .join()
            .expect("writer thread panicked")
            .expect("write boot banner");
        assert_eq!(buf, banner, "boot banner must arrive byte-exact");
    });
}

// Same device-lifetime hazard as `boot_banner_is_captured_as_the_first_bytes`
// above (see its comment for the full root cause): the writer used to own
// `device` outright, so it — and the PTY master along with it — could drop
// the instant the last scripted write returned, racing the main thread's
// still-in-progress `read_exact`. `thread::scope` keeps `device` owned by
// this function for its whole lifetime instead.
#[test]
fn periodic_lines_arrive_in_order_at_the_scripted_interval() {
    let device = MockDevice::new().expect("open mock device");
    let mut reader = device.open_slave().expect("open slave");

    const COUNT: u64 = 5;
    let interval = Duration::from_millis(40);

    let start = Instant::now();
    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for seq in 0..COUNT {
                device.write_device_output(&script::periodic_line(seq))?;
                thread::sleep(interval);
            }
            Ok::<(), std::io::Error>(())
        });

        let mut expected = Vec::new();
        for seq in 0..COUNT {
            expected.extend_from_slice(&script::periodic_line(seq));
        }
        let mut buf = vec![0u8; expected.len()];
        reader.read_exact(&mut buf).expect("read periodic lines");
        let elapsed = start.elapsed();
        writer
            .join()
            .expect("writer thread panicked")
            .expect("write periodic lines");
        assert_eq!(
            buf, expected,
            "periodic lines must arrive in order, byte-exact"
        );

        // Loose sanity check that the interval was actually honored (not
        // just one instantaneous burst) — generous tolerance for scheduler
        // jitter. This one *is* an inherently time-based assertion (the
        // acceptance criterion is about the interval being honored), unlike
        // the device-teardown race this refactor fixes.
        let expected_min = interval * (COUNT as u32 - 1);
        assert!(
            elapsed >= expected_min,
            "expected the writer loop to take at least {expected_min:?}, took {elapsed:?}"
        );
    });
}

#[test]
fn binary_chunk_round_trips_byte_exact_and_is_non_utf8() {
    let device = MockDevice::new().expect("open mock device");
    let mut reader = device.open_slave().expect("open slave");

    let payload = script::binary_chunk(8192);
    assert!(
        std::str::from_utf8(&payload).is_err(),
        "fixture bug: binary_chunk should never be valid UTF-8"
    );

    // Same device-lifetime hazard as `boot_banner_is_captured_as_the_first_bytes`
    // (see its comment): borrow `device` via `thread::scope` instead of
    // moving it into the writer, so it can't be torn down mid-read.
    thread::scope(|scope| {
        let writer = scope.spawn(|| device.write_device_output(&payload));

        let mut buf = vec![0u8; payload.len()];
        reader.read_exact(&mut buf).expect("read binary chunk");
        writer
            .join()
            .expect("writer thread panicked")
            .expect("write binary chunk");
        assert_eq!(
            buf, payload,
            "binary chunk must round-trip byte-exact over the PTY"
        );
    });
}

// Same device-lifetime hazard as `boot_banner_is_captured_as_the_first_bytes`
// above (see its comment for the full root cause).
#[test]
fn repeated_line_round_trips_with_expected_repeat_count() {
    let device = MockDevice::new().expect("open mock device");
    let mut reader = device.open_slave().expect("open slave");

    let payload = script::repeated_line("boot ok", 50);
    thread::scope(|scope| {
        let writer = scope.spawn(|| device.write_device_output(&payload));

        let mut buf = vec![0u8; payload.len()];
        reader.read_exact(&mut buf).expect("read repeated lines");
        writer
            .join()
            .expect("writer thread panicked")
            .expect("write repeated lines");
        assert_eq!(buf, payload);
        assert_eq!(
            buf.iter().filter(|&&b| b == b'\n').count(),
            50,
            "expected exactly 50 newline-terminated repeats"
        );
    });
}

#[test]
fn status_command_gets_the_registered_response() {
    let device = MockDevice::new().expect("open mock device");
    device.on_command(Pattern::exact("status"), b"status: ok\n".to_vec());

    let mut daemon_side = device.open_slave().expect("open slave");
    daemon_side
        .write_all(b"status\n")
        .expect("send status command");

    let response = read_with_deadline(daemon_side, Duration::from_secs(5))
        .expect("expected a response to `status\\n` within 5s");
    assert_eq!(response, b"status: ok\n");
}

#[test]
fn unregistered_command_gets_no_response() {
    let device = MockDevice::new().expect("open mock device");
    device.on_command(Pattern::exact("status"), b"status: ok\n".to_vec());

    let mut daemon_side = device.open_slave().expect("open slave");
    daemon_side
        .write_all(b"not_a_registered_command\n")
        .expect("send unregistered command");

    // Nothing should come back; a short bounded wait that times out is the
    // expected (passing) outcome here.
    let response = read_with_deadline(daemon_side, Duration::from_millis(300));
    assert!(
        response.is_none(),
        "expected no response to an unregistered command, got {response:?}"
    );
}

#[test]
fn disconnect_then_reconnect_recovers_communication() {
    let mut device = MockDevice::new().expect("open mock device");
    let mut reader = device.open_slave().expect("open slave");

    device
        .write_device_output(b"before disconnect\n")
        .expect("write before disconnect");
    let mut buf = [0u8; 64];
    let n = reader.read(&mut buf).expect("read before disconnect");
    assert_eq!(&buf[..n], b"before disconnect\n");

    device.disconnect().expect("disconnect");
    assert!(!device.is_connected());

    // The already-open reader must see the device go away. Which exact
    // outcome (EOF vs. an I/O error) depends on the platform (see crate
    // docs) — both mean "disconnected" from a daemon's point of view, so
    // the acceptance criterion is "EOF or error", not one specific variant.
    match reader.read(&mut buf) {
        Ok(0) => {} // EOF
        Ok(n) => panic!("expected EOF/error after disconnect, got {n} bytes of data"),
        Err(_) => {} // I/O error is also acceptable
    }

    device.reconnect().expect("reconnect");
    assert!(device.is_connected());

    let mut reader2 = device.open_slave().expect("open slave after reconnect");
    device
        .write_device_output(b"after reconnect\n")
        .expect("write after reconnect");
    let n = reader2.read(&mut buf).expect("read after reconnect");
    assert_eq!(&buf[..n], b"after reconnect\n");
}

#[test]
fn sustains_at_least_one_megabyte_per_second() {
    let device = MockDevice::new().expect("open mock device");
    let reader = device.open_slave().expect("open slave");

    const TOTAL: usize = 8 * 1024 * 1024; // 8 MiB
    const CHUNK: usize = 64 * 1024;
    let payload = script::binary_chunk(TOTAL);

    let start = Instant::now();
    let reader_handle = thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; CHUNK];
        let mut received = 0usize;
        while received < TOTAL {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(e) => panic!("reader error after {received} bytes: {e}"),
            }
        }
        received
    });

    for chunk in payload.chunks(CHUNK) {
        device
            .write_device_output(chunk)
            .expect("write high-rate chunk");
    }

    let received = reader_handle.join().expect("reader thread panicked");
    let elapsed = start.elapsed();

    assert_eq!(received, TOTAL, "reader must receive the full payload");

    let bytes_per_sec = TOTAL as f64 / elapsed.as_secs_f64();
    println!(
        "mock-device sustained throughput: {:.2} MB/s ({TOTAL} bytes in {elapsed:?})",
        bytes_per_sec / 1_000_000.0
    );
    assert!(
        bytes_per_sec >= 1_000_000.0,
        "throughput {bytes_per_sec:.0} B/s is below the required 1 MB/s floor"
    );
}
