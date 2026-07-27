//! Device identity and hotplug detection (`TASKS.md` T1.1, issue #3).
//!
//! This is the seam that makes the project's core promise — "you never
//! lose the boot log" — actually true: a device that appears must be
//! opened and recording *without any client ever having to be present*.
//! See the wiki's [Architecture — process and lifecycle
//! model](https://github.com/SheldonChangL/serialwrap/wiki/Architecture#process-and-lifecycle-model)
//! and [platform differences
//! table](https://github.com/SheldonChangL/serialwrap/wiki/Architecture#platform-differences)
//! for the authoritative design this module implements.
//!
//! # Scope
//!
//! In scope: device identity ([`DeviceId`]), enumeration ([`DeviceEnumerator`],
//! [`SystemEnumerator`]), polling-based hotplug detection
//! ([`HotplugDetector`]), the `connect`/`disconnect`/`open_failed` events
//! that go with it, and — as of T1.3, issue #5 — port configuration
//! (baud/data bits/parity/stop bits/flow control, DTR/RTS, error counts)
//! layered on top of the same open fd. See [`crate::port_config`] for the
//! pure configuration types/encoding and [`crate::port_io`] for the actual
//! syscalls; this module wires both into the connect/reconnect/disconnect
//! state machine below and exposes [`PortConfigApi`] as the seam later
//! tasks (T1.4's UDS query layer) call into.
//!
//! Explicitly out of scope (later task): the UDS client protocol (T1.4).
//!
//! # Device identity
//!
//! An id is USB VID:PID + serial number when all three are available
//! (`usb-<vid>_<pid>_<serial>`, e.g. `usb-1a86_7523_A5069RR4` — this exact
//! format is the wiki's storage-layout example). This is what survives
//! `ttyUSB0 -> ttyUSB1` renumbering across a replug: the id never looks at
//! the device path. When there's no USB metadata at all (a raw UART on a
//! GPIO header) *or* the USB metadata is missing a serial number (some
//! adapters report `None` here — see [`SystemEnumerator`]'s docs), the id
//! falls back to the device path (`path-<sanitized path>`) and is marked as
//! such via [`DeviceId::is_path_based`] — callers must not assume a
//! path-based id survives a reconnect, because it can't: it *is* the path.
//!
//! # Hotplug detection
//!
//! Polling, not IOKit/udev: one implementation covers both platforms, and
//! the default 150ms interval is comfortably inside the 200ms ceiling the
//! issue sets and the wider 300ms open-latency budget. Event-driven
//! detection is a deliberate v2 optimisation, not a v1 correctness
//! requirement (same reasoning as the wiki's Architecture page).
//!
//! Two independent signals feed the same state machine, matching the
//! wiki's failure-modes table ("Read error triggers a `disconnect` event;
//! the poller resumes searching"):
//!
//! - A per-device reader thread's `read()` returning `Ok(0)` or `Err` is
//!   the primary, low-latency disconnect signal. The reader polls with a
//!   short timeout ([`READER_POLL_TIMEOUT_MS`]) rather than calling a
//!   plain blocking `read()`, specifically so it can also be told to stop
//!   promptly and exit — the guarantee (see [`stop_and_join_reader`]) that
//!   at most one reader is ever active for a given device, which matters
//!   because two concurrently-open fds on the same device would otherwise
//!   split incoming bytes between them unpredictably.
//! - A device's id no longer appearing in an enumeration snapshot is a
//!   backup signal, for transports where a read error never arrives (e.g.
//!   idle raw UARTs that simply stop being enumerated).
//!
//! A device reappearing (same id, any path) after a disconnect reuses its
//! existing [`Recorder`] — same directory, same event stream — rather than
//! creating a new one, which is what makes path drift a non-event for
//! identity. A retry at the *same* path right after a disconnect is
//! throttled ([`RECONNECT_COOLDOWN`]) so a device that opens fine but
//! can't sustain a connection (e.g. a tty left with problematic termios by
//! a previous process — this task deliberately doesn't configure termios,
//! see above) can't spin connect/disconnect events forever; a path change
//! is never throttled.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::device_profile::{self, DeviceProfile, ProfileStore};
use crate::error_counts::{self, ErrorCounts};
use crate::port_config::PortConfig;
use crate::port_io::{self, ControlLine};
use crate::recorder::{Recorder, RecorderConfig};

/// Stable identifier for a serial device.
///
/// Derived from USB VID:PID + serial number where available, falling back
/// to the device path otherwise, so identity survives `ttyUSB0 -> ttyUSB1`
/// renumbering across replugs. See the module docs for exactly when each
/// form applies. Also used verbatim as a filesystem directory name by
/// [`Recorder::open`], which is why every constructor below sanitizes its
/// input (see [`sanitize_component`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Build a USB-based id from vid/pid/serial. Returns `None` when there's
    /// no serial number to anchor on — some USB-serial adapters report
    /// `None` for `serial_number` (see [`SystemEnumerator`]'s docs) — in
    /// which case VID:PID alone is not a reliable per-device identifier
    /// (two identical boards without a serial would collide), so the
    /// caller should fall back to [`DeviceId::from_path`] instead. This is
    /// exactly what [`DeviceId::for_device`] does.
    pub fn from_usb(usb: &UsbMetadata) -> Option<DeviceId> {
        let serial = usb.serial_number.as_deref()?;
        if serial.trim().is_empty() {
            return None;
        }
        Some(DeviceId(format!(
            "usb-{:04x}_{:04x}_{}",
            usb.vid,
            usb.pid,
            sanitize_component(serial)
        )))
    }

    /// Fallback id built from the device path alone. Used when there's no
    /// USB metadata at all (a raw UART on a GPIO header) or no serial
    /// number to make VID:PID unique. Explicitly *not* stable across a
    /// replug — `ttyUSB0` and `ttyUSB1` produce different ids, because the
    /// path *is* the id here. The `path-` prefix is how this is "明確標示"
    /// (marked in the id itself) per this task's spec; check
    /// [`DeviceId::is_path_based`] before assuming identity survives a
    /// reconnect.
    pub fn from_path(path: &Path) -> DeviceId {
        DeviceId(format!(
            "path-{}",
            sanitize_component(&path.to_string_lossy())
        ))
    }

    /// The id for one enumerated device: USB-based when possible ([`DeviceId::from_usb`]),
    /// else the path-based fallback ([`DeviceId::from_path`]).
    pub fn for_device(device: &EnumeratedDevice) -> DeviceId {
        device
            .usb
            .as_ref()
            .and_then(DeviceId::from_usb)
            .unwrap_or_else(|| DeviceId::from_path(&device.path))
    }

    /// `true` if this id was built from [`DeviceId::from_path`] — i.e. it is
    /// *not* guaranteed stable across a replug, unlike the USB-based form.
    pub fn is_path_based(&self) -> bool {
        self.0.starts_with("path-")
    }
}

/// Make a string safe to use as one filesystem path component
/// (`Recorder::open` builds `<data_dir>/devices/<device_id>/segments/...`):
/// keep ASCII alphanumerics, `-`, and `.`; replace everything else (spaces,
/// slashes, unicode, control characters, ...) with `_`, then trim any
/// resulting leading underscores for readability. Never returns an empty
/// string (falls back to `"unknown"`), which matters because
/// `Recorder::open` rejects an empty device id outright.
///
/// The character-replacement step alone is deliberately *not* injective —
/// `"AB/CD"`, `"AB_CD"`, and `"AB CD"` all clean to the same string, and
/// any two non-ASCII-alphanumeric serials both collapse toward `"unknown"`
/// — which would otherwise let two genuinely different devices collide on
/// one directory. So whenever cleaning actually changed the input, an 8-hex
/// -digit FNV-1a hash of the *original* (unsanitized) bytes is appended to
/// disambiguate; an already-clean input (the common case — the wiki's own
/// `A5069RR4`-style examples need no changes) is returned untouched, with
/// no hash suffix, so the common-case format stays exactly the wiki's
/// documented example.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches('_');
    let base = if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    };
    if base == s {
        base.to_string()
    } else {
        format!("{base}-{:08x}", fnv1a32(s.as_bytes()))
    }
}

/// Small, dependency-free FNV-1a 32-bit hash — only used to disambiguate
/// [`sanitize_component`]'s otherwise-lossy character replacement, not for
/// anything security-sensitive.
fn fnv1a32(bytes: &[u8]) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// USB metadata for one enumerated device, as reported by
/// `serialport::available_ports()`'s `SerialPortType::UsbPort`.
///
/// `serial_number` is `Option` on purpose: several common USB-serial
/// adapters (notably some CH340 clones) don't program a serial number into
/// their EEPROM, and `serialport`'s Linux/macOS backends both surface that
/// as `None` rather than an empty string — see [`SystemEnumerator`]'s docs
/// for where this was verified against the crate's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbMetadata {
    pub vid: u16,
    pub pid: u16,
    pub serial_number: Option<String>,
}

/// One device as reported by a [`DeviceEnumerator`]: a path plus whatever
/// USB metadata (if any) came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumeratedDevice {
    pub path: PathBuf,
    pub usb: Option<UsbMetadata>,
}

/// The seam between "what devices exist right now" and everything that
/// consumes that answer ([`HotplugDetector`]).
///
/// [`SystemEnumerator`] is the real implementation, backed by
/// `serialport::available_ports()`. [`testing::ScriptedEnumerator`] is the
/// test double: a scripted, in-memory device list that can simulate
/// appearance, disappearance, and path drift (`ttyUSB0 -> ttyUSB1`)
/// without any real hardware — real USB hotplug cannot be reproduced in CI,
/// so this trait is what makes this task's acceptance criteria testable at
/// all.
pub trait DeviceEnumerator: Send {
    fn enumerate(&mut self) -> io::Result<Vec<EnumeratedDevice>>;
}

/// Which platform-specific enumeration rules to apply. A plain enum
/// (rather than gating logic behind `#[cfg(target_os = ...)]`) so
/// [`filter_platform`] and [`describe_open_error`] stay unit-testable with
/// fake data on *any* CI runner — including asserting the macOS `cu`/`tty`
/// filtering and the Linux dialout/udev error message on a Linux runner,
/// and vice versa (acceptance criteria 5 and 6 both require this).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Other,
}

/// The platform this process is actually running on.
fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}

/// Host-independent filtering pass over raw enumeration results.
///
/// `serialport::available_ports()` on macOS reports *both*
/// `IOCalloutDevice` (`/dev/cu.*`) and `IODialinDevice` (`/dev/tty.*`) as
/// separate entries for the very same physical device (confirmed against
/// serialport-rs's `src/posix/enumerate.rs`, which pushes one
/// `SerialPortInfo` per key in `["IOCalloutDevice", "IODialinDevice"]` from
/// the same IOKit service). Opening the `tty.*` node blocks waiting for
/// DCD — the classic macOS serial footgun this project's spec calls out by
/// name — so it must never be the path the daemon opens, and it must not
/// survive to look like "the same device at two paths simultaneously" to
/// [`HotplugDetector`]. Linux/other platforms are passed through unchanged;
/// `serialport`'s own backends there already filter to recognized tty
/// subsystems.
pub fn filter_platform(
    devices: Vec<EnumeratedDevice>,
    platform: Platform,
) -> Vec<EnumeratedDevice> {
    match platform {
        Platform::MacOs => devices
            .into_iter()
            .filter(|d| {
                !d.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("tty."))
            })
            .collect(),
        Platform::Linux | Platform::Other => devices,
    }
}

