//! Acceptance tests for T1.3 (port I/O and config core, issue #5) against
//! the real PTY-backed mock-device fixture (T0.2) — the tests here need a
//! genuine, independently-openable device path, unlike the sleep-free unit
//! tests in `src/port_config.rs`/`src/port_io.rs`/`src/error_counts.rs`/
//! `src/device_profile.rs` that use plain data or fake regular files.
//!
//! Covers: profile persistence + automatic re-application across a real
//! disconnect/reconnect cycle (acceptance criterion 4), `PortConfigApi`'s
//! error behavior for unknown/disconnected devices, and — Linux only — a
//! real `TCGETS2`/`TCSETS2` round-trip proving the `BOTHER` path actually
//! accepts 74880 through a real ioctl call, not just this crate's own pure
//! encoding function (see `src/port_config.rs`'s module docs for why the
//! equivalent macOS `IOSSIOSPEED` round-trip cannot be automated at all —
//! it fails `ENOTTY` against any PTY, verified empirically during this
//! task).

use std::io;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use mock_device::MockDevice;
use serialwrapd::port::testing::ScriptedEnumerator;
use serialwrapd::port::{DeviceId, EnumeratedDevice, HotplugConfig, HotplugDetector, UsbMetadata};
use serialwrapd::port_config::{FlowControl, PortConfig};
use serialwrapd::recorder::RecorderConfig;
use wrap_proto::Record;

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

fn tiny_poll_config() -> HotplugConfig {
    HotplugConfig {
        poll_interval: Duration::from_millis(5),
        recorder_config: RecorderConfig::default(),
    }
}

fn event_count(records: &[Record], event_name: &str) -> usize {
    records
        .iter()
        .filter(|r| matches!(r, Record::Event { event, .. } if event == event_name))
        .count()
}

fn config_change_new_bauds(records: &[Record]) -> Vec<u64> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == "config_change" => extra
                .get("new")
                .and_then(|v| v.get("baud"))
                .and_then(|v| v.as_u64()),
            _ => None,
        })
        .collect()
}

/// Acceptance criterion 4: a config saved via
/// `PortConfigApi::set_port_config` must be automatically re-applied the
/// next time the *same device* reconnects — proven here through the real
/// `HotplugDetector` connect/disconnect/reconnect flow (`device_profile.rs`'s
/// own unit tests already cover `ProfileStore` save/load in isolation; this
/// proves the wiring on top of it).
#[test]
fn set_port_config_persists_and_is_reapplied_after_reconnect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x1a86,
        pid: 0x7523,
        serial_number: Some("PROFILE-PERSIST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");
    let old_path = device.slave_path().to_path_buf();

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

    // Explicitly set a non-default, non-standard config while connected.
    let api = detector.port_config_api();
    let custom = PortConfig {
        baud: 74_880,
        flow_control: FlowControl::Hardware,
        ..PortConfig::default()
    };
    api.set_port_config(&id, custom.clone(), "test:persist")
        .expect("set_port_config on a connected, known device must succeed");

    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "disconnect") == 1
            })
        }),
        "expected a disconnect event"
    );

    device.reconnect().expect("reconnect");
    let new_path = device.slave_path().to_path_buf();
    enumerator.replace_path(&old_path, new_path);

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "connect") == 2
            })
        }),
        "expected a reconnect (second connect event)"
    );

    let recorders = detector.recorders();
    let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let bauds = config_change_new_bauds(&records);
    // Sequence: [initial connect -> default 9600, explicit set -> 74880,
    // reconnect -> must be 74880 again, not reset to the 9600 default].
    assert_eq!(
        bauds.last().copied(),
        Some(74_880),
        "reconnect must re-apply the persisted 74880 baud, not the 9600 default; full config_change baud sequence was {bauds:?}"
    );

    // And the profile really did hit disk under this device's own
    // directory (not just held in memory) — `device_profile.rs`'s own
    // tests check `ProfileStore` in isolation; this confirms the full
    // wiring actually calls `save`.
    let profile_path = tmp
        .path()
        .join("data")
        .join("devices")
        .join(&id.0)
        .join("profile.json");
    assert!(profile_path.is_file(), "expected {profile_path:?} to exist");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    assert_eq!(saved["config"]["baud"], 74_880);
}

/// `PortConfigApi` methods must not silently succeed against a device the
/// detector has never seen at all.
#[test]
fn set_port_config_on_unknown_device_errors_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let enumerator = ScriptedEnumerator::new();
    let detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    let api = detector.port_config_api();

    let unknown_id = DeviceId::from_path(std::path::Path::new("/dev/does-not-exist"));
    let err = api
        .set_port_config(&unknown_id, PortConfig::default(), "test")
        .expect_err("must error for a device the detector has never tracked");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let err = api
        .error_counts(&unknown_id)
        .expect_err("error_counts on an unknown device must also error");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// DTR/RTS and error-count operations need a live fd — they must fail
/// clearly (not silently no-op, not panic) once a previously-connected
/// device has disconnected.
#[test]
fn dtr_and_error_counts_error_once_disconnected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x2341,
        pid: 0x0043,
        serial_number: Some("NOT-CONNECTED-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
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

    let api = detector.port_config_api();
    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "disconnect") == 1
            })
        }),
        "expected a disconnect event"
    );

    assert_eq!(
        api.set_dtr(&id, true, "test").unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.set_rts(&id, false, "test").unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.dtr_pulse(&id, Duration::from_millis(10), "test")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.error_counts(&id).unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
}

