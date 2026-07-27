//! Port I/O: termios, DTR/RTS control lines, non-blocking device access.
//!
//! Not yet implemented — see `TASKS.md` T1.1 (device identity, hotplug) and
//! T1.3 (baud/frame settings, DTR/RTS, error counters).

/// Stable identifier for a serial device.
///
/// Derived from USB VID:PID + serial number where available, falling back
/// to the device path otherwise, so identity survives `ttyUSB0 -> ttyUSB1`
/// renumbering across replugs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);
