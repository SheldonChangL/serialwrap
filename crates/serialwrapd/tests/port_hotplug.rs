//! Acceptance tests for T1.1 (device identity + hotplug detection, issue
//! #3) against the real PTY-backed mock-device fixture (T0.2) — these are
//! the tests that need a genuine, independently-openable device path
//! (rather than the lighter, sleep-free unit tests in `src/port.rs` that
//! use plain temp files). Covers acceptance criteria 1-4 from the issue:
//! id stability across 100 replugs, path-drift identity, the 300ms
//! open-latency budget, and the S1 end-to-end scenario (boot banner
//! captured with no client present).

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use mock_device::{script, MockDevice};
use serialwrapd::port::testing::ScriptedEnumerator;
use serialwrapd::port::{DeviceId, EnumeratedDevice, HotplugConfig, HotplugDetector, UsbMetadata};
use serialwrapd::recorder::{Recorder, RecorderConfig};
use wrap_proto::Record;

/// Repeatedly call `poll_once` (1ms retry step, no fixed poll interval)
/// until `check` is satisfied or `timeout` elapses.
fn poll_until(
    detector: &mut HotplugDetector,
    timeout: Duration,
    mut check: impl FnMut(&mut HotplugDetector) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let _ = detector.poll_once();
        if check(detector) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn count_events(recorder: &Recorder, event_name: &str) -> usize {
    recorder
        .read_since(0, usize::MAX)
        .expect("read_since")
        .records
        .iter()
        .filter(|r| matches!(r, Record::Event { event, .. } if event == event_name))
        .count()
}

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

/// Point a symlink at `link_path` to `target`, replacing anything already
/// there. Lets a test give a PTY slave path a literal `ttyUSB0`/`ttyUSB1`
/// -style name of its own choosing, independent of whatever pty index the
/// OS's real allocator happens to (re)use across a disconnect/reconnect —
/// macOS in particular can and does hand back the *same* pty path on
/// reconnect, which would otherwise make a path-drift test either flaky or
/// silently not testing what it claims to.
fn point_symlink(link_path: &Path, target: &Path) {
    let _ = fs::remove_file(link_path);
    std::os::unix::fs::symlink(target, link_path).expect("create symlink");
}

fn tiny_poll_config() -> HotplugConfig {
    HotplugConfig {
        poll_interval: Duration::from_millis(5),
        recorder_config: RecorderConfig::default(),
    }
}

/// Acceptance criterion 2: `ttyUSB0 -> ttyUSB1`-style path drift on the
/// same USB identity (same serial number) must be recognized as the same
/// device and keep writing into the same `Recorder` directory.
#[test]
fn path_drift_reuses_the_same_recorder_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let devnodes = tmp.path().join("devnodes");
    fs::create_dir_all(&devnodes).unwrap();
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x1a86,
        pid: 0x7523,
        serial_number: Some("DRIFT-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    // Report a literal `ttyUSB0` name (symlinked to the real PTY slave) so
    // the drift below is a genuine, deterministic path-name change rather
    // than depending on what the OS's pty allocator happens to hand back.
    let old_path = devnodes.join("ttyUSB0");
    point_symlink(&old_path, device.slave_path());

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: old_path.clone(),
        usb: Some(usb.clone()),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    );

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );
    let recorder_before = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());

    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard
                .get(&id)
                .is_some_and(|r| count_events(r, "disconnect") == 1)
        }),
        "expected a disconnect event"
    );

    device.reconnect().expect("reconnect");
    let new_path = devnodes.join("ttyUSB1");
    point_symlink(&new_path, device.slave_path());
    assert_ne!(
        old_path, new_path,
        "ttyUSB0 -> ttyUSB1 must be a different reported path"
    );
    enumerator.replace_path(&old_path, new_path.clone());

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard
                .get(&id)
                .is_some_and(|r| count_events(r, "connect") == 2)
        }),
        "expected a reconnect (second connect event) at the new path"
    );

    // Same device id -> same Recorder instance -> same on-disk directory,
    // and exactly one device directory ever created.
    let recorder_after = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());
    assert!(
        Arc::ptr_eq(&recorder_before, &recorder_after),
        "path drift must reuse the same Recorder, not open a new one"
    );
    let devices_dir = tmp.path().join("data").join("devices");
    let dir_count = fs::read_dir(&devices_dir).unwrap().count();
    assert_eq!(
        dir_count, 1,
        "expected exactly one device directory despite the path change"
    );
    assert!(devices_dir.join(&id.0).is_dir());

    // The second connect event must report the *new* path.
    let records = recorder_after.read_since(0, usize::MAX).unwrap().records;
    let paths: Vec<String> = records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == "connect" => extra
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect();
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0], old_path.to_string_lossy());
    assert_eq!(paths[1], new_path.to_string_lossy());
}