/// Sanity check that the real `open_and_configure` path (not just the
/// fake-regular-file devices `src/port.rs`'s own unit tests use) actually
/// connects successfully against a real PTY and keeps receiving bytes —
/// regardless of platform, and regardless of whether every configuration
/// step (in particular macOS's `IOSSIOSPEED`, see module docs) fully
/// applied.
#[test]
fn connecting_through_a_real_pty_still_receives_bytes_after_full_config_application() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x0403,
        pid: 0x6001,
        serial_number: Some("REAL-PTY-CONNECT".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected a connect event even though this is a real tty going through full termios/DTR configuration"
    );

    device
        .write_device_output(b"hello after config\n")
        .expect("write");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                r.read_since(0, usize::MAX)
                    .unwrap()
                    .records
                    .iter()
                    .any(|rec| matches!(rec, Record::Rx { .. }))
            })
        }),
        "expected to still receive rx bytes after the full open+configure sequence"
    );
}

/// A PTY has no real modem-control lines, so — discovered empirically
/// while writing this test suite, on this task's macOS development
/// machine — `TIOCMBIS`/`TIOCMBIC` against a pty slave fail with the same
/// `ENOTTY` (raw OS error 25) that `IOSSIOSPEED` does (see
/// `src/port_config.rs`'s module docs). `PortConfigApi::set_dtr`/`set_rts`/
/// `dtr_pulse` deliberately do *not* swallow that failure the way
/// open-time config application does (see `port_io`'s module docs on why
/// *that* case is best-effort): these are explicit, user-invoked
/// operations, and — especially for `dtr_pulse`, whose entire purpose is
/// reliably resetting a board — silently reporting success when the
/// underlying ioctl did nothing would be actively dishonest. So this test
/// accepts either a real success (asserted event recorded) or the
/// well-understood PTY-specific `ENOTTY`, but not any other failure.
/// Real DTR/RTS electrical behavior itself is, as ever,
/// `docs/manual-checklist.md` §2's job, not this test's.
fn accept_success_or_pty_enotty(result: io::Result<()>, what: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => false,
        Err(e) => {
            panic!("{what}: expected success or ENOTTY (PTY has no real modem lines), got {e:?}")
        }
    }
}

#[test]
fn manual_dtr_rts_assert_and_pulse_succeed_or_fail_only_with_pty_enotty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("DTR-LIVE-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
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

    let api = detector.port_config_api();
    let dtr_ok = accept_success_or_pty_enotty(api.set_dtr(&id, true, "test:dtr"), "set_dtr");
    let rts_ok = accept_success_or_pty_enotty(api.set_rts(&id, false, "test:rts"), "set_rts");
    let pulse_ok = accept_success_or_pty_enotty(
        api.dtr_pulse(&id, Duration::from_millis(5), "test:pulse"),
        "dtr_pulse",
    );

    let recorders = detector.recorders();
    let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let expected_control_line_changes = usize::from(dtr_ok) + usize::from(rts_ok);
    assert_eq!(
        event_count(&records, "control_line_change"),
        expected_control_line_changes
    );
    assert_eq!(event_count(&records, "dtr_pulse"), usize::from(pulse_ok));
}

/// Real, Linux-only verification of the actual `BOTHER`/`TCSETS2` ioctl
/// path (not just `encode_linux_baud`'s pure logic, already covered by
/// `src/port_config.rs`'s unit tests): a real PTY genuinely accepts an
/// arbitrary, non-standard baud rate through this crate's real
/// `apply_termios`, and a subsequent real `TCGETS2` read-back shows the
/// exact rate, not a rounded one.
///
/// No macOS equivalent exists: `IOSSIOSPEED` against a PTY fails `ENOTTY`
/// (confirmed empirically during this task — see `src/port_io.rs`'s
/// module docs), so a real macOS round-trip needs actual hardware
/// (`docs/manual-checklist.md` §1).
#[cfg(target_os = "linux")]
#[test]
fn linux_real_ioctl_round_trip_accepts_74880_via_bother() {
    use std::os::fd::AsRawFd;

    let device = MockDevice::new().expect("open mock device");
    let file = device.open_slave().expect("open slave");
    let fd = file.as_raw_fd();

    let config = PortConfig {
        baud: 74_880,
        ..PortConfig::default()
    };
    serialwrapd::port_io::apply_termios(fd, &config)
        .expect("a real Linux PTY must accept BOTHER + an arbitrary baud rate via TCSETS2");

    let mut t: libc::termios2 = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TCGETS2, &mut t) };
    assert_eq!(
        rc,
        0,
        "TCGETS2 read-back failed: {:?}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        t.c_ispeed, 74_880,
        "the real ioctl round-trip must preserve 74880 exactly, not round to a standard rate"
    );
    assert_eq!(
        t.c_cflag & libc::CBAUD,
        serialwrapd::port_config::LINUX_BOTHER,
        "CBAUD must read back as the BOTHER selector value after a real TCSETS2 call"
    );
}
