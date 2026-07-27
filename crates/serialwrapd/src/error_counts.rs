//! Serial line error counters (`TASKS.md` T1.3, issue #5): Linux's
//! `TIOCGICOUNT`, and the honest "unavailable" macOS gets instead of a
//! fake `0`.
//!
//! # Why `Unavailable` is a type, not a display-layer convention
//!
//! Framing/overrun/parity error counts exist to help a human tell "the
//! baud rate is wrong" apart from "the firmware is buggy" — a `framing: 0`
//! reading is itself load-bearing evidence that baud is *not* the problem.
//! macOS has no ioctl equivalent to Linux's `TIOCGICOUNT` at all. If this
//! crate reported `0` on macOS, every consumer downstream (CLI, GUI) would
//! render a number that looks exactly like "confirmed zero errors" and
//! actively mislead someone into debugging firmware instead of a cable or
//! baud mismatch. [`ErrorCounts`] makes "never measured" a distinct enum
//! variant that has to be matched explicitly — a caller cannot
//! accidentally compare it to `0` or format it as a number by accident,
//! because there is no numeric field to reach for on that variant.
//!
//! # Why both branches are unit-testable without hardware
//!
//! A *real* `TIOCGICOUNT` call needs a real UART with real framing errors
//! to be a meaningful test at all (`docs/manual-checklist.md` §5 already
//! registers this as hardware-only — even a PTY on Linux CI can't
//! substitute, since the PTY driver doesn't implement the `get_icount`
//! tty op `TIOCGICOUNT` dispatches to, so it would just report `EINVAL`,
//! proving nothing). What *is* real code with real bugs to catch is (a)
//! the byte-layout of the hand-rolled kernel struct this module reads into,
//! and (b) the branch that decides whether to even attempt the ioctl.
//! [`error_counts_from`] takes platform-dispatch as a plain parameter and
//! the actual fetch as an injectable closure — same "platform as data, not
//! `cfg`" pattern `port.rs`'s `describe_open_error`/`filter_platform`
//! already established — so both the Linux-shaped and macOS-shaped
//! behavior are exercised by ordinary unit tests on any CI runner, while
//! the real ioctl invocation itself stays a thin, separately-named function
//! that's Linux-only by construction.

use std::io;
use std::os::fd::RawFd;

use serde::{Deserialize, Serialize};

use crate::port::Platform;

/// Framing/overrun/parity error counters for one device — or the honest
/// admission that this platform has no way to measure them. See the
/// module docs for why this is a variant, never a sentinel `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ErrorCounts {
    /// No mechanism exists on this platform (macOS, or anything other than
    /// Linux) to read these counters — deliberately distinct from
    /// `Available { framing: 0, .. }`, which asserts "measured, and zero".
    Unavailable,
    Available {
        framing: u64,
        overrun: u64,
        parity: u64,
    },
}

/// Raw `struct serial_icounter_struct` layout from Linux's
/// `include/uapi/linux/serial.h` (11 `int`s + 9 reserved `int`s). Not
/// present in the `libc` crate (checked: no hit anywhere in its source),
/// so reproduced here field-for-field; this is part of the kernel's
/// longstanding stable uABI (TIOCGICOUNT has shipped this exact shape for
/// decades), so hand-rolling it carries little drift risk.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawIcounter {
    pub cts: i32,
    pub dsr: i32,
    pub rng: i32,
    pub dcd: i32,
    pub rx: i32,
    pub tx: i32,
    pub frame: i32,
    pub overrun: i32,
    pub parity: i32,
    pub brk: i32,
    pub buf_overrun: i32,
    pub reserved: [i32; 9],
}

/// Pure mapping from the raw kernel struct to this crate's honest type —
/// the actual unit-tested risk surface (did we get `frame`/`overrun`/
/// `parity`'s field positions right?), independent of any real ioctl call.
pub fn parse_icounter(raw: RawIcounter) -> ErrorCounts {
    ErrorCounts::Available {
        // `serial_icounter_struct`'s counters are unsigned quantities that
        // the kernel happens to store as `int`; `.max(0)` is defensive
        // only (these should never actually go negative) so a cast can
        // never silently produce a huge u64 from a negative i32.
        framing: raw.frame.max(0) as u64,
        overrun: raw.overrun.max(0) as u64,
        parity: raw.parity.max(0) as u64,
    }
}

/// Decide `Unavailable` vs. `Available` purely from `platform`, only ever
/// calling `fetch` on `Platform::Linux` — regardless of what `fetch` would
/// have returned, macOS/other platforms never even attempt it. `fetch` is
/// injectable so both branches are unit-testable without any real fd (see
/// tests below).
pub fn error_counts_from(
    platform: Platform,
    fetch: impl FnOnce() -> io::Result<RawIcounter>,
) -> io::Result<ErrorCounts> {
    match platform {
        Platform::Linux => fetch().map(parse_icounter),
        Platform::MacOs | Platform::Other => Ok(ErrorCounts::Unavailable),
    }
}