/// Production [`DeviceEnumerator`]: wraps `serialport::available_ports()`
/// and applies [`filter_platform`] for the host platform.
///
/// Behavior actually observed by reading serialport-rs's source
/// (`src/posix/enumerate.rs`), since this daemon has no real USB hardware
/// available in this development environment to verify against directly:
///
/// - **macOS**: IOKit-backed, matches `kIOSerialBSDServiceValue`. Emits one
///   entry per `IOCalloutDevice` (`cu.*`) *and* one per `IODialinDevice`
///   (`tty.*`) for the same device — see [`filter_platform`]. USB metadata
///   comes from walking up to the parent `IOUSBHostInterface`/legacy USB
///   device and reading `idVendor`/`idProduct`/`USB Serial Number` — the
///   last of which is a plain `Option` (absent devices just don't have the
///   property).
/// - **Linux**: with the `libudev` feature (this workspace disables it —
///   see the root `Cargo.toml` comment — so this path doesn't apply here,
///   but is documented for completeness): reads `ID_VENDOR_ID`/
///   `ID_MODEL_ID`/`ID_SERIAL_SHORT` udev properties, `serial_number` is
///   `None` when `ID_SERIAL_SHORT` isn't set. Without `libudev` (this
///   workspace's configuration): scans `/sys/class/tty/*/device`, follows
///   the `subsystem` symlink, and for `usb`/`usb-serial` reads
///   `idVendor`/`idProduct`/`serial` (again `None` when the `serial` sysfs
///   file doesn't exist) straight from sysfs — same fields, no udev
///   dependency.
///
/// In both cases `serial_number: None` is a real, expected outcome (not an
/// error) for USB-serial adapters that never had a serial number
/// programmed into them — [`DeviceId::for_device`] falls back to a
/// path-based id for exactly this case.
#[derive(Debug, Default)]
pub struct SystemEnumerator;

impl SystemEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceEnumerator for SystemEnumerator {
    fn enumerate(&mut self) -> io::Result<Vec<EnumeratedDevice>> {
        let ports = serialport::available_ports().map_err(to_io_error)?;
        let raw: Vec<EnumeratedDevice> = ports
            .into_iter()
            .map(|p| EnumeratedDevice {
                path: PathBuf::from(p.port_name),
                usb: match p.port_type {
                    serialport::SerialPortType::UsbPort(info) => Some(UsbMetadata {
                        vid: info.vid,
                        pid: info.pid,
                        serial_number: info.serial_number,
                    }),
                    _ => None,
                },
            })
            .collect();
        Ok(filter_platform(raw, current_platform()))
    }
}

fn to_io_error(e: serialport::Error) -> io::Error {
    match e.kind() {
        serialport::ErrorKind::Io(kind) => io::Error::new(kind, e.to_string()),
        _ => io::Error::other(e.to_string()),
    }
}

/// Default hotplug poll interval: within the issue's "≤200ms" requirement,
/// leaving headroom under the 300ms open-latency budget for the
/// open()/`Recorder::open()` work that follows each detected appearance
/// (worst case ≈ one interval + that overhead — see the acceptance test
/// that measures this).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often a per-device reader thread wakes up (even with no data ready)
/// to re-check whether it's been told to stop. This bounds how long
/// [`stop_and_join_reader`] can block: any state transition away from
/// `Connected` (a real disconnect noticed via the backup enumeration
/// signal, or a path change superseding an existing connection) waits at
/// most this long for the outgoing reader to actually exit before it's
/// safe to open a fresh fd for the same device — which is what guarantees
/// at most one active reader per device at a time.
const READER_POLL_TIMEOUT_MS: u16 = 100;

/// Minimum time to wait before retrying an open at the *same* path right
/// after a disconnect. Without this, a device that opens successfully but
/// then immediately hits EOF on every read — e.g. a real tty left with
/// `VMIN=0`-style termios by a crashed previous process — would spin
/// connect/disconnect events and reader threads forever (this module's own
/// `port_io::apply_termios` sets `VMIN=1`/`VTIME=0` on every successful
/// connect, but a device whose config application failed, or one this
/// process has never successfully opened before, may still be left in
/// whatever state a previous process — or nothing at all — configured). A
/// path change (genuine drift) is never subject to this cooldown; only a
/// retry at the exact same path is throttled.
const RECONNECT_COOLDOWN: Duration = Duration::from_millis(500);

/// Tunable knobs for a [`HotplugDetector`].
#[derive(Debug, Clone)]
pub struct HotplugConfig {
    pub poll_interval: Duration,
    pub recorder_config: RecorderConfig,
}

impl Default for HotplugConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            recorder_config: RecorderConfig::default(),
        }
    }
}

/// Human-actionable description of a failure to `open()` a device's path:
/// `(reason_code, message)`. `reason_code` is a short machine-readable tag
/// stored in the `open_failed` event's `reason` field; `message` is what a
/// human (CLI/GUI) should be shown. Per this task's spec, a permission
/// failure must name the concrete fix — `dialout` group / udev rule on
/// Linux, driver install on macOS — not just repeat "permission denied".
///
/// Takes `platform` as a parameter (rather than reading `cfg!(target_os)`
/// internally) so both branches are unit-testable on *any* CI runner —
/// acceptance criterion 5 requires asserting the Linux dialout/udev wording
/// specifically, which must run even on the macOS CI leg.
fn describe_open_error(err: &io::Error, path: &Path, platform: Platform) -> (String, String) {
    let path = path.display();
    if err.kind() == io::ErrorKind::PermissionDenied {
        let message = match platform {
            Platform::Linux => format!(
                "permission denied opening {path}: add your user to the `dialout` group \
                 (`sudo usermod -aG dialout $USER`, then log out and back in) or install a udev \
                 rule granting access (e.g. `SUBSYSTEM==\"tty\", GROUP=\"dialout\", MODE=\"0660\"` \
                 in /etc/udev/rules.d/, then `sudo udevadm control --reload-rules && sudo udevadm \
                 trigger`)"
            ),
            Platform::MacOs => format!(
                "permission denied opening {path}: this is usually a missing or unapproved \
                 USB-serial driver (e.g. CH340/CP210x) — install the vendor driver and allow it \
                 under System Settings > Privacy & Security, then replug the device"
            ),
            Platform::Other => format!("permission denied opening {path}"),
        };
        return ("permission_denied".to_string(), message);
    }
    if err.raw_os_error() == Some(libc::EBUSY) {
        return (
            "busy".to_string(),
            format!(
                "{path} is already open by another process — serialwrap requires exclusive \
                 access to the device; check for another serialwrapd instance or another program \
                 (Arduino IDE, screen, minicom, ...) holding the port open"
            ),
        );
    }
    (
        "io_error".to_string(),
        format!("failed to open {path}: {err}"),
    )
}

/// Append a `connect` event: `device_id`, `path`, whether the id is
/// USB-based or path-based, and (when available) the USB metadata itself —
/// per this task's spec ("connect：含 device_id、path、以及 USB metadata").
fn append_connect_event(
    recorder: &Recorder,
    id: &DeviceId,
    path: &Path,
    usb: Option<&UsbMetadata>,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("device_id".to_string(), id.0.clone().into());
    extra.insert(
        "path".to_string(),
        path.to_string_lossy().into_owned().into(),
    );
    extra.insert(
        "id_kind".to_string(),
        (if id.is_path_based() { "path" } else { "usb" }).into(),
    );
    if let Some(usb) = usb {
        extra.insert("vid".to_string(), usb.vid.into());
        extra.insert("pid".to_string(), usb.pid.into());
        extra.insert(
            "serial_number".to_string(),
            match &usb.serial_number {
                Some(s) => s.clone().into(),
                None => Value::Null,
            },
        );
    }
    recorder.append_event("connect", extra)?;
    Ok(())
}

/// Append a `disconnect` event: `device_id`, `path`, `reason`.
fn append_disconnect_event(
    recorder: &Recorder,
    id: &DeviceId,
    path: &Path,
    reason: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("device_id".to_string(), id.0.clone().into());
    extra.insert(
        "path".to_string(),
        path.to_string_lossy().into_owned().into(),
    );
    extra.insert("reason".to_string(), reason.into());
    recorder.append_event("disconnect", extra)?;
    Ok(())
}

/// Append an `open_failed` event: `device_id`, `path`, a short `reason`
/// code, and a human-actionable `message` (see [`describe_open_error`]).
/// Not one of the event kinds the wiki's Event-stream-and-storage page
/// enumerates yet — that page notes the schema "will keep growing new
/// event shapes" via `extra`'s forward-compatible flattening, and this
/// task's own acceptance criteria require a distinct event for open
/// failures (permission denied, device busy), so this is that addition.
fn append_open_failed_event(
    recorder: &Recorder,
    id: &DeviceId,
    path: &Path,
    reason: &str,
    message: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("device_id".to_string(), id.0.clone().into());
    extra.insert(
        "path".to_string(),
        path.to_string_lossy().into_owned().into(),
    );
    extra.insert("reason".to_string(), reason.into());
    extra.insert("message".to_string(), message.into());
    recorder.append_event("open_failed", extra)?;
    Ok(())
}

/// Append a `lease_start` event: `device_id`, `path`, `command`, `pid` (the
/// kernel-verified pid of the connection that requested the lease — see
/// `protocol::session`'s `Request::LeaseAcquire` handler, not the pid of
/// whatever the caller eventually execs, which the daemon has no way to
/// know at acquire time), `token`, and `timeout_s` (`null` when none was
/// given). `pub(crate)` rather than private: both this module's own lease
/// state machine and `protocol::backend`'s `TestBackend` (a second,
/// in-memory `DeviceBackend` impl used by tests) append this exact same
/// event shape, so the schema can't drift between the two (`TASKS.md`
/// T2.2, issue #9).
pub(crate) fn append_lease_start_event(
    recorder: &Recorder,
    id: &DeviceId,
    path: &Path,
    command: &str,
    pid: u32,
    token: &str,
    timeout_s: Option<f64>,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("device_id".to_string(), id.0.clone().into());
    extra.insert(
        "path".to_string(),
        path.to_string_lossy().into_owned().into(),
    );
    extra.insert("command".to_string(), command.into());
    extra.insert("pid".to_string(), pid.into());
    extra.insert("token".to_string(), token.into());
    extra.insert(
        "timeout_s".to_string(),
        match timeout_s {
            Some(s) => s.into(),
            None => Value::Null,
        },
    );
    recorder.append_event("lease_start", extra)?;
    Ok(())
}

/// Append a `lease_end` event: everything `lease_start` carries, plus
/// `exit_code` (`null` for a daemon-initiated reclaim — a `timeout` or a
/// residual lease found at startup, neither of which ever learns the
/// child's real exit status), `duration_ms`, and `reason` (`"released"`,
/// `"timeout"`, or `"daemon_restart"`). See [`append_lease_start_event`]
/// for why this is `pub(crate)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_lease_end_event(
    recorder: &Recorder,
    id: &DeviceId,
    path: Option<&Path>,
    command: &str,
    pid: u32,
    token: &str,
    exit_code: Option<i32>,
    duration_ms: u64,
    reason: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("device_id".to_string(), id.0.clone().into());
    if let Some(path) = path {
        extra.insert(
            "path".to_string(),
            path.to_string_lossy().into_owned().into(),
        );
    }
    extra.insert("command".to_string(), command.into());
    extra.insert("pid".to_string(), pid.into());
    extra.insert("token".to_string(), token.into());
    extra.insert(
        "exit_code".to_string(),
        match exit_code {
            Some(c) => c.into(),
            None => Value::Null,
        },
    );
    extra.insert("duration_ms".to_string(), duration_ms.into());
    extra.insert("reason".to_string(), reason.into());
    recorder.append_event("lease_end", extra)?;
    Ok(())
}