/// Acceptance criterion 1: replug 100 times, device id must never change,
/// and exactly one Recorder/directory must be used throughout.
///
/// `#[ignore]`d: each cycle's real PTY teardown/setup (disconnect joins a
/// responder thread; reconnect spawns a fresh `openpty()` pair and
/// responder thread) costs several ms of genuine OS/thread-scheduling
/// overhead, and 100 of them take ~5-6s — on its own more than half this
/// project's ≤10s default `cargo test --all` budget (see `TASKS.md`/issue
/// #3's acceptance criteria). Run explicitly via `cargo test -- --ignored`
/// (CI does this as its own step, so it still runs on every push). The
/// literal-wording acceptance criterion is this test; the *mechanism*
/// (path drift not affecting identity) is also covered by the fast,
/// non-ignored `path_drift_reuses_the_same_recorder_directory` above,
/// which is single-cycle and runs in well under 100ms.
#[test]
#[ignore = "100 real PTY disconnect/reconnect cycles take ~5-6s; run via `cargo test -- --ignored` \
            (also wired into CI). See doc comment; the fast \
            path_drift_reuses_the_same_recorder_directory test above covers the same mechanism \
            single-cycle in well under 100ms."]
fn device_id_survives_100_replug_cycles() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let devnodes = tmp.path().join("devnodes");
    fs::create_dir_all(&devnodes).unwrap();
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x2341,
        pid: 0x0043,
        serial_number: Some("REPLUG-100".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    // Literal `ttyUSB<n>`-style reported names (symlinked to whatever real
    // pty the OS hands back each cycle) so this genuinely exercises a
    // changing path every cycle, not just "disconnect/reconnect happened".
    let path_at = |n: usize| devnodes.join(format!("ttyUSB{n}"));
    let mut current_path = path_at(0);
    point_symlink(&current_path, device.slave_path());

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: current_path.clone(),
        usb: Some(usb.clone()),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    );

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );
    let recorder_first = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());

    const CYCLES: usize = 100;
    let start = Instant::now();
    for i in 0..CYCLES {
        device.disconnect().expect("disconnect");
        assert!(
            poll_until(&mut detector, Duration::from_secs(2), |d| {
                let recorders = d.recorders();
                let guard = recorders.lock().unwrap();
                guard
                    .get(&id)
                    .is_some_and(|r| count_events(r, "disconnect") == i + 1)
            }),
            "disconnect #{i} not observed"
        );

        device.reconnect().expect("reconnect");
        let next_path = path_at(i + 1);
        point_symlink(&next_path, device.slave_path());
        enumerator.replace_path(&current_path, next_path.clone());
        current_path = next_path;
        assert!(
            poll_until(&mut detector, Duration::from_secs(2), |d| {
                let recorders = d.recorders();
                let guard = recorders.lock().unwrap();
                guard
                    .get(&id)
                    .is_some_and(|r| count_events(r, "connect") == i + 2)
            }),
            "reconnect #{i} not observed"
        );

        // The id itself is re-derived fresh every cycle straight from the
        // fixed USB metadata — assert it never drifts, exactly as the
        // acceptance criterion is worded ("裝置 ID 全程不變").
        assert_eq!(DeviceId::from_usb(&usb).unwrap(), id);
    }
    let elapsed = start.elapsed();
    println!(
        "100 replug cycles: {elapsed:?} total, {:.2}ms/cycle average",
        elapsed.as_secs_f64() * 1000.0 / CYCLES as f64
    );

    let devices_dir = tmp.path().join("data").join("devices");
    let dir_count = fs::read_dir(&devices_dir).unwrap().count();
    assert_eq!(
        dir_count, 1,
        "expected exactly one device directory across {CYCLES} replugs"
    );

    let recorder_last = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());
    assert!(
        Arc::ptr_eq(&recorder_first, &recorder_last),
        "must keep reusing the same Recorder across all {CYCLES} replugs"
    );
}