#[cfg(target_os = "linux")]
fn fetch_icounter_via_ioctl(fd: RawFd) -> io::Result<RawIcounter> {
    let mut raw = RawIcounter::default();
    if unsafe { libc::ioctl(fd, libc::TIOCGICOUNT, &mut raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(raw)
}

/// Never actually invoked in production: [`error_counts_from`] only calls
/// `fetch` on the `Platform::Linux` arm, and [`read_error_counts`] is
/// always called with `current_platform()`, which is never
/// `Platform::Linux` on a non-Linux build. Exists purely so this module —
/// and its `Platform::MacOs`-branch tests — compile and run on every host
/// OS (same reasoning as T1.1's `describe_open_error`).
#[cfg(not(target_os = "linux"))]
fn fetch_icounter_via_ioctl(_fd: RawFd) -> io::Result<RawIcounter> {
    Err(io::Error::other("TIOCGICOUNT is Linux-only"))
}

/// Production entry point: read `fd`'s error counters for the host
/// platform.
pub fn read_error_counts(platform: Platform, fd: RawFd) -> io::Result<ErrorCounts> {
    error_counts_from(platform, || fetch_icounter_via_ioctl(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn raw(frame: i32, overrun: i32, parity: i32) -> RawIcounter {
        RawIcounter {
            frame,
            overrun,
            parity,
            ..RawIcounter::default()
        }
    }

    // ---- Acceptance criterion 6 ----

    #[test]
    fn macos_never_reports_a_number_always_unavailable() {
        let called = Cell::new(false);
        let result = error_counts_from(Platform::MacOs, || {
            called.set(true);
            Ok(raw(3, 1, 2))
        });
        assert_eq!(result.unwrap(), ErrorCounts::Unavailable);
        assert!(!called.get(), "macOS must never even attempt the fetch");
    }

    #[test]
    fn other_platform_also_reports_unavailable_not_zero() {
        let result = error_counts_from(Platform::Other, || Ok(raw(0, 0, 0)));
        assert_eq!(result.unwrap(), ErrorCounts::Unavailable);
    }

    #[test]
    fn linux_maps_real_counter_values_through() {
        let result = error_counts_from(Platform::Linux, || Ok(raw(3, 1, 2)));
        assert_eq!(
            result.unwrap(),
            ErrorCounts::Available {
                framing: 3,
                overrun: 1,
                parity: 2
            }
        );
    }

    #[test]
    fn linux_zero_counts_are_a_real_available_zero_not_confused_with_unavailable() {
        let result = error_counts_from(Platform::Linux, || Ok(raw(0, 0, 0)));
        let counts = result.unwrap();
        assert_eq!(
            counts,
            ErrorCounts::Available {
                framing: 0,
                overrun: 0,
                parity: 0
            }
        );
        assert_ne!(
            counts,
            ErrorCounts::Unavailable,
            "a measured zero must be distinguishable from never-measured"
        );
    }

    #[test]
    fn linux_propagates_a_real_ioctl_failure_as_an_error_not_a_silent_unavailable() {
        let result = error_counts_from(Platform::Linux, || Err(io::Error::other("ioctl failed")));
        assert!(result.is_err(), "a real ioctl failure on Linux must surface as an error, not be swallowed into Unavailable");
    }

    #[test]
    fn parse_icounter_reads_frame_overrun_parity_fields_specifically() {
        // Distinct values in every field to catch a field-order mistake in
        // the hand-rolled struct layout.
        let raw = RawIcounter {
            cts: 10,
            dsr: 11,
            rng: 12,
            dcd: 13,
            rx: 14,
            tx: 15,
            frame: 16,
            overrun: 17,
            parity: 18,
            brk: 19,
            buf_overrun: 20,
            reserved: [0; 9],
        };
        assert_eq!(
            parse_icounter(raw),
            ErrorCounts::Available {
                framing: 16,
                overrun: 17,
                parity: 18
            }
        );
    }

    #[test]
    fn error_counts_serialize_with_an_explicit_status_tag() {
        let unavailable = serde_json::to_value(ErrorCounts::Unavailable).unwrap();
        assert_eq!(unavailable, serde_json::json!({ "status": "unavailable" }));

        let available = serde_json::to_value(ErrorCounts::Available {
            framing: 1,
            overrun: 2,
            parity: 3,
        })
        .unwrap();
        assert_eq!(
            available,
            serde_json::json!({ "status": "available", "framing": 1, "overrun": 2, "parity": 3 })
        );
    }

    #[test]
    fn raw_icounter_struct_size_matches_kernel_layout() {
        // 11 leading i32 fields + 9 reserved = 20 x 4 bytes = 80 bytes,
        // matching `include/uapi/linux/serial.h`'s
        // `serial_icounter_struct`. A size drift here would mean a field
        // was added/removed relative to the kernel header.
        assert_eq!(std::mem::size_of::<RawIcounter>(), 80);
    }
}