/// Monotonically increasing counter backing [`generate_lease_token`] — only
/// needs to be unique within one daemon process's lifetime (tokens are
/// never persisted across a restart in a way that requires global
/// uniqueness; see [`PersistedLease`]), so a plain counter plus this
/// process's pid is sufficient without pulling in a UUID dependency.
static LEASE_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, opaque lease token, unique for this daemon process's lifetime.
fn generate_lease_token() -> String {
    let n = LEASE_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("lease-{}-{n}", std::process::id())
}

/// One device's currently-active lease, tracked only inside the poll
/// thread's own state ([`HotplugDetector::leases`]) — never shared behind a
/// `Mutex`, unlike [`SharedDeviceConfigs`]/[`SharedRecorders`] — because
/// acquiring/releasing a lease must run on the one thread that already owns
/// `tracked`/[`stop_and_join_reader`]/`attempt_open`'s reconnect machinery
/// (see [`LeaseCommand`] for how a call from *outside* that thread reaches
/// it).
struct ActiveLease {
    token: String,
    command: String,
    pid: u32,
    started: Instant,
    /// `None` when no `--lease-timeout`-equivalent was given — the lease
    /// stays open until an explicit `LeaseCommand::Release`.
    deadline: Option<Instant>,
}

/// On-disk record of an in-progress lease, at
/// `<data_dir>/devices/<device_id>/lease.json` — written when a lease is
/// acquired, deleted on a clean release. [`ActiveLease`] (in-memory,
/// poll-thread-private) cannot survive a daemon crash/restart; this file is
/// what lets a *fresh* `HotplugDetector` instance (a new daemon process,
/// same `data_dir`) recognize "the previous process died while this device
/// was leased" the first time it sees the device again (see
/// [`HotplugDetector::recover_residual_lease`]) — the edge case `TASKS.md`
/// T2.2 calls out by name ("daemon 自己重啟等邊角：啟動時檢查殘留 lease 並
/// 收回"). `started_epoch_ms` (wall-clock, not `Instant`) is what makes
/// `duration_ms` computable across that restart — `Instant` has no stable
/// epoch and a fresh process can't compare its own `Instant`s against a
/// previous process's.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedLease {
    token: String,
    command: String,
    pid: u32,
    started_epoch_ms: u64,
    timeout_s: Option<f64>,
}

/// Successful result of [`PortConfigApi::acquire_lease`]: the opaque token
/// a later `release_lease` call identifies this lease by, and the device's
/// current path — the caller (`serialwrap run`) needs this to hand to
/// whatever external tool it spawns against the now-freed port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAcquired {
    pub token: String,
    pub path: PathBuf,
}

/// Successful result of [`PortConfigApi::release_lease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseReleased {
    pub duration_ms: u64,
}

/// Everything that can go wrong acquiring/releasing a lease. Deliberately
/// its own type rather than reusing `io::Error`/`io::ErrorKind`: unlike
/// every other `PortConfigApi` method (which map cleanly onto
/// `NotFound`/`NotConnected`), a lease call has a third failure mode with
/// no matching `io::ErrorKind` — "a lease is already active" — and the wire
/// layer needs to tell it apart from the other two to return the
/// wiki-documented `lease_held` error code (see
/// `protocol::session::lease_error_to_wire`) rather than a generic one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// No such device id has ever been seen by this detector.
    UnknownDevice,
    /// The device is known but not currently connected — there is no fd to
    /// hand off.
    NotConnected,
    /// This device already has an active lease. Carries a human-readable
    /// description of the current holder (`pid`/`command`) — the wiki's
    /// `lease_held` error code documents a `holder` field precisely so a
    /// client sees *who* to blame, not just that it lost a race.
    AlreadyLeased { holder: String },
    /// `release_lease` was called with a token that doesn't match any
    /// currently-active lease (already released, already timed out, or
    /// simply never existed).
    UnknownToken,
    /// An I/O failure while acquiring/releasing (e.g. reopening the device
    /// after release failed outright) or while talking to the poll thread
    /// itself.
    Io(String),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::UnknownDevice => write!(f, "unknown device"),
            LeaseError::NotConnected => write!(f, "device is not currently connected"),
            LeaseError::AlreadyLeased { holder } => {
                write!(f, "device already has an active lease (held by {holder})")
            }
            LeaseError::UnknownToken => write!(f, "no active lease for that token"),
            LeaseError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// A lease request submitted from outside the poll thread (via
/// [`PortConfigApi::acquire_lease`]/[`PortConfigApi::release_lease`]) and
/// processed synchronously inside it (see
/// [`HotplugDetector::drain_lease_commands`]).
///
/// # Why a channel, not a shared `Mutex`
///
/// Acquiring a lease means stopping and joining a device's reader thread
/// and clearing its live fd — exactly what [`stop_and_join_reader`] and
/// [`HotplugDetector::clear_live_fd`] already do for a supersede/reconnect,
/// both of which assume `&mut self.tracked` (poll-thread-private, moved
/// into the background thread by [`HotplugDetector::spawn`]). Releasing a
/// lease means reopening the device and re-arming a fresh reader thread —
/// again `attempt_open`-shaped work needing the same `&mut self.tracked`
/// plus `self.disconnect_tx`. Routing both through a command channel the
/// poll thread already drains once per tick (same pattern as
/// `DisconnectNotice`) reuses that existing, already-tested machinery
/// as-is, rather than duplicating it behind a second lock or restructuring
/// `ConnectionState` to be `Mutex`-shared just for this one feature.
enum LeaseCommand {
    Acquire {
        id: DeviceId,
        command: String,
        pid: u32,
        timeout_s: Option<f64>,
        reply: mpsc::Sender<Result<LeaseAcquired, LeaseError>>,
    },
    Release {
        token: String,
        exit_code: i32,
        reply: mpsc::Sender<Result<LeaseReleased, LeaseError>>,
    },
}

/// A per-device reader thread's report that it stopped reading (EOF or a
/// real I/O error) — the primary disconnect signal (see module docs).
struct DisconnectNotice {
    device_id: DeviceId,
    reason: String,
}

enum ConnectionState {
    /// `stop`/`reader` are the handle to the currently-running per-device
    /// reader thread — see [`stop_and_join_reader`], which is the only way
    /// this variant should ever be replaced, so that at most one reader is
    /// ever active for a given device at a time.
    Connected {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        reader: JoinHandle<()>,
    },
    /// `since`/`last_path` back [`RECONNECT_COOLDOWN`]'s same-path retry
    /// throttle — see its docs.
    Disconnected { since: Instant, last_path: PathBuf },
    /// The last attempt to open this (already-known) device's path failed.
    /// `last_reason` lets [`HotplugDetector`] debounce repeated identical
    /// failures (e.g. a persistent permission error) into a single event
    /// instead of one every poll tick, while still re-reporting if the
    /// failure reason *changes*.
    OpenFailed { last_reason: String },
}

struct TrackedDevice {
    recorder: Arc<Recorder>,
    state: ConnectionState,
}

/// What to do about one device_id present in the latest enumeration
/// snapshot, decided by [`HotplugDetector::reconcile_present`] purely from
/// its current tracked state (see that method for the borrow-scoping
/// reason this is a separate, plain-data decision step).
enum PresentAction {
    New,
    Retry,
    SupersedeConnected,
    AlreadyConnected,
    CooldownSkip,
}

/// Signal an outgoing `Connected` reader thread to stop and wait
/// (bounded by [`READER_POLL_TIMEOUT_MS`]) for it to actually exit before
/// returning. This is the guarantee that makes it safe to open a fresh fd
/// for the same device immediately afterward without risking two
/// concurrently-active readers splitting incoming bytes between them (each
/// would get whichever chunks the kernel happens to schedule its `read()`
/// to see). No-op for any other state.
fn stop_and_join_reader(state: ConnectionState) {
    if let ConnectionState::Connected { stop, reader, .. } = state {
        stop.store(true, Ordering::Relaxed);
        let _ = reader.join();
    }
}

/// Thread-safe, shared view of every device's [`Recorder`] a
/// [`HotplugDetector`] has ever opened, keyed by [`DeviceId`]. This is the
/// seam later tasks (T1.4's query layer) look devices up through, and what
/// this module's own tests use to assert on recorded events from outside
/// the detector.
pub type SharedRecorders = Arc<Mutex<HashMap<DeviceId, Arc<Recorder>>>>;

/// One device's live configuration state: its persisted [`DeviceProfile`]
/// plus, only while actually `Connected`, the shared fd config operations
/// apply to. Kept in its own map (rather than folded into `TrackedDevice`,
/// which stays private to the poll thread) specifically so it can be
/// wrapped in an `Arc<Mutex<_>>` and handed out via [`PortConfigApi`] to
/// callers outside the poll loop — e.g. a future T1.4 UDS client-handler
/// thread — the same way [`SharedRecorders`] already is.
struct LiveDeviceConfig {
    profile: DeviceProfile,
    /// `None` whenever the device isn't currently `Connected`. A config
    /// change made while disconnected still updates `profile` and the
    /// on-disk [`ProfileStore`]; it just has no live fd to re-apply to
    /// until the device reconnects (see `HotplugDetector::attempt_open`).
    fd: Option<Arc<File>>,
    /// The path last seen for this device (T1.4's `list_devices`): set on
    /// first sight and updated on every successful (re)connect, left
    /// untouched across a disconnect so a currently-unplugged device still
    /// reports where it was last seen. Not authoritative for identity —
    /// see the module docs on why `DeviceId` never keys off this.
    path: Option<PathBuf>,
}

/// Thread-safe, shared view of every known device's live configuration
/// state, keyed by [`DeviceId`]. See [`LiveDeviceConfig`] and
/// [`PortConfigApi`].
type SharedDeviceConfigs = Arc<Mutex<HashMap<DeviceId, LiveDeviceConfig>>>;

/// Polls a [`DeviceEnumerator`] and drives the connect/disconnect/
/// open-failed state machine described in the module docs.
///
/// Call [`HotplugDetector::poll_once`] directly for deterministic,
/// sleep-free tests (repeatedly call it in a tight loop until the expected
/// state is reached), or [`HotplugDetector::spawn`] to run it on its own
/// background thread at [`HotplugConfig::poll_interval`] cadence, which is
/// what the daemon itself will do.
pub struct HotplugDetector {
    enumerator: Box<dyn DeviceEnumerator>,
    data_dir: PathBuf,
    platform: Platform,
    config: HotplugConfig,
    tracked: HashMap<DeviceId, TrackedDevice>,
    disconnect_tx: mpsc::Sender<DisconnectNotice>,
    disconnect_rx: mpsc::Receiver<DisconnectNotice>,
    recorders: SharedRecorders,
    configs: SharedDeviceConfigs,
    profiles: Arc<ProfileStore>,
    /// Lease requests submitted from outside the poll thread — see
    /// [`LeaseCommand`].
    lease_tx: mpsc::Sender<LeaseCommand>,
    lease_rx: mpsc::Receiver<LeaseCommand>,
    /// Devices with an active lease right now. Poll-thread-private (never
    /// shared behind a `Mutex`) for the same reason `tracked` is — see
    /// [`LeaseCommand`]'s docs. [`reconcile_present`]/[`reconcile_missing`]
    /// skip any device_id present here entirely, which is what stops the
    /// normal hotplug machinery from racing a lease holder to reopen the
    /// port out from under it.
    leases: HashMap<DeviceId, ActiveLease>,
}

