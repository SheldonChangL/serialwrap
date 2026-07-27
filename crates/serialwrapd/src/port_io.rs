//! Real syscalls that apply [`crate::port_config`]'s pure types to an
//! actually-open serial device fd (`TASKS.md` T1.3, issue #5).
//!
//! # `open()`'s own side effects on control lines, and why this sequence
//!
//! POSIX serial semantics make DTR/RTS harder to "just not touch" than it
//! looks:
//!
//! - Opening a tty **without** `O_NONBLOCK` and without `CLOCAL` already
//!   set blocks until carrier detect (DCD) — legacy dial-up modem
//!   semantics baked into the terminal driver. That's a hang risk on
//!   first open of a device whose termios doesn't already have `CLOCAL`
//!   set (a fresh device, or one left by a previous process in an
//!   unknown state) and is unrelated to DTR/RTS *output* — but it means
//!   the very first open must use `O_NONBLOCK`.
//! - Many USB-serial drivers (CH340/CP210x/FTDI clones) assert DTR/RTS as
//!   part of their own device-bring-up sequence at `open()` time, at the
//!   USB-control-request level, entirely below any termios/ioctl call
//!   this process could make. **Nothing in userspace can prevent this**
//!   — by the time `open()` returns, whatever the driver did has already
//!   happened. This is the actual well-known root cause of "opening the
//!   serial port resets my Arduino", and it is exactly why
//!   `docs/manual-checklist.md` §2 requires real hardware: no test
//!   fixture (a PTY has no such driver at all) can prove or disprove it.
//! - The only userspace-visible lever this process *does* control is
//!   whether *it itself* ever issues a `TIOCMBIS`/`TIOCMBIC`/`TIOCMSET`
//!   call. [`OpenControlLines::Preserve`] is the honest contract this
//!   module can actually keep: *this process* will not touch the lines.
//!   See [`plan_open_sequence`] — the whole point of exposing this as a
//!   pure, unit-tested plan is that "Preserve mode calls zero
//!   `SetControlLine` ops" is something a test can assert directly,
//!   independent of what a specific board's driver does on top of it.
//!
//! Given that, the sequence this module actually follows is:
//!
//! 1. `open(path, O_RDWR | O_NOCTTY | O_NONBLOCK)` — nonblocking so this
//!    call can never hang on DCD.
//! 2. Apply termios (baud/format bits/raw mode + force `CLOCAL`) — this
//!    never touches DTR/RTS (there is no such bit in `c_cflag`; DTR/RTS
//!    are `TIOCM*`-ioctl-only). Once `CLOCAL` is set, later blocking
//!    reads/writes on this fd are safe regardless of carrier state.
//! 3. *Only* if [`OpenControlLines::Assert`] was requested: issue the
//!    `TIOCMBIS`/`TIOCMBIC` calls for DTR/RTS. Skipped entirely for
//!    `Preserve` — see [`plan_open_sequence`].
//! 4. Clear `O_NONBLOCK` (`fcntl(F_SETFL)`) — safe now that `CLOCAL` is
//!    guaranteed set; restores the fully-blocking-read assumption T1.1's
//!    reader thread was written against (it already handles readiness via
//!    its own `poll()` loop either way, so this is about not changing
//!    write-blocking behavior out from under later tasks more than it is
//!    about the reader).
//!
//! # Why a config-application failure doesn't fail the whole open
//!
//! `open_and_configure`'s [`PortOp::ApplyTermios`]/[`PortOp::SetControlLine`]
//! steps can genuinely fail even in production (a device that doesn't
//! support every ioctl) — and, discovered directly during this task,
//! **always** fail in this crate's own PTY-backed tests on macOS: macOS's
//! `IOSSIOSPEED` returns `ENOTTY` against a pseudo-terminal (verified
//! empirically against a real PTY on this task's macOS development
//! machine — see `port_config.rs`'s docs). T1.1's own existing tests also
//! stand in for "a device" with a plain regular file, which doesn't
//! support *any* termios ioctl. Rather than let either case make every
//! connection attempt look like an `open_failed`, this module treats "the
//! path opened" as the connect signal and "did every configuration step
//! apply" as separate, best-effort information — see `open_and_configure`'s
//! return type.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use crate::port_config::{self, OpenControlLines, PortConfig};

/// Which control line an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlLine {
    Dtr,
    Rts,
}

impl ControlLine {
    /// Short machine-readable name, used in `control_line_change` events.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlLine::Dtr => "dtr",
            ControlLine::Rts => "rts",
        }
    }
}

/// One primitive step in opening and configuring a port. See
/// [`plan_open_sequence`] and the module docs' "why this sequence" note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortOp {
    OpenNonblocking,
    ApplyTermios,
    SetControlLine { line: ControlLine, level: bool },
    ClearNonblocking,
}