/// Acceptance criterion 3: from a device appearing in the enumeration
/// snapshot to the daemon opening it and starting to record must be
/// <=300ms. Uses the *real* default poll interval (not a test-tuned faster
/// one) so the measured number reflects production configuration.
///
/// A single fixed delay before injecting the device would just measure
/// whatever phase offset that one sleep duration happens to land at
/// relative to the poll loop's cadence — every run landing at ~100ms out
/// of a 150ms interval proves nothing about the worst case near the full
/// interval. So this sweeps the injection point across the poll cycle
/// (a step that doesn't evenly divide `poll_interval` so consecutive
/// trials land at different phases) and reports the *maximum* observed
/// latency across all trials, which is what could actually blow the
/// budget.
#[test]
fn hotplug_open_latency_is_within_the_300ms_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let enumerator = ScriptedEnumerator::new();
    let detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        HotplugConfig::default(), // real production poll_interval (150ms)
    );
    let handle = detector.spawn();

    const TRIALS: usize = 6;
    const STEP_MS: u64 = 41; // coprime-ish with 150ms: sweeps distinct phases
    let poll_interval_ms = HotplugConfig::default().poll_interval.as_millis() as u64;

    let mut all_elapsed = Vec::with_capacity(TRIALS);
    for i in 0..TRIALS {
        let phase = Duration::from_millis((i as u64 * STEP_MS) % poll_interval_ms);
        thread::sleep(phase);

        let device = MockDevice::new().expect("open mock device");
        let usb = UsbMetadata {
            vid: 0x2341,
            pid: 0x0044,
            serial_number: Some(format!("BUDGET-TEST-{i}")),
        };
        let id = DeviceId::from_usb(&usb).expect("usb id");

        let t0 = Instant::now();
        enumerator.push(EnumeratedDevice {
            path: device.slave_path().to_path_buf(),
            usb: Some(usb),
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut elapsed = None;
        while Instant::now() < deadline {
            let recorders = handle.recorders();
            let found = {
                let guard = recorders.lock().unwrap();
                guard
                    .get(&id)
                    .is_some_and(|r| count_events(r, "connect") >= 1)
            };
            if found {
                elapsed = Some(t0.elapsed());
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let elapsed = elapsed
            .unwrap_or_else(|| panic!("trial {i} (phase {phase:?}): no connect event within 2s"));
        all_elapsed.push((phase, elapsed));
    }
    handle.stop();

    let max_elapsed = all_elapsed
        .iter()
        .map(|(_, e)| *e)
        .max()
        .expect("at least one trial ran");
    println!(
        "hotplug open latency across {TRIALS} phase-swept trials (phase, elapsed): {all_elapsed:?}"
    );
    println!(
        "hotplug open latency: max={max_elapsed:?} ({} ms) across {TRIALS} trials, \
         poll_interval={poll_interval_ms}ms",
        max_elapsed.as_millis()
    );
    assert!(
        max_elapsed <= Duration::from_millis(300),
        "worst observed open latency {max_elapsed:?} exceeded the 300ms budget"
    );
}

/// Acceptance criterion 4 / exit scenario S1: a device is enumerated with
/// no client involvement, immediately produces its boot banner, and that
/// banner's first line must already be in the recording — this is the
/// literal "you never lose the boot log" promise.
#[test]
fn boot_banner_first_line_is_captured_end_to_end_via_mock_device() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("E2E-BOOT".to_string()),
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
            poll_interval: Duration::from_millis(20),
            recorder_config: RecorderConfig::default(),
        },
    );
    let handle = detector.spawn();

    // The device "powers on" and prints its banner immediately — racing
    // the daemon's own detection on purpose, exactly like a real replug.
    //
    // ROOT CAUSE this refactor removes (same class as issue #39's
    // `mock_device.rs` flake): the writer used to own `device` outright and
    // end with a fixed `thread::sleep(300ms)` "drain barrier", because
    // dropping `device` closes the PTY master, and a master-side close can
    // discard whatever the detector's own reader thread hasn't yet drained
    // out of the kernel's input queue — a fixed sleep only makes that race
    // *less likely*, it doesn't remove it. The poll loop below already
    // waits for the real, observable event that makes it safe to let
    // `device` go: bytes actually landing in `handle.recorders()` can only
    // happen after the detector's reader has already pulled them off the
    // wire, so once that loop finds them there is nothing left in the
    // kernel queue that dropping `device` could lose. `thread::scope` keeps
    // `device` borrowed (not owned) by the writer, so it stays alive for as
    // long as this whole function runs — including through the entire poll
    // loop below — regardless of how long the write itself takes.
    let banner = script::boot_banner();
    let mut collected = Vec::new();
    thread::scope(|scope| {
        let writer = scope.spawn(|| device.write_device_output(&banner));

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let recorders = handle.recorders();
            let snapshot = {
                let guard = recorders.lock().unwrap();
                guard.get(&id).map(|r| rx_bytes(r))
            };
            if let Some(bytes) = snapshot {
                if bytes.len() >= banner.len() {
                    collected = bytes;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        writer
            .join()
            .expect("writer thread panicked")
            .expect("write boot banner");
    });
    handle.stop();

    assert!(
        collected.len() >= banner.len(),
        "expected at least the banner's bytes to be recorded, got {} bytes",
        collected.len()
    );
    assert!(
        collected.starts_with(&banner),
        "expected the FIRST recorded rx bytes to be the boot banner, byte-exact"
    );
    let first_line = collected.split(|&b| b == b'\n').next().unwrap();
    let expected_first_line = &banner[..banner.len() - 1]; // strip trailing '\n'
    assert_eq!(
        first_line, expected_first_line,
        "boot banner's first line must be the first line in the recording"
    );
}
