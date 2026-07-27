//! Acceptance tests for T2.2's lease mode (issue #9) that specifically need
//! the *real* `HotplugDetector`/`LiveBackend`/`port_io` fd lifecycle — as
//! opposed to `tests/lease_protocol.rs`, which drives the same wire-level
//! state machine through `TestBackend` (fast, in-memory, no real fd at
//! all). Two things can only be proven here, against a genuine PTY-backed
//! device:
//!
//! - The shared-fd fix this task's issue calls out as a prerequisite:
//!   `write_bytes` must go through the exact same fd `acquire_lease`
//!   closes, not a second, independently opened one — proven by writing
//!   successfully before a lease, failing during it (see
//!   `write_bytes_uses_the_shared_fd_and_fails_while_leased_then_recovers`'s
//!   doc comment for why this specific assertion is the discriminating
//!   one), and succeeding again after release.
//! - Residual-lease recovery after a daemon restart: only meaningful
//!   against a real `HotplugDetector`, since `TestBackend`'s lease state is
//!   plain in-memory and has no "process restart" concept to recover
//!   across at all — the persisted `lease.json` this recovers from is a
//!   `port.rs`-only mechanism.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_device::MockDevice;
use serialwrapd::port::testing::ScriptedEnumerator;
use serialwrapd::port::{DeviceId, EnumeratedDevice, HotplugConfig, HotplugDetector, UsbMetadata};
use serialwrapd::protocol::backend::{DeviceBackend, LiveBackend};
use serialwrapd::recorder::RecorderConfig;
use wrap_proto::Record;

fn tiny_poll_config() -> HotplugConfig {
    HotplugConfig {
        poll_interval: Duration::from_millis(5),
        recorder_config: RecorderConfig::default(),
    }
}

fn usb_meta(serial: &str) -> UsbMetadata {
    UsbMetadata {
        vid: 0x1a86,
        pid: 0x7523,
        serial_number: Some(serial.to_string()),
    }
}

/// Poll `check` until it returns `true` or `timeout` elapses. Async
/// (`tokio::time::sleep`) rather than `thread::sleep`, so the detector's own
/// background poll thread keeps making progress concurrently.
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

fn is_connected(backend: &LiveBackend, id: &DeviceId) -> bool {
    backend
        .list_devices()
        .into_iter()
        .find(|d| &d.id == id)
        .is_some_and(|d| d.connected)
}

// ---- Criterion 1: write goes through the shared fd, not a second one ----

/// The discriminating test for the shared-fd fix: T2.1's original
/// `LiveBackend::write_bytes` opened a *fresh*, independent fd at the
/// device's path on every call (documented at the time as a known
/// limitation). Under that old behavior this test's middle assertion would
/// have *passed* the write (a fresh `open()` at the same path doesn't care
/// that the daemon's own shared fd was closed) — silently defeating the
/// entire point of a lease, since the external tool the lease is for would
/// then be racing the daemon's write path for the same device. Under the
/// fixed behavior (`write_bytes` goes through `PortConfigApi::live_fd`, the
/// exact same accessor `acquire_lease`/`set_dtr`/`dtr_pulse` all share),
/// there is no second fd left to reopen, so the write must fail with
/// `NotConnected` for as long as the lease is held.
#[tokio::test]
async fn write_bytes_uses_the_shared_fd_and_fails_while_leased_then_recovers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = usb_meta("LEASE-FD-TEST");
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    let backend = LiveBackend::new(detector.port_config_api(), detector.recorders());
    let handle = detector.spawn();

    assert!(
        wait_until(Duration::from_secs(2), || is_connected(&backend, &id)).await,
        "device never came up connected"
    );

    // Before any lease: a write succeeds through the shared fd.
    backend
        .write_bytes(&id, b"before-lease")
        .expect("write before lease should succeed");

    // Acquire a lease: the daemon's shared fd for this device must be
    // fully closed now.
    let acquired = backend
        .acquire_lease(&id, "esptool.py write_flash", 4242, None)
        .expect("acquire_lease should succeed");
    assert_eq!(acquired.path, device.slave_path());

    assert!(
        wait_until(Duration::from_secs(2), || !is_connected(&backend, &id)).await,
        "device should report disconnected for the duration of the lease"
    );

    let err = backend
        .write_bytes(&id, b"during-lease")
        .expect_err("write during an active lease must fail — the shared fd is gone");
    assert_eq!(err.kind(), std::io::ErrorKind::NotConnected, "{err}");

    // Release: the daemon reopens and the shared fd works again.
    let released = backend
        .release_lease(&acquired.token, 0)
        .expect("release_lease should succeed");
    assert!(
        released.duration_ms < 5_000,
        "duration_ms should be a small, real elapsed time, got {}",
        released.duration_ms
    );

    assert!(
        wait_until(Duration::from_secs(2), || is_connected(&backend, &id)).await,
        "device should reconnect after lease release"
    );
    backend
        .write_bytes(&id, b"after-lease")
        .expect("write after lease release should succeed");

    handle.stop();
    println!(
        "acceptance (T2.2) #1 — write_bytes goes through the shared fd: succeeds before/after a \
         lease, fails with NotConnected for its entire duration"
    );
}