impl HotplugDetector {
    pub fn new(
        enumerator: Box<dyn DeviceEnumerator>,
        data_dir: impl Into<PathBuf>,
        config: HotplugConfig,
    ) -> Self {
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let (lease_tx, lease_rx) = mpsc::channel();
        let data_dir = data_dir.into();
        let profiles = Arc::new(ProfileStore::new(data_dir.clone()));
        Self {
            enumerator,
            data_dir,
            platform: current_platform(),
            config,
            tracked: HashMap::new(),
            disconnect_tx,
            disconnect_rx,
            recorders: Arc::new(Mutex::new(HashMap::new())),
            configs: Arc::new(Mutex::new(HashMap::new())),
            profiles,
            lease_tx,
            lease_rx,
            leases: HashMap::new(),
        }
    }

    /// Shared, thread-safe view of every device's [`Recorder`] this
    /// detector has ever opened. See [`SharedRecorders`].
    pub fn recorders(&self) -> SharedRecorders {
        Arc::clone(&self.recorders)
    }

    /// The seam T1.3's config API is exposed through — see
    /// [`PortConfigApi`]. Safe to call and clone freely, before or after
    /// [`Self::spawn`]; every clone shares the same underlying state.
    pub fn port_config_api(&self) -> PortConfigApi {
        PortConfigApi {
            configs: Arc::clone(&self.configs),
            recorders: Arc::clone(&self.recorders),
            profiles: Arc::clone(&self.profiles),
            lease_tx: self.lease_tx.clone(),
        }
    }

    /// Run exactly one enumerate-and-reconcile cycle: drain pending
    /// disconnect notices and lease commands, reclaim any lease whose
    /// deadline has passed, enumerate current devices, open/reconnect any
    /// that need it, and mark anything no longer enumerated as
    /// disconnected. Safe to call directly in a tight loop from tests —
    /// see the module docs.
    pub fn poll_once(&mut self) -> io::Result<()> {
        self.drain_disconnect_notices();
        self.drain_lease_commands();
        self.reclaim_expired_leases();

        let snapshot = self.enumerator.enumerate()?;
        let mut seen = HashSet::with_capacity(snapshot.len());
        for dev in snapshot {
            let id = DeviceId::for_device(&dev);
            seen.insert(id.clone());
            self.reconcile_present(id, dev);
        }
        self.reconcile_missing(&seen);
        Ok(())
    }

    /// Run [`Self::poll_once`] on a background thread at
    /// [`HotplugConfig::poll_interval`] cadence until [`DetectorHandle::stop`]
    /// is called (or the handle is dropped).
    pub fn spawn(mut self) -> DetectorHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let recorders = Arc::clone(&self.recorders);
        let configs = Arc::clone(&self.configs);
        let profiles = Arc::clone(&self.profiles);
        let lease_tx = self.lease_tx.clone();
        let poll_interval = self.config.poll_interval;