/// Pure planning step: decide which [`PortOp`]s [`open_and_configure`] will
/// perform, in order, for this `open_control_lines` policy. This is the
/// actual unit-tested contract behind "Preserve mode never touches
/// DTR/RTS" (acceptance criterion 2) — no real fd involved, just a `Vec`
/// to assert on.
pub fn plan_open_sequence(open_control_lines: OpenControlLines) -> Vec<PortOp> {
    let mut ops = vec![PortOp::OpenNonblocking, PortOp::ApplyTermios];
    if let OpenControlLines::Assert { dtr, rts } = open_control_lines {
        ops.push(PortOp::SetControlLine {
            line: ControlLine::Dtr,
            level: dtr,
        });
        ops.push(PortOp::SetControlLine {
            line: ControlLine::Rts,
            level: rts,
        });
    }
    ops.push(PortOp::ClearNonblocking);
    ops
}

/// Open `path` and apply `config`, mechanically following exactly
/// [`plan_open_sequence`]'s plan for `config.open_control_lines` — the
/// plan *is* what runs, so there is no way for this executor to silently
/// diverge from what the plan (and its tests) say it does.
///
/// Returns the open file plus, if any *non-open* step failed, the first
/// such error. See the module docs for why that error is not folded into
/// the outer `Result`.
pub fn open_and_configure(
    path: &Path,
    config: &PortConfig,
) -> io::Result<(File, Option<io::Error>)> {
    let file = open_nonblocking(path)?;
    let fd = file.as_raw_fd();
    let mut first_err: Option<io::Error> = None;

    for op in plan_open_sequence(config.open_control_lines) {
        let result = match op {
            PortOp::OpenNonblocking => Ok(()), // already done above; open() itself is a hard failure
            PortOp::ApplyTermios => apply_termios(fd, config),
            PortOp::SetControlLine { line, level } => set_control_line(fd, line, level),
            PortOp::ClearNonblocking => clear_nonblocking(fd),
        };
        if let Err(e) = result {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }

    Ok((file, first_err))
}

fn open_nonblocking(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)
}