// ---- Criterion 7: a lease left dangling by a crashed daemon is detected
// and reclaimed on the next process's startup ----

#[tokio::test]
async fn residual_lease_is_detected_and_reclaimed_after_a_simulated_daemon_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    let device = MockDevice::new().expect("open mock device");
    let usb = usb_meta("LEASE-RESTART-TEST");
    let id = DeviceId::from_usb(&usb).expect("usb id");

    // "Daemon instance #1": acquire a lease, then go away without ever
    // calling `release_lease` — standing in for a crash mid-lease.
    {
        let enumerator = ScriptedEnumerator::new();
        enumerator.push(EnumeratedDevice {
            path: device.slave_path().to_path_buf(),
            usb: Some(usb.clone()),
        });
        let detector =
            HotplugDetector::new(Box::new(enumerator), data_dir.clone(), tiny_poll_config());
        let backend = LiveBackend::new(detector.port_config_api(), detector.recorders());
        let handle = detector.spawn();

        assert!(
            wait_until(Duration::from_secs(2), || is_connected(&backend, &id)).await,
            "device never came up connected on the first instance"
        );
        backend
            .acquire_lease(&id, "esptool.py write_flash", 1111, None)
            .expect("acquire_lease should succeed");

        handle.stop();
    }

    assert!(
        data_dir
            .join("devices")
            .join(&id.0)
            .join("lease.json")
            .exists(),
        "expected a persisted lease.json after acquiring, simulating a crash before release"
    );

    // "Daemon instance #2": a fresh HotplugDetector against the same
    // data_dir, and the same still-open mock device (a real restart
    // reopens the same physical port at whatever path it enumerates at).
    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });
    let detector = HotplugDetector::new(Box::new(enumerator), data_dir.clone(), tiny_poll_config());
    let backend = LiveBackend::new(detector.port_config_api(), detector.recorders());
    let handle = detector.spawn();

    assert!(
        wait_until(Duration::from_secs(2), || is_connected(&backend, &id)).await,
        "the second instance should connect and record normally despite the residual lease"
    );

    assert!(
        !data_dir
            .join("devices")
            .join(&id.0)
            .join("lease.json")
            .exists(),
        "residual lease.json should have been consumed by the second instance's startup check"
    );

    let recorders = handle.recorders();
    let recorder = Arc::clone(
        recorders
            .lock()
            .unwrap()
            .get(&id)
            .expect("recorder should exist"),
    );
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let lease_end = records
        .iter()
        .find_map(|r| match r {
            Record::Event { event, extra, .. } if event == "lease_end" => Some(extra.clone()),
            _ => None,
        })
        .expect("expected a lease_end event recovered at startup");
    assert_eq!(
        lease_end.get("reason").and_then(|v| v.as_str()),
        Some("daemon_restart")
    );
    assert_eq!(lease_end.get("pid").and_then(|v| v.as_u64()), Some(1111));
    assert_eq!(
        lease_end
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("esptool")),
        Some(true)
    );
    assert!(
        lease_end.get("exit_code").is_some_and(|v| v.is_null()),
        "a daemon-recovered residual lease never learned the child's real exit status"
    );
    assert!(lease_end
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .is_some());

    handle.stop();
    println!(
        "acceptance (T2.2) #7 — a residual lease left by a simulated daemon crash is detected \
         and reclaimed on the next instance's startup, with a lease_end(reason=daemon_restart) \
         event recorded"
    );
}