        let join = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                // `catch_unwind` rather than letting a panic kill this
                // thread outright: `serialport::available_ports()`'s
                // no-libudev Linux fallback panics (via `.expect(...)` on
                // a missing `/sys/class/tty/`) rather than returning `Err`
                // for at least one failure mode, and a silently-dead poll
                // thread would leave the daemon looking alive while never
                // detecting another hotplug event again. Catching here
                // keeps the loop — and therefore hotplug detection — self-
                // healing across a single bad poll instead of forever.
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.poll_once()));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        eprintln!("serialwrapd: port: enumeration failed: {e}");
                    }
                    Err(_) => {
                        eprintln!(
                            "serialwrapd: port: a poll cycle panicked; continuing to poll \
                             (this indicates a bug in the enumerator or its dependencies)"
                        );
                    }
                }
                thread::sleep(poll_interval);
            }
        });

        DetectorHandle {
            stop,
            join: Some(join),
            recorders,
            configs,
            profiles,
            lease_tx,
        }
    }

    fn reconcile_present(&mut self, id: DeviceId, dev: EnumeratedDevice) {
        // A leased device is left entirely alone: it's still physically
        // enumerated (nothing about a lease changes what's plugged in), but
        // the poll loop must not reopen it out from under whatever
        // currently holds the port — see `leases`' docs and `LeaseCommand`.
        if self.leases.contains_key(&id) {
            return;
        }
        // Phase 1: decide what to do — this borrows `self.tracked`
        // immutably and produces a plain, borrow-free `PresentAction`, so
        // phase 2 below is free to take `&mut self` without any conflict.
        let action = match self.tracked.get(&id).map(|t| &t.state) {
            None => PresentAction::New,
            Some(ConnectionState::Connected { path, .. }) => {
                if *path == dev.path {
                    PresentAction::AlreadyConnected
                } else {
                    PresentAction::SupersedeConnected
                }
            }
            Some(ConnectionState::OpenFailed { .. }) => PresentAction::Retry,
            Some(ConnectionState::Disconnected { since, last_path }) => {
                if *last_path == dev.path && since.elapsed() < RECONNECT_COOLDOWN {
                    PresentAction::CooldownSkip
                } else {
                    PresentAction::Retry
                }
            }
        };

        // Phase 2: act.
        match action {
            PresentAction::New => self.handle_new_device(id, dev),
            PresentAction::Retry => self.attempt_open(id, dev),
            PresentAction::SupersedeConnected => {
                // The enumerator reports a *different* path for a
                // device_id we think is still connected. A real replug
                // normally surfaces as disconnect-then-reconnect (the old
                // fd errors first), but if this ever races ahead of that,
                // treat the new path as superseding: stop the old reader
                // (bounded wait, see `stop_and_join_reader`) and open
                // fresh at the new path, rather than trusting a possibly-
                // stale fd or — worse — leaving it running concurrently
                // with a second reader on the new path.
                if let Some(tracked) = self.tracked.get_mut(&id) {
                    let old = std::mem::replace(
                        &mut tracked.state,
                        ConnectionState::Disconnected {
                            since: Instant::now(),
                            last_path: dev.path.clone(),
                        },
                    );
                    stop_and_join_reader(old);
                }
                self.clear_live_fd(&id);
                self.attempt_open(id, dev);
            }
            PresentAction::AlreadyConnected | PresentAction::CooldownSkip => {}
        }
    }

    fn handle_new_device(&mut self, id: DeviceId, dev: EnumeratedDevice) {
        let recorder =
            match Recorder::open(&self.data_dir, &id.0, self.config.recorder_config.clone()) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    eprintln!(
                        "serialwrapd: port: failed to open recorder directory for {}: {e}",
                        id.0
                    );
                    return;
                }
            };
        self.recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), Arc::clone(&recorder));

        // The first time this detector instance ever sees this device_id
        // is exactly the right (and only) moment to check for a lease left
        // dangling by a previous daemon process against the same
        // `data_dir` — see [`Self::recover_residual_lease`].
        self.recover_residual_lease(&id, &recorder);

        // Load this device's persisted profile (if any) the first time
        // it's ever seen — falls back to `DeviceProfile::default()` for a
        // brand-new device, or if the saved profile fails to load (logged,
        // never fatal to detection: see the module docs' general
        // best-effort stance on config application).
        {
            let mut configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
            if !configs.contains_key(&id) {
                let profile = match self.profiles.load(&id.0) {
                    Ok(Some(p)) => p,
                    Ok(None) => DeviceProfile::default(),
                    Err(e) => {
                        eprintln!(
                            "serialwrapd: port: failed to load saved profile for {} (using \
                             default): {e}",
                            id.0
                        );
                        DeviceProfile::default()
                    }
                };
                configs.insert(
                    id.clone(),
                    LiveDeviceConfig {
                        profile,
                        fd: None,
                        path: Some(dev.path.clone()),
                    },
                );
            }
        }

        self.tracked.insert(
            id.clone(),
            TrackedDevice {
                recorder,
                // Placeholder, immediately overwritten by the
                // unconditional `attempt_open` call below (brand new
                // devices are never subject to the reconnect cooldown).
                state: ConnectionState::Disconnected {
                    since: Instant::now(),
                    last_path: dev.path.clone(),
                },
            },
        );
        self.attempt_open(id, dev);
    }

    fn attempt_open(&mut self, id: DeviceId, dev: EnumeratedDevice) {
        let recorder = Arc::clone(
            &self
                .tracked
                .get(&id)
                .expect("attempt_open is only called for a tracked device")
                .recorder,
        );

        // The config to apply is always this device's *current* live
        // profile — freshly loaded in `handle_new_device` for a brand-new
        // device, or whatever a `PortConfigApi::set_port_config` call
        // updated it to since (persisted, so it survives a full daemon
        // restart too). This is what makes "reconnect re-applies the saved
        // profile" true: the same `DeviceId` always maps to the same
        // config entry, regardless of how many times it disconnects and
        // reconnects (possibly at a different path — see the module docs
        // on device identity).
        let config = {
            let configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
            configs
                .get(&id)
                .map(|c| c.profile.config.clone())
                .unwrap_or_default()
        };

        match port_io::open_and_configure(&dev.path, &config) {
            Ok((file, config_err)) => {
                if let Some(e) = config_err {
                    // Best-effort: see `port_io`'s module docs for why a
                    // config-application failure (e.g. IOSSIOSPEED's
                    // documented, empirically-confirmed ENOTTY against a
                    // PTY, or a fake test device that isn't a tty at all)
                    // must not be treated the same as a failure to open
                    // the device at all — the device is still connected
                    // and still recording raw bytes either way.
                    eprintln!(
                        "serialwrapd: port: config application for {} did not fully apply (device \
                         is still connected and recording): {e}",
                        id.0
                    );
                }
                if let Err(e) = append_connect_event(&recorder, &id, &dev.path, dev.usb.as_ref()) {
                    eprintln!(
                        "serialwrapd: port: failed to append connect event for {}: {e}",
                        id.0
                    );
                }
                if let Err(e) = device_profile::append_config_change_event(
                    &recorder,
                    None,
                    &config,
                    "system:connect",
                ) {
                    eprintln!(
                        "serialwrapd: port: failed to append config_change event for {}: {e}",
                        id.0
                    );
                }

                let file = Arc::new(file);
                if let Some(entry) = self
                    .configs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&id)
                {
                    entry.fd = Some(Arc::clone(&file));
                    entry.path = Some(dev.path.clone());
                }

                let stop = Arc::new(AtomicBool::new(false));
                let reader_stop = Arc::clone(&stop);
                let reader_tx = self.disconnect_tx.clone();
                let reader_id = id.clone();
                let reader_recorder = Arc::clone(&recorder);
                let reader_file = Arc::clone(&file);
                let reader = thread::spawn(move || {
                    run_reader(
                        reader_file,
                        reader_id,
                        reader_recorder,
                        reader_tx,
                        reader_stop,
                    );
                });

                if let Some(tracked) = self.tracked.get_mut(&id) {
                    tracked.state = ConnectionState::Connected {
                        path: dev.path.clone(),
                        stop,
                        reader,
                    };
                }
            }
            Err(e) => {
                let (reason, message) = describe_open_error(&e, &dev.path, self.platform);
                let previously_reported = match self.tracked.get(&id).map(|t| &t.state) {
                    Some(ConnectionState::OpenFailed { last_reason }) => Some(last_reason.clone()),
                    _ => None,
                };
                let should_report = previously_reported.as_deref() != Some(reason.as_str());
                if should_report {
                    if let Err(e2) =
                        append_open_failed_event(&recorder, &id, &dev.path, &reason, &message)
                    {
                        eprintln!(
                            "serialwrapd: port: failed to append open_failed event for {}: {e2}",
                            id.0
                        );
                    }
                }
                if let Some(tracked) = self.tracked.get_mut(&id) {
                    tracked.state = ConnectionState::OpenFailed {
                        last_reason: reason,
                    };
                }
            }
        }
    }

    fn drain_disconnect_notices(&mut self) {
        while let Ok(notice) = self.disconnect_rx.try_recv() {
            // A notice is stale (and safely ignored) if this device_id is
            // no longer `Connected` by the time we drain it — e.g. it was
            // already superseded or backup-disconnected by
            // `reconcile_missing`/`reconcile_present` since the reader
            // thread sent this notice. `stop_and_join_reader` guarantees
            // that transition already fully stopped the old reader, so
            // there is nothing left for a stale notice to do here.
            let recorder_and_path = self.tracked.get(&notice.device_id).and_then(|t| {
                if let ConnectionState::Connected { path, .. } = &t.state {
                    Some((Arc::clone(&t.recorder), path.clone()))
                } else {
                    None
                }
            });
            let Some((recorder, path)) = recorder_and_path else {
                continue;
            };
            if let Err(e) =
                append_disconnect_event(&recorder, &notice.device_id, &path, &notice.reason)
            {
                eprintln!(
                    "serialwrapd: port: failed to append disconnect event for {}: {e}",
                    notice.device_id.0
                );
            }
            if let Some(tracked) = self.tracked.get_mut(&notice.device_id) {
                let old = std::mem::replace(
                    &mut tracked.state,
                    ConnectionState::Disconnected {
                        since: Instant::now(),
                        last_path: path,
                    },
                );
                // The reader that sent this notice is already on its way
                // out (it sends the notice immediately before returning),
                // so this join is expected to complete essentially
                // immediately — this call exists for the invariant, not
                // because a long wait is expected here.
                stop_and_join_reader(old);
            }
            self.clear_live_fd(&notice.device_id);
        }
    }

    /// Clear a device's live fd from the shared config map (see
    /// [`LiveDeviceConfig`]) — called from every place a device transitions
    /// away from `Connected`, so [`PortConfigApi`] never operates on a
    /// stale fd for a device that's actually disconnected. The persisted
    /// profile itself is untouched; only the live-fd handle goes away.
    fn clear_live_fd(&self, id: &DeviceId) {
        if let Some(entry) = self
            .configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            entry.fd = None;
        }
    }

    /// Backup disconnect signal: anything still marked `Connected` but
    /// absent from the latest enumeration snapshot (see the module docs),
    /// plus resetting any `OpenFailed` device's debounce memory on the
    /// same condition — a device that could never open also disappearing
    /// is worth a fresh report if/when it reappears, rather than a stale
    /// `last_reason` suppressing it forever.
    fn reconcile_missing(&mut self, seen: &HashSet<DeviceId>) {
        let missing_ids: Vec<DeviceId> = self
            .tracked
            .iter()
            .filter(|(id, t)| {
                !seen.contains(*id)
                    && !self.leases.contains_key(*id)
                    && matches!(
                        t.state,
                        ConnectionState::Connected { .. } | ConnectionState::OpenFailed { .. }
                    )
            })
            .map(|(id, _)| id.clone())
            .collect();

        // Collected rather than cleared inline: `clear_live_fd` takes
        // `&self` (it only touches `self.configs`), which would otherwise
        // conflict with the `&mut self.tracked` borrow `tracked` holds for
        // the rest of this loop body.
        let mut newly_disconnected = Vec::new();

        for id in missing_ids {
            let Some(tracked) = self.tracked.get_mut(&id) else {
                continue;
            };
            match &tracked.state {
                ConnectionState::Connected { path, .. } => {
                    let path = path.clone();
                    let recorder = Arc::clone(&tracked.recorder);
                    let old = std::mem::replace(
                        &mut tracked.state,
                        ConnectionState::Disconnected {
                            since: Instant::now(),
                            last_path: path.clone(),
                        },
                    );
                    if let Err(e) = append_disconnect_event(
                        &recorder,
                        &id,
                        &path,
                        "device no longer enumerated",
                    ) {
                        eprintln!(
                            "serialwrapd: port: failed to append disconnect event for {}: {e}",
                            id.0
                        );
                    }
                    stop_and_join_reader(old);
                    newly_disconnected.push(id);
                }
                ConnectionState::OpenFailed { .. } => {
                    // No event: nothing was ever actually connected to
                    // report a disconnect for. `last_path` deliberately
                    // empty so a later reappearance — even at the exact
                    // same path — is never mistaken for a same-path
                    // cooldown-throttled retry (see `RECONNECT_COOLDOWN`);
                    // an OpenFailed device reappearing should always be
                    // retried promptly.
                    tracked.state = ConnectionState::Disconnected {
                        since: Instant::now(),
                        last_path: PathBuf::new(),
                    };
                }
                _ => {}
            }
        }

        for id in &newly_disconnected {
            self.clear_live_fd(id);
        }
    }

    // ---- Lease mode (`TASKS.md` T2.2, issue #9) ------------------------

    /// Drain every [`LeaseCommand`] queued by [`PortConfigApi::acquire_lease`]/
    /// [`PortConfigApi::release_lease`] since the last call, processing each
    /// synchronously (same drain-at-top-of-`poll_once` shape as
    /// [`Self::drain_disconnect_notices`]) and replying on its channel.
    fn drain_lease_commands(&mut self) {
        while let Ok(cmd) = self.lease_rx.try_recv() {
            match cmd {
                LeaseCommand::Acquire {
                    id,
                    command,
                    pid,
                    timeout_s,
                    reply,
                } => {
                    let result = self.handle_acquire_lease(&id, command, pid, timeout_s);
                    let _ = reply.send(result);
                }
                LeaseCommand::Release {
                    token,
                    exit_code,
                    reply,
                } => {
                    let result = self.handle_release_lease(&token, exit_code);
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn handle_acquire_lease(
        &mut self,
        id: &DeviceId,
        command: String,
        pid: u32,
        timeout_s: Option<f64>,
    ) -> Result<LeaseAcquired, LeaseError> {
        if let Some(existing) = self.leases.get(id) {
            return Err(LeaseError::AlreadyLeased {
                holder: format!("pid {} running `{}`", existing.pid, existing.command),
            });
        }
        let path = self.take_fd_for_lease(id).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => LeaseError::UnknownDevice,
            io::ErrorKind::NotConnected => LeaseError::NotConnected,
            _ => LeaseError::Io(e.to_string()),
        })?;

        let token = generate_lease_token();
        let started = Instant::now();
        let deadline = timeout_s
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| started + Duration::from_secs_f64(s));
        self.leases.insert(
            id.clone(),
            ActiveLease {
                token: token.clone(),
                command: command.clone(),
                pid,
                started,
                deadline,
            },
        );

        if let Err(e) = self.persist_lease(id, &token, &command, pid, timeout_s) {
            eprintln!(
                "serialwrapd: port: failed to persist lease state for {} (a daemon crash mid-\
                 lease will not be auto-reclaimed on restart): {e}",
                id.0
            );
        }
        if let Some(recorder) = self.tracked.get(id).map(|t| Arc::clone(&t.recorder)) {
            if let Err(e) =
                append_lease_start_event(&recorder, id, &path, &command, pid, &token, timeout_s)
            {
                eprintln!(
                    "serialwrapd: port: failed to append lease_start event for {}: {e}",
                    id.0
                );
            }
        }
        Ok(LeaseAcquired { token, path })
    }

    fn handle_release_lease(
        &mut self,
        token: &str,
        exit_code: i32,
    ) -> Result<LeaseReleased, LeaseError> {
        let Some(id) = self
            .leases
            .iter()
            .find(|(_, l)| l.token == token)
            .map(|(id, _)| id.clone())
        else {
            return Err(LeaseError::UnknownToken);
        };
        let lease = self.leases.remove(&id).expect("just located above");
        let duration_ms = self.finish_lease(&id, &lease, Some(exit_code), "released");
        Ok(LeaseReleased { duration_ms })
    }

    /// Check every currently-active lease's deadline (if any) and reclaim
    /// any that has passed — the daemon's own safety net for
    /// `--lease-timeout`, independent of whether the client that acquired
    /// the lease is still around to call `release_lease` itself (see
    /// `TASKS.md` T2.2's "`--lease-timeout` 逾時...daemon 收回" acceptance
    /// criterion). Runs every [`Self::poll_once`] tick, so the worst-case
    /// delay past the deadline is one `poll_interval` — comfortably inside
    /// the 1-second bound the issue's acceptance criteria set.
    fn reclaim_expired_leases(&mut self) {
        let now = Instant::now();
        let expired: Vec<DeviceId> = self
            .leases
            .iter()
            .filter(|(_, l)| l.deadline.is_some_and(|d| now >= d))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(lease) = self.leases.remove(&id) {
                self.finish_lease(&id, &lease, None, "timeout");
            }
        }
    }

    /// Shared tail end of both a client-requested release and a daemon-
    /// initiated timeout reclaim: reopen the device, append `lease_end`,
    /// and drop the persisted lease file. Returns the lease's held
    /// duration in milliseconds. Best-effort on the reopen — matching this
    /// module's general "config/connect problems don't fail the whole
    /// operation" stance (see `port_io`'s module docs): if the path is
    /// somehow still busy (e.g. an external tool that never actually let
    /// go), the device is left `Disconnected` and the *normal* poll loop's
    /// existing retry/cooldown machinery (no longer held back by `leases`,
    /// since the entry was already removed by the caller) picks it back up
    /// on a later tick.
    fn finish_lease(
        &mut self,
        id: &DeviceId,
        lease: &ActiveLease,
        exit_code: Option<i32>,
        reason: &str,
    ) -> u64 {
        let duration_ms = lease.started.elapsed().as_millis() as u64;
        if let Err(e) = self.reopen_after_lease(id) {
            eprintln!(
                "serialwrapd: port: failed to reopen {} after its lease ended (will retry via \
                 the normal reconnect path): {e}",
                id.0
            );
        }
        let recorder_and_path = {
            let path = self
                .configs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .and_then(|c| c.path.clone());
            self.tracked
                .get(id)
                .map(|t| (Arc::clone(&t.recorder), path))
        };
        if let Some((recorder, path)) = recorder_and_path {
            if let Err(e) = append_lease_end_event(
                &recorder,
                id,
                path.as_deref(),
                &lease.command,
                lease.pid,
                &lease.token,
                exit_code,
                duration_ms,
                reason,
            ) {
                eprintln!(
                    "serialwrapd: port: failed to append lease_end event for {}: {e}",
                    id.0
                );
            }
        }
        self.remove_persisted_lease(id);
        duration_ms
    }

    /// Stop and join `id`'s reader thread and clear its live fd (see
    /// [`stop_and_join_reader`]/[`Self::clear_live_fd`]) without touching
    /// `tracked`'s recorder or appending any connect/disconnect event —
    /// this is a controlled handoff, not a disconnect. Returns the path
    /// the device was open at, which the caller hands off to whatever
    /// external process the lease is for.
    fn take_fd_for_lease(&mut self, id: &DeviceId) -> io::Result<PathBuf> {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown device {}", id.0),
            ));
        };
        let path = match &tracked.state {
            ConnectionState::Connected { path, .. } => path.clone(),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("{} is not currently connected", id.0),
                ));
            }
        };
        let old = std::mem::replace(
            &mut tracked.state,
            ConnectionState::Disconnected {
                since: Instant::now(),
                last_path: path.clone(),
            },
        );
        stop_and_join_reader(old);
        self.clear_live_fd(id);
        Ok(path)
    }

    /// Reverse of [`Self::take_fd_for_lease`]: reopen the device at its
    /// last-known path, re-arm a fresh reader thread, and restore
    /// `Connected` state — everything [`Self::attempt_open`]'s success path
    /// does, *except* appending a `connect`/`config_change` event, since
    /// (per the lease-vs-disconnect distinction the wiki draws) this is a
    /// resumption, not a fresh connection.
    fn reopen_after_lease(&mut self, id: &DeviceId) -> io::Result<()> {
        let (path, config) = {
            let configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
            let entry = configs.get(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            let path = entry.path.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} has no known path", id.0),
                )
            })?;
            (path, entry.profile.config.clone())
        };

        let (file, config_err) = port_io::open_and_configure(&path, &config)?;
        if let Some(e) = config_err {
            eprintln!(
                "serialwrapd: port: config application for {} did not fully apply after lease \
                 release (device is still connected and recording): {e}",
                id.0
            );
        }

        let Some(recorder) = self.tracked.get(id).map(|t| Arc::clone(&t.recorder)) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown device {}", id.0),
            ));
        };

        let file = Arc::new(file);
        if let Some(entry) = self
            .configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            entry.fd = Some(Arc::clone(&file));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_tx = self.disconnect_tx.clone();
        let reader_id = id.clone();
        let reader_file = Arc::clone(&file);
        let reader = thread::spawn(move || {
            run_reader(reader_file, reader_id, recorder, reader_tx, reader_stop);
        });

        if let Some(tracked) = self.tracked.get_mut(id) {
            tracked.state = ConnectionState::Connected { path, stop, reader };
        }
        Ok(())
    }

    fn lease_file_path(&self, id: &DeviceId) -> PathBuf {
        self.data_dir.join("devices").join(&id.0).join("lease.json")
    }

    /// Persist a just-acquired lease to `<data_dir>/devices/<id>/lease.json`
    /// (atomic write-then-rename, same pattern `device_profile::ProfileStore::save`
    /// uses) — the durable record [`Self::recover_residual_lease`] reads
    /// back after a crash/restart. Best-effort: a failure here is logged by
    /// the caller, never fatal to the lease itself (the in-memory
    /// `ActiveLease` is still fully functional for this process's
    /// lifetime; only crash-recovery is degraded).
    fn persist_lease(
        &self,
        id: &DeviceId,
        token: &str,
        command: &str,
        pid: u32,
        timeout_s: Option<f64>,
    ) -> io::Result<()> {
        let path = self.lease_file_path(id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let started_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let persisted = PersistedLease {
            token: token.to_string(),
            command: command.to_string(),
            pid,
            started_epoch_ms,
            timeout_s,
        };
        let bytes = serde_json::to_vec(&persisted)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &bytes)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    fn remove_persisted_lease(&self, id: &DeviceId) {
        let path = self.lease_file_path(id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "serialwrapd: port: failed to remove lease file for {}: {e}",
                id.0
            ),
        }
    }

    /// Check for a lease file left behind by a previous daemon process
    /// against this same `data_dir` (see [`PersistedLease`]'s docs) and, if
    /// found, close it out: append a `lease_end` event with `reason:
    /// "daemon_restart"` (an honest record that the previous process died
    /// mid-lease, not a silent gap) and remove the file. Called from
    /// [`Self::handle_new_device`] — the first (and only) moment a fresh
    /// `HotplugDetector` instance sees this device_id, which is exactly
    /// when "was this leased by whoever ran before me" needs answering.
    /// Never touches `self.leases`/`self.tracked` — normal connect
    /// processing (already in progress in the caller) proceeds exactly as
    /// it would for a device with no residual lease at all.
    fn recover_residual_lease(&self, id: &DeviceId, recorder: &Recorder) {
        let path = self.lease_file_path(id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!(
                    "serialwrapd: port: failed to read residual lease file for {}: {e}",
                    id.0
                );
                return;
            }
        };
        let persisted: PersistedLease = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "serialwrapd: port: residual lease file for {} is corrupt, discarding: {e}",
                    id.0
                );
                let _ = fs::remove_file(&path);
                return;
            }
        };
        let now_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let duration_ms = now_epoch_ms.saturating_sub(persisted.started_epoch_ms);
        if let Err(e) = append_lease_end_event(
            recorder,
            id,
            None,
            &persisted.command,
            persisted.pid,
            &persisted.token,
            None,
            duration_ms,
            "daemon_restart",
        ) {
            eprintln!(
                "serialwrapd: port: failed to append recovery lease_end event for {}: {e}",
                id.0
            );
        }
        let _ = fs::remove_file(&path);
        eprintln!(
            "serialwrapd: port: reclaimed a residual lease for {} left by a previous daemon \
             instance (token {})",
            id.0, persisted.token
        );
    }
}