fn clear_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Re-apply `config`'s termios settings (baud/format bits/raw mode) to an
/// already-open, already-connected fd — used both by
/// [`open_and_configure`]'s initial application and by a live
/// `set_port_config` call while the device is connected (see `port.rs`'s
/// `PortConfigApi`). Never touches DTR/RTS.
#[cfg(target_os = "linux")]
pub fn apply_termios(fd: RawFd, config: &PortConfig) -> io::Result<()> {
    let mut t: libc::termios2 = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TCGETS2, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    t.c_iflag = port_config::raw_mode_iflag(t.c_iflag);
    t.c_iflag = port_config::encode_flow_control_iflag(t.c_iflag, config.flow_control);
    t.c_oflag = port_config::raw_mode_oflag(t.c_oflag);
    t.c_lflag = port_config::raw_mode_lflag(t.c_lflag);
    t.c_cflag = port_config::force_cread_clocal(t.c_cflag);
    t.c_cflag = port_config::encode_format_cflag(
        t.c_cflag,
        config.data_bits,
        config.parity,
        config.stop_bits,
        config.flow_control,
    );
    let baud = port_config::encode_linux_baud(t.c_cflag, config.baud);
    t.c_cflag = baud.c_cflag;
    t.c_ispeed = baud.c_ispeed;
    t.c_ospeed = baud.c_ospeed;
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;

    if unsafe { libc::ioctl(fd, libc::TCSETS2, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// See the Linux version's docs. macOS uses the plain `termios` struct for
/// everything except baud, and a separate `IOSSIOSPEED` ioctl for baud
/// (see `port_config.rs`'s module docs for why, verified against
/// `serialport` 4.9.0's source) — which, per that same source's comment
/// and confirmed empirically on this task's own macOS machine against a
/// real PTY, fails with `ENOTTY` on a pseudo-terminal. That failure is
/// intentionally allowed to propagate out of *this* function; it is the
/// caller ([`open_and_configure`], and `port.rs`'s live-reconfigure path)
/// that decides not to let it block the connection or the whole call.
#[cfg(target_os = "macos")]
pub fn apply_termios(fd: RawFd, config: &PortConfig) -> io::Result<()> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    t.c_iflag = port_config::raw_mode_iflag(t.c_iflag);
    t.c_iflag = port_config::encode_flow_control_iflag(t.c_iflag, config.flow_control);
    t.c_oflag = port_config::raw_mode_oflag(t.c_oflag);
    t.c_lflag = port_config::raw_mode_lflag(t.c_lflag);
    t.c_cflag = port_config::force_cread_clocal(t.c_cflag);
    t.c_cflag = port_config::encode_format_cflag(
        t.c_cflag,
        config.data_bits,
        config.parity,
        config.stop_bits,
        config.flow_control,
    );
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;

    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let speed = port_config::encode_macos_speed(config.baud) as libc::speed_t;
    if unsafe { libc::ioctl(fd, MACOS_IOSSIOSPEED, &speed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Neither Linux nor macOS: no termios support assumed. Kept so this crate
/// still compiles on an unanticipated target rather than hard-failing the
/// whole build; production code never reaches this in practice.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn apply_termios(_fd: RawFd, _config: &PortConfig) -> io::Result<()> {
    Err(io::Error::other(
        "termios configuration is not implemented for this platform",
    ))
}

/// `IOSSIOSPEED`'s ioctl request number. Not present in the `libc` crate
/// at all (checked: no hit anywhere in `libc` 0.2.189's source) — this is
/// the same hardcoded value `serialport` 4.9.0 itself uses
/// (`src/posix/ioctl.rs`), which is the correct, longstanding Darwin ABI
/// constant from `<IOKit/serial/ioss.h>`.
#[cfg(target_os = "macos")]
const MACOS_IOSSIOSPEED: libc::c_ulong = 0x8004_5402;

/// Assert (`level = true`) or deassert (`level = false`) one control line
/// via `TIOCMBIS`/`TIOCMBIC`. Portable: `TIOCM_DTR`/`TIOCM_RTS`/
/// `TIOCMBIS`/`TIOCMBIC` are all defined identically-named (if
/// differently-valued) by `libc` on both Linux and macOS, so this needs no
/// `cfg` at all.
pub fn set_control_line(fd: RawFd, line: ControlLine, level: bool) -> io::Result<()> {
    let mut bits: libc::c_int = match line {
        ControlLine::Dtr => libc::TIOCM_DTR,
        ControlLine::Rts => libc::TIOCM_RTS,
    };
    let request = if level {
        libc::TIOCMBIS
    } else {
        libc::TIOCMBIC
    };
    if unsafe { libc::ioctl(fd, request, &mut bits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read current DTR/RTS levels via `TIOCMGET`. Returns `(dtr, rts)`.
pub fn get_control_lines(fd: RawFd) -> io::Result<(bool, bool)> {
    let mut bits: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::TIOCMGET, &mut bits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((bits & libc::TIOCM_DTR != 0, bits & libc::TIOCM_RTS != 0))
}

/// Pulse DTR: deassert, hold for `duration`, then reassert. A distinct,
/// independently-invoked operation — not a `set_config` parameter — so a
/// future rule engine/audit trail (T4.1) can tell "this reset the board"
/// apart from "this changed a control line" (see `device_profile.rs`'s
/// event-naming docs). The deassert-then-reassert direction matches the
/// conventional Arduino-bootloader auto-reset convention; exact polarity
/// is board-wiring-dependent and — like all real DTR/RTS electrical
/// behavior — cannot be verified without hardware (`docs/manual-checklist.md`
/// §2).
pub fn dtr_pulse(fd: RawFd, duration: Duration) -> io::Result<()> {
    set_control_line(fd, ControlLine::Dtr, false)?;
    std::thread::sleep(duration);
    set_control_line(fd, ControlLine::Dtr, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Acceptance criterion 2: the ioctl call *sequence* is what's
    // testable without hardware; DTR/RTS electrical behavior isn't. ----

    #[test]
    fn preserve_mode_issues_zero_control_line_operations() {
        let ops = plan_open_sequence(OpenControlLines::Preserve);
        assert!(
            ops.iter()
                .all(|op| !matches!(op, PortOp::SetControlLine { .. })),
            "Preserve must never plan a TIOCM*-touching op, got {ops:?}"
        );
        assert_eq!(
            ops,
            vec![
                PortOp::OpenNonblocking,
                PortOp::ApplyTermios,
                PortOp::ClearNonblocking
            ]
        );
    }

    #[test]
    fn assert_mode_plans_exactly_dtr_then_rts_between_termios_and_clearing_nonblock() {
        let ops = plan_open_sequence(OpenControlLines::Assert {
            dtr: true,
            rts: false,
        });
        assert_eq!(
            ops,
            vec![
                PortOp::OpenNonblocking,
                PortOp::ApplyTermios,
                PortOp::SetControlLine {
                    line: ControlLine::Dtr,
                    level: true
                },
                PortOp::SetControlLine {
                    line: ControlLine::Rts,
                    level: false
                },
                PortOp::ClearNonblocking,
            ]
        );
    }

    #[test]
    fn open_always_happens_before_termios_which_happens_before_clearing_nonblock() {
        for lines in [
            OpenControlLines::Preserve,
            OpenControlLines::Assert {
                dtr: true,
                rts: true,
            },
        ] {
            let ops = plan_open_sequence(lines);
            let open_idx = ops
                .iter()
                .position(|o| *o == PortOp::OpenNonblocking)
                .unwrap();
            let termios_idx = ops.iter().position(|o| *o == PortOp::ApplyTermios).unwrap();
            let clear_idx = ops
                .iter()
                .position(|o| *o == PortOp::ClearNonblocking)
                .unwrap();
            assert!(open_idx < termios_idx);
            assert!(termios_idx < clear_idx);
        }
    }

    #[test]
    fn control_line_as_str_matches_expected_event_field_values() {
        assert_eq!(ControlLine::Dtr.as_str(), "dtr");
        assert_eq!(ControlLine::Rts.as_str(), "rts");
    }
}