/// Read loop for one connected device: copies whatever bytes arrive
/// straight into the recorder as `rx` records (no line assembly — see
/// the module docs).
///
/// Takes `Arc<File>` (shared with `LiveDeviceConfig::fd`, see that type's
/// docs) rather than owning the `File` outright: `PortConfigApi` needs to
/// issue termios/`TIOCM*` ioctls against the exact same fd this thread
/// reads from, and POSIX termios/control-line state belongs to the
/// underlying tty, not to any one fd/thread — sharing the same `Arc<File>`
/// (rather than a second independent `open()` of the same path) is what
/// guarantees both sides are always looking at the same open file
/// description with no risk of racing a second exclusive-mode open.
/// Reading through `&File` (via `impl Read for &File`) rather than
/// `&mut File` is what makes this safe to share: nothing here ever needs
/// exclusive access to `file` itself, only to `buf`.
///
/// Polls with a short timeout ([`READER_POLL_TIMEOUT_MS`]) rather than
/// calling a plain blocking `read()`, so `stop` (set by
/// [`stop_and_join_reader`]) is checked regularly even when no data ever
/// arrives — this is what lets a superseded or backup-disconnected
/// connection be told to stop and actually exit promptly, instead of
/// staying blocked in `read()` indefinitely and potentially overlapping
/// with a second reader opened for the same device (which would split
/// incoming bytes unpredictably between the two).
///
/// Exits either because `stop` was set (a controlled handoff — no
/// [`DisconnectNotice`] sent, since the device isn't actually gone) or
/// because `read` returned EOF/an error (a real disconnect — reported via
/// `tx`).
fn run_reader(
    file: Arc<File>,
    device_id: DeviceId,
    recorder: Arc<Recorder>,
    tx: mpsc::Sender<DisconnectNotice>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 4096];
    // Logged at most once per connection (not once per read) so a
    // sustained failure (e.g. a full disk) produces one diagnostic line
    // instead of one per incoming chunk at line rate.
    let mut logged_append_failure = false;
    let mut file_ref: &File = file.as_ref();

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        match poll(
            &mut [PollFd::new(file.as_fd(), PollFlags::POLLIN)],
            PollTimeout::from(READER_POLL_TIMEOUT_MS),
        ) {
            Ok(0) => continue, // timed out, no data ready — re-check `stop`
            Ok(_) => {} // ready: data, hangup, or an error condition — `read` below is authoritative either way
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                let _ = tx.send(DisconnectNotice {
                    device_id,
                    reason: format!("poll error: {e}"),
                });
                return;
            }
        }

        match file_ref.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(DisconnectNotice {
                    device_id,
                    reason: "eof".to_string(),
                });
                return;
            }
            Ok(n) => {
                if let Err(e) = recorder.append_rx(&buf[..n]) {
                    if !logged_append_failure {
                        eprintln!(
                            "serialwrapd: port: append_rx failed for {device_id:?} (further \
                             failures on this connection are suppressed): {e}"
                        );
                        logged_append_failure = true;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = tx.send(DisconnectNotice {
                    device_id,
                    reason: format!("read error: {e}"),
                });
                return;
            }
        }
    }
}

/// Handle to a [`HotplugDetector`] running on its own background thread
/// (via [`HotplugDetector::spawn`]).
pub struct DetectorHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    recorders: SharedRecorders,
    configs: SharedDeviceConfigs,
    profiles: Arc<ProfileStore>,
    lease_tx: mpsc::Sender<LeaseCommand>,
}

impl DetectorHandle {
    /// Shared, thread-safe view of every device's [`Recorder`] the running
    /// detector has opened. See [`SharedRecorders`].
    pub fn recorders(&self) -> SharedRecorders {
        Arc::clone(&self.recorders)
    }

    /// See [`HotplugDetector::port_config_api`].
    pub fn port_config_api(&self) -> PortConfigApi {
        PortConfigApi {
            configs: Arc::clone(&self.configs),
            recorders: Arc::clone(&self.recorders),
            profiles: Arc::clone(&self.profiles),
            lease_tx: self.lease_tx.clone(),
        }
    }

    /// Signal the poll loop to stop and wait for it to actually exit.
    /// Per-device reader threads are not force-joined here (they exit on
    /// their own once their underlying fd errors/EOFs, e.g. on process
    /// exit or test teardown closing the transport).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for DetectorHandle {
    fn drop(&mut self) {
        // Best-effort safety net: signal the loop to stop even if the
        // caller drops the handle without calling `stop()` explicitly.
        // Doesn't join (avoids blocking an unrelated unwind/drop).
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The seam through which anything outside the poll loop — currently only
/// this crate's own tests, but the intended caller is T1.4's future UDS
/// client-handler threads — reads and changes a device's shared port
/// configuration. Cheap to clone (every field is an `Arc`); every clone
/// operates on the exact same underlying state as the [`HotplugDetector`]
/// (or [`DetectorHandle`]) it came from.
///
/// This is where this task's "config is shared state" requirement actually
/// lives: there is one [`PortConfig`] per [`DeviceId`], not one per caller,
/// so a change made through any clone of this API is immediately visible
/// (and, if the device is connected, immediately re-applied to the one fd)
/// to every other holder — including the poll thread's own next reconnect.
#[derive(Clone)]
pub struct PortConfigApi {
    configs: SharedDeviceConfigs,
    recorders: SharedRecorders,
    profiles: Arc<ProfileStore>,
    /// Submits lease acquire/release requests to the poll thread — see
    /// [`LeaseCommand`].
    lease_tx: mpsc::Sender<LeaseCommand>,
}

/// One device's summary for `list_devices` (`TASKS.md` T1.4): identity,
/// last-known path, whether it's currently connected, and its current
/// configuration. See [`PortConfigApi::list_devices`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceSummary {
    pub id: DeviceId,
    /// `None` only if the device has never been tracked long enough to
    /// reach `handle_new_device`'s config insert — should not happen in
    /// practice, since both are part of the same reconciliation step.
    pub path: Option<PathBuf>,
    pub connected: bool,
    pub config: PortConfig,
}

impl PortConfigApi {
    /// Every device the detector has ever seen, with its last-known path,
    /// live connection state, and current configuration — the data
    /// `list_devices` (T1.4) needs. `connected` is derived from whether a
    /// live fd is currently held, the same signal [`Self::live_fd`] and
    /// [`Self::error_counts`] already key off of.
    pub fn list_devices(&self) -> Vec<DeviceSummary> {
        self.configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, entry)| DeviceSummary {
                id: id.clone(),
                path: entry.path.clone(),
                connected: entry.fd.is_some(),
                config: entry.profile.config.clone(),
            })
            .collect()
    }

    /// Read `id`'s current configuration. Unlike [`Self::error_counts`],
    /// this works whether or not the device is currently connected — config
    /// is persisted, shared state (see the wiki: "previously recorded data
    /// is not reinterpreted"), not a property of the live fd. Errors with
    /// [`io::ErrorKind::NotFound`] if `id` has never been seen.
    pub fn get_config(&self, id: &DeviceId) -> io::Result<PortConfig> {
        self.configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|entry| entry.profile.config.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })
    }

    /// Change `id`'s port configuration (baud/data bits/parity/stop
    /// bits/flow control/open-time DTR-RTS policy): persist it (so a
    /// future reconnect — or daemon restart — re-applies it, see
    /// `port.rs`'s `attempt_open`), re-apply it live if the device is
    /// currently connected, and append a `config_change` event carrying
    /// the full old/new values and `changed_by`.
    ///
    /// Errors with [`io::ErrorKind::NotFound`] if `id` has never been seen
    /// by the detector at all. A live re-application failure (e.g. a real
    /// ioctl error) is logged, not returned — matching `attempt_open`'s
    /// own "config problems don't fail the operation" stance (see
    /// `port_io`'s module docs) — because the new config *has* been
    /// durably persisted and will be attempted again on the next
    /// reconnect regardless.
    pub fn set_port_config(
        &self,
        id: &DeviceId,
        new_config: PortConfig,
        changed_by: &str,
    ) -> io::Result<()> {
        let (old_config, fd) = {
            let mut configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
            let entry = configs.get_mut(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            let old_config = entry.profile.config.clone();
            entry.profile.config = new_config.clone();
            self.profiles.save(&id.0, &entry.profile)?;
            (old_config, entry.fd.clone())
        };

        if let Some(fd) = fd {
            if let Err(e) = port_io::apply_termios(fd.as_raw_fd(), &new_config) {
                eprintln!(
                    "serialwrapd: port: failed to live-apply new config for {} (persisted; will \
                     be retried on next reconnect): {e}",
                    id.0
                );
            }
        }

        if let Some(recorder) = self
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            device_profile::append_config_change_event(
                recorder,
                Some(&old_config),
                &new_config,
                changed_by,
            )?;
        }
        Ok(())
    }

    /// Manually assert/deassert DTR. Errors with
    /// [`io::ErrorKind::NotConnected`] if the device isn't currently
    /// connected (there is no live fd to touch). Appends a
    /// `control_line_change` event — distinct from `config_change` and
    /// from `dtr_pulse` (see `device_profile.rs`'s event-naming docs).
    pub fn set_dtr(&self, id: &DeviceId, level: bool, changed_by: &str) -> io::Result<()> {
        self.set_control_line(id, ControlLine::Dtr, level, changed_by)
    }

    /// Manually assert/deassert RTS. See [`Self::set_dtr`].
    pub fn set_rts(&self, id: &DeviceId, level: bool, changed_by: &str) -> io::Result<()> {
        self.set_control_line(id, ControlLine::Rts, level, changed_by)
    }

    fn set_control_line(
        &self,
        id: &DeviceId,
        line: ControlLine,
        level: bool,
        changed_by: &str,
    ) -> io::Result<()> {
        let fd = self.live_fd(id)?;
        port_io::set_control_line(fd.as_raw_fd(), line, level)?;
        if let Some(recorder) = self
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            device_profile::append_control_line_change_event(
                recorder,
                line.as_str(),
                level,
                changed_by,
            )?;
        }
        Ok(())
    }

    /// Pulse DTR (deassert, hold, reassert) — the independently-named
    /// reset-shaped operation this task's issue specifically calls for
    /// (see `port_io::dtr_pulse`'s and `device_profile.rs`'s docs). Errors
    /// with [`io::ErrorKind::NotConnected`] if the device isn't currently
    /// connected.
    pub fn dtr_pulse(&self, id: &DeviceId, duration: Duration, changed_by: &str) -> io::Result<()> {
        let fd = self.live_fd(id)?;
        port_io::dtr_pulse(fd.as_raw_fd(), duration)?;
        if let Some(recorder) = self
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            device_profile::append_dtr_pulse_event(
                recorder,
                duration.as_millis() as u64,
                changed_by,
            )?;
        }
        Ok(())
    }

    /// Read `id`'s framing/overrun/parity error counters for the host
    /// platform — [`ErrorCounts::Unavailable`] on macOS, never a
    /// misleading `0` (see `error_counts.rs`'s module docs). Errors with
    /// [`io::ErrorKind::NotConnected`] if the device isn't currently
    /// connected.
    pub fn error_counts(&self, id: &DeviceId) -> io::Result<ErrorCounts> {
        let fd = self.live_fd(id)?;
        error_counts::read_error_counts(current_platform(), fd.as_raw_fd())
    }

    /// Write `data` to `id`'s physical port through the exact same shared
    /// fd `set_dtr`/`dtr_pulse`/the reader thread already operate on (see
    /// [`Self::live_fd`]) — never a second, independently opened fd. This
    /// is what makes lease mode actually work: a lease's entire point is
    /// that [`PortConfigApi::acquire_lease`] closes *every* fd this daemon
    /// holds for the device, and it can only keep that promise if there is
    /// exactly one fd to close in the first place (`TASKS.md` T2.2, issue
    /// #9 — see `protocol::backend::LiveBackend::write_bytes`'s docs for
    /// the T2.1-era second-fd workaround this replaces). Errors with
    /// [`io::ErrorKind::NotConnected`] whenever there's no live fd to write
    /// through — including, deliberately, for the entire duration of an
    /// active lease.
    pub fn write_bytes(&self, id: &DeviceId, data: &[u8]) -> io::Result<()> {
        let fd = self.live_fd(id)?;
        (&*fd).write_all(data)
    }

    /// Acquire an exclusive lease on `id`'s port: stop its reader thread,
    /// close every fd this daemon holds for it, and mark it so the poll
    /// loop leaves it alone until [`Self::release_lease`] (or the daemon's
    /// own `timeout_s` deadline) ends the lease. Returns the device's
    /// current path (for the caller to hand off to an external tool) and
    /// an opaque `token` to release with later. See [`LeaseCommand`] for
    /// why this round-trips through the poll thread rather than acting
    /// directly.
    pub fn acquire_lease(
        &self,
        id: &DeviceId,
        command: &str,
        pid: u32,
        timeout_s: Option<f64>,
    ) -> Result<LeaseAcquired, LeaseError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.lease_tx
            .send(LeaseCommand::Acquire {
                id: id.clone(),
                command: command.to_string(),
                pid,
                timeout_s,
                reply: reply_tx,
            })
            .map_err(|_| LeaseError::Io("the poll thread is not running".to_string()))?;
        reply_rx
            .recv()
            .map_err(|_| LeaseError::Io("the poll thread dropped the reply channel".to_string()))?
    }

    /// End a lease previously granted by [`Self::acquire_lease`], identified
    /// by its opaque `token`. Reopens the device and resumes recording.
    pub fn release_lease(&self, token: &str, exit_code: i32) -> Result<LeaseReleased, LeaseError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.lease_tx
            .send(LeaseCommand::Release {
                token: token.to_string(),
                exit_code,
                reply: reply_tx,
            })
            .map_err(|_| LeaseError::Io("the poll thread is not running".to_string()))?;
        reply_rx
            .recv()
            .map_err(|_| LeaseError::Io("the poll thread dropped the reply channel".to_string()))?
    }

    fn live_fd(&self, id: &DeviceId) -> io::Result<Arc<File>> {
        let configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
        let entry = configs.get(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
        })?;
        entry.fd.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("device {} is not connected", id.0),
            )
        })
    }
}

/// Test-only device enumeration support.
///
/// Not `#[cfg(test)]`-gated because it must also be usable from this
/// crate's `tests/*.rs` integration tests, which compile against
/// `serialwrapd` normally — `cfg(test)` inside the library crate itself is
/// invisible there. Same reasoning as the `mock-device` crate being a
/// plain, always-compiled crate wired in only via `[dev-dependencies]`
/// (see its own crate docs).
pub mod testing {
    use super::{DeviceEnumerator, EnumeratedDevice};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// A scripted, in-memory stand-in for `serialport::available_ports()`.
    /// Real USB hotplug cannot be reproduced in CI, so tests drive
    /// appearance/disappearance/path-drift scenarios by mutating this
    /// directly instead.
    ///
    /// Cloning shares the same underlying device list (an `Arc` inside) —
    /// clone one handle to hand to a [`super::HotplugDetector`] as its
    /// [`DeviceEnumerator`] and keep another as the test's "control panel".
    #[derive(Clone, Default)]
    pub struct ScriptedEnumerator {
        devices: Arc<Mutex<Vec<EnumeratedDevice>>>,
    }

    impl ScriptedEnumerator {
        pub fn new() -> Self {
            Self::default()
        }

        /// Replace the entire scripted device list.
        pub fn set_devices(&self, devices: Vec<EnumeratedDevice>) {
            *self.devices.lock().unwrap_or_else(|e| e.into_inner()) = devices;
        }

        /// Add one device to the scripted list (simulates an appearance).
        pub fn push(&self, device: EnumeratedDevice) {
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(device);
        }

        /// Remove whichever entry currently has this path (simulates a
        /// disappearance from enumeration).
        pub fn remove_path(&self, path: &Path) {
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|d| d.path != path);
        }

        /// Replace the path on whichever entry currently has `old_path` —
        /// the direct way to script a `ttyUSB0 -> ttyUSB1`-style path
        /// drift for the same (unchanged) USB metadata. No-op if nothing
        /// currently has `old_path`.
        pub fn replace_path(&self, old_path: &Path, new_path: PathBuf) {
            let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            for d in guard.iter_mut() {
                if d.path == old_path {
                    d.path = new_path;
                    return;
                }
            }
        }
    }

    impl DeviceEnumerator for ScriptedEnumerator {
        fn enumerate(&mut self) -> io::Result<Vec<EnumeratedDevice>> {
            Ok(self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::ScriptedEnumerator;
    use super::*;
    use std::time::Instant;
    use wrap_proto::Record;

    fn usb(vid: u16, pid: u16, serial: Option<&str>) -> UsbMetadata {
        UsbMetadata {
            vid,
            pid,
            serial_number: serial.map(str::to_string),
        }
    }

    // ---- DeviceId / sanitization -----------------------------------

    #[test]
    fn usb_id_matches_the_wiki_storage_layout_example() {
        let id = DeviceId::from_usb(&usb(0x1a86, 0x7523, Some("A5069RR4"))).unwrap();
        assert_eq!(id.0, "usb-1a86_7523_A5069RR4");
        assert!(!id.is_path_based());
    }

    #[test]
    fn usb_id_is_none_without_a_serial_number() {
        assert!(DeviceId::from_usb(&usb(0x1a86, 0x7523, None)).is_none());
        assert!(DeviceId::from_usb(&usb(0x1a86, 0x7523, Some("   "))).is_none());
    }

    #[test]
    fn for_device_falls_back_to_path_when_no_usb_metadata() {
        let dev = EnumeratedDevice {
            path: PathBuf::from("/dev/ttyAMA0"),
            usb: None,
        };
        let id = DeviceId::for_device(&dev);
        assert!(id.is_path_based());
        assert!(id.0.starts_with("path-"));
        assert!(id.0.contains("ttyAMA0"));
    }

    #[test]
    fn for_device_falls_back_to_path_when_usb_metadata_has_no_serial() {
        let dev = EnumeratedDevice {
            path: PathBuf::from("/dev/ttyUSB3"),
            usb: Some(usb(0x10c4, 0xea60, None)),
        };
        let id = DeviceId::for_device(&dev);
        assert!(
            id.is_path_based(),
            "no serial number means VID:PID alone can't be trusted as unique"
        );
    }

    #[test]
    fn sanitize_component_strips_unsafe_characters_and_is_never_empty() {
        // Already-clean input (the common case) passes through untouched,
        // no hash suffix — this is what keeps the wiki's exact
        // `usb-1a86_7523_A5069RR4` example format unchanged.
        assert_eq!(sanitize_component("A5069RR4"), "A5069RR4");

        // Anything that needed replacement gets a disambiguating hash
        // suffix appended (see the collision test below for why).
        let cleaned = sanitize_component("has space");
        assert!(cleaned.starts_with("has_space-"), "got {cleaned:?}");
        let cleaned = sanitize_component("/dev/ttyUSB0");
        assert!(cleaned.starts_with("dev_ttyUSB0-"), "got {cleaned:?}");

        assert!(sanitize_component("").starts_with("unknown-"));
        assert!(sanitize_component("///").starts_with("unknown-"));
    }

    #[test]
    fn sanitize_component_disambiguates_inputs_that_would_otherwise_collide() {
        // These would all clean to the literal string "AB_CD" under plain
        // character replacement — the hash suffix must keep them distinct.
        let a = sanitize_component("AB CD");
        let b = sanitize_component("AB/CD");
        let c = sanitize_component("AB_CD");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // "AB_CD" itself needed no cleaning, so it's returned bare.
        assert_eq!(c, "AB_CD");

        // Two different inputs that both collapse toward "unknown" (no
        // ASCII-alphanumeric/`-`/`.` characters at all) must not collide.
        let x = sanitize_component("日本語");
        let y = sanitize_component("한국어");
        assert_ne!(x, y);
    }

    // ---- Platform filtering (acceptance criterion 6) ----------------

    #[test]
    fn macos_filter_keeps_cu_and_drops_tty() {
        let devices = vec![
            EnumeratedDevice {
                path: PathBuf::from("/dev/cu.usbserial-1420"),
                usb: Some(usb(0x1a86, 0x7523, Some("X1"))),
            },
            EnumeratedDevice {
                path: PathBuf::from("/dev/tty.usbserial-1420"),
                usb: Some(usb(0x1a86, 0x7523, Some("X1"))),
            },
        ];
        let filtered = filter_platform(devices, Platform::MacOs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, PathBuf::from("/dev/cu.usbserial-1420"));
    }

    #[test]
    fn linux_filter_passes_everything_through() {
        let devices = vec![
            EnumeratedDevice {
                path: PathBuf::from("/dev/ttyUSB0"),
                usb: Some(usb(0x1a86, 0x7523, Some("X1"))),
            },
            EnumeratedDevice {
                path: PathBuf::from("/dev/ttyACM0"),
                usb: Some(usb(0x2341, 0x0043, Some("X2"))),
            },
        ];
        let filtered = filter_platform(devices.clone(), Platform::Linux);
        assert_eq!(filtered, devices);
    }

    // ---- Open-failure messaging (acceptance criterion 5) ------------

    #[test]
    fn linux_permission_error_mentions_dialout_and_udev() {
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        let (reason, message) =
            describe_open_error(&err, Path::new("/dev/ttyUSB0"), Platform::Linux);
        assert_eq!(reason, "permission_denied");
        assert!(message.contains("dialout"), "message was: {message}");
        assert!(message.contains("udev"), "message was: {message}");
    }

    #[test]
    fn macos_permission_error_mentions_driver_install_not_dialout() {
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        let (reason, message) =
            describe_open_error(&err, Path::new("/dev/cu.usbserial-1420"), Platform::MacOs);
        assert_eq!(reason, "permission_denied");
        assert!(message.contains("driver"), "message was: {message}");
        assert!(!message.contains("dialout"));
    }

    #[test]
    fn busy_error_names_exclusive_access() {
        let err = io::Error::from_raw_os_error(libc::EBUSY);
        let (reason, message) =
            describe_open_error(&err, Path::new("/dev/ttyUSB0"), Platform::Linux);
        assert_eq!(reason, "busy");
        assert!(message.contains("already open"), "message was: {message}");
    }

    // ---- Detector state machine, driven synchronously via poll_once --

    /// Repeatedly call `poll_once` (no sleeping beyond a 1ms retry step)
    /// until `check` is satisfied or `timeout` elapses. Used instead of a
    /// fixed poll interval so these tests are deterministic and fast
    /// regardless of the configured `poll_interval`.
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

    fn recorder_event_kinds(recorder: &Recorder) -> Vec<String> {
        recorder
            .read_since(0, usize::MAX)
            .expect("read_since")
            .records
            .into_iter()
            .filter_map(|r| match r {
                Record::Event { event, .. } => Some(event),
                _ => None,
            })
            .collect()
    }

    fn test_recorder_config() -> RecorderConfig {
        // Defaults have a 1s fsync interval, which is irrelevant for
        // correctness here but slows nothing down either way — kept as
        // `default()` since these tests write only a handful of records.
        RecorderConfig::default()
    }

    #[test]
    fn connect_event_carries_device_id_path_and_usb_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let enumerator = ScriptedEnumerator::new();
        // A real openable path (so `attempt_open` succeeds and a `connect`
        // event, not `open_failed`, is what gets appended) with fake USB
        // metadata standing in for the real device.
        let tmpfile = tmp.path().join("fake-device");
        std::fs::write(&tmpfile, b"").unwrap();
        let dev = EnumeratedDevice {
            path: tmpfile,
            usb: Some(usb(0x1a86, 0x7523, Some("A5069RR4"))),
        };
        enumerator.push(dev.clone());

        let mut detector = HotplugDetector::new(
            Box::new(enumerator),
            tmp.path().join("data"),
            HotplugConfig {
                poll_interval: Duration::from_millis(5),
                recorder_config: test_recorder_config(),
            },
        );
        let id = DeviceId::for_device(&dev);

        let found = poll_until(&mut detector, Duration::from_secs(2), |d| {
            d.recorders().lock().unwrap().contains_key(&id)
        });
        assert!(found, "expected a Recorder to be created for the device");

        let recorders = detector.recorders();
        let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let connect = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "connect" => Some(extra.clone()),
                _ => None,
            })
            .expect("expected a connect event");
        assert_eq!(
            connect.get("device_id").and_then(|v| v.as_str()),
            Some(id.0.as_str())
        );
        assert_eq!(connect.get("vid").and_then(|v| v.as_u64()), Some(0x1a86));
        assert_eq!(connect.get("pid").and_then(|v| v.as_u64()), Some(0x7523));
        assert_eq!(
            connect.get("serial_number").and_then(|v| v.as_str()),
            Some("A5069RR4")
        );
        assert_eq!(connect.get("id_kind").and_then(|v| v.as_str()), Some("usb"));
    }

    #[test]
    fn disconnect_event_recorded_with_reason_when_device_goes_away() {
        let tmp = tempfile::tempdir().unwrap();
        let enumerator = ScriptedEnumerator::new();
        let tmpfile = tmp.path().join("fake-device");
        std::fs::write(&tmpfile, b"").unwrap();
        let dev = EnumeratedDevice {
            path: tmpfile,
            usb: Some(usb(0x2341, 0x0043, Some("SN1"))),
        };
        enumerator.push(dev.clone());

        let mut detector = HotplugDetector::new(
            Box::new(enumerator),
            tmp.path().join("data"),
            HotplugConfig {
                poll_interval: Duration::from_millis(5),
                recorder_config: test_recorder_config(),
            },
        );
        let id = DeviceId::for_device(&dev);

        // An empty regular file EOFs immediately on read, so the reader
        // thread reports disconnect on its own without any extra action.
        let disconnected = poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard
                .get(&id)
                .is_some_and(|r| recorder_event_kinds(r).iter().any(|k| k == "disconnect"))
        });
        assert!(disconnected, "expected a disconnect event");

        let recorders = detector.recorders();
        let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let disconnect = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "disconnect" => Some(extra.clone()),
                _ => None,
            })
            .expect("expected a disconnect event");
        assert_eq!(
            disconnect.get("device_id").and_then(|v| v.as_str()),
            Some(id.0.as_str())
        );
        assert!(disconnect.get("reason").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn open_failure_on_a_permission_denied_path_produces_an_event_with_actionable_message() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, chmod 000 does not block open()");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("blocked-device");
        std::fs::write(&blocked, b"").unwrap();
        let mut perms = std::fs::metadata(&blocked).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&blocked, perms).unwrap();

        let enumerator = ScriptedEnumerator::new();
        let dev = EnumeratedDevice {
            path: blocked.clone(),
            usb: Some(usb(0x0403, 0x6001, Some("FTDI1"))),
        };
        enumerator.push(dev.clone());

        let mut detector = HotplugDetector::new(
            Box::new(enumerator),
            tmp.path().join("data"),
            HotplugConfig {
                poll_interval: Duration::from_millis(5),
                recorder_config: test_recorder_config(),
            },
        );
        let id = DeviceId::for_device(&dev);

        let failed = poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard
                .get(&id)
                .is_some_and(|r| recorder_event_kinds(r).iter().any(|k| k == "open_failed"))
        });
        assert!(failed, "expected an open_failed event");

        let recorders = detector.recorders();
        let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let open_failed = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "open_failed" => Some(extra.clone()),
                _ => None,
            })
            .expect("expected an open_failed event");
        assert_eq!(
            open_failed.get("reason").and_then(|v| v.as_str()),
            Some("permission_denied")
        );
        let message = open_failed
            .get("message")
            .and_then(|v| v.as_str())
            .expect("message field");
        if cfg!(target_os = "linux") {
            assert!(message.contains("dialout"), "message was: {message}");
            assert!(message.contains("udev"), "message was: {message}");
        }
    }

    #[test]
    fn open_failure_is_not_reported_again_every_poll_while_it_persists() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, chmod 000 does not block open()");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("blocked-device");
        std::fs::write(&blocked, b"").unwrap();
        let mut perms = std::fs::metadata(&blocked).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&blocked, perms).unwrap();

        let enumerator = ScriptedEnumerator::new();
        let dev = EnumeratedDevice {
            path: blocked,
            usb: Some(usb(0x0403, 0x6001, Some("FTDI2"))),
        };
        enumerator.push(dev.clone());

        let mut detector = HotplugDetector::new(
            Box::new(enumerator),
            tmp.path().join("data"),
            HotplugConfig {
                poll_interval: Duration::from_millis(1),
                recorder_config: test_recorder_config(),
            },
        );
        let id = DeviceId::for_device(&dev);

        for _ in 0..50 {
            let _ = detector.poll_once();
        }

        let recorders = detector.recorders();
        let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
        let count = recorder_event_kinds(&recorder)
            .iter()
            .filter(|k| *k == "open_failed")
            .count();
        assert_eq!(
            count, 1,
            "a persistent identical failure must be reported once, not every poll"
        );
    }
}
