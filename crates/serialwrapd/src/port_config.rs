//! Port configuration: baud (including arbitrary non-standard values),
//! data bits, parity, stop bits, flow control, and the open-time DTR/RTS
//! policy (`TASKS.md` T1.3, issue #5).
//!
//! # Scope and design
//!
//! This module is the *pure* half of T1.3: plain data types
//! ([`PortConfig`] and friends) plus functions that compute what a
//! termios/ioctl call *should* contain, without ever making one. Every
//! function here takes already-fetched struct fields (or nothing at all)
//! and returns new field values — no `fd`, no `unsafe`, no syscall. That
//! split is deliberate and is what makes "74880 gets encoded correctly, on
//! both platforms" testable at all without hardware: the actual ioctl
//! calls (in `port_io.rs`) are exactly the part a PTY/CI runner *can't*
//! stand in for on macOS (see that module's docs), but the encoding logic
//! — did we build the right bit pattern? — has nothing to do with real
//! hardware and is fully unit-tested here.
//!
//! # Verifying `serialport` crate's actual non-standard-baud behavior
//!
//! Before writing any of this, `serialport` 4.9.0's own behavior was
//! checked directly against its vendored source
//! (`~/.cargo/registry/src/.../serialport-4.9.0/src/posix/termios.rs` and
//! `tty.rs`), not assumed:
//!
//! - **Linux** (`src/posix/termios.rs::set_baud_rate`, non-powerpc arm):
//!   clears `CBAUD`/`CIBAUD` in `c_cflag`, sets `BOTHER`, and writes the
//!   raw `u32` baud straight into `c_ispeed`/`c_ospeed` — no lookup table,
//!   no clamping to a nearest standard rate. This *is* the termios2/BOTHER
//!   path the issue asks for, done correctly.
//! - **macOS** (`src/posix/tty.rs::set_baud_rate`, ios/macos): calls the
//!   `IOSSIOSPEED` ioctl directly with the raw `u32` baud cast to
//!   `libc::speed_t` — again no table, no rounding.
//!
//! So `serialport` already does the right thing on both platforms; there
//! is no need to bypass it because it silently rounds to a standard rate
//! — it doesn't. The reason this crate still hand-rolls the encoding
//! itself (rather than calling `serialport::SerialPort::set_baud_rate`
//! directly) is structural, not correctness-driven: `HotplugDetector`
//! (`port.rs`) owns a raw `std::fs::File` fd via its own `open()` call
//! (see `port_io.rs`'s `open_and_configure`), and re-opening the same
//! device a second time through `serialport::TTYPort::open()` would fight
//! over exclusivity with that fd rather than configure it. Since
//! `serialport`'s own encoding logic is `pub(crate)` (not exported), this
//! module reproduces it — verified field-for-field against the same
//! source above — so the *encoding* can be applied to that existing fd and
//! unit-tested independent of it.
//!
//! One more thing confirmed directly (empirically, on this task's actual
//! development machine, a macOS host) rather than assumed: `IOSSIOSPEED`
//! against a PTY slave fd fails with `ENOTTY` (raw OS error 25,
//! "Inappropriate ioctl for device"), for *both* a standard rate (9600)
//! and 74880 — confirming `serialport`'s own source comment ("attempting
//! to set the baud rate on a pseudo terminal via this ioctl call will fail
//! with the `ENOTTY` error") and confirming this is a PTY limitation, not
//! a non-standard-value rejection. This is exactly why macOS real-baud
//! verification is a `docs/manual-checklist.md` item and cannot be
//! automated — see `port_io.rs`'s docs for how this shaped that module's
//! error handling.

use serde::{Deserialize, Serialize};

/// Number of data bits per character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

/// Parity checking mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    None,
    Odd,
    Even,
}

/// Number of stop bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopBits {
    One,
    Two,
}

/// Flow control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    None,
    /// XON/XOFF (`IXON`/`IXOFF`).
    Software,
    /// RTS/CTS (`CRTSCTS`).
    Hardware,
}

/// What to do with DTR/RTS at `open()` time. See `port_io.rs`'s module
/// docs for the full reasoning; in short: most boards (Arduino, ESP8266/32)
/// reset when DTR toggles, which is why [`OpenControlLines::Preserve`] —
/// touch nothing — is this project's documented default, not merely an
/// option buried in a config struct.
///
/// This is deliberately *not* the same knob as `set_dtr`/`set_rts`/
/// `dtr_pulse` (see `device_profile.rs`'s event-naming docs): those are
/// explicit, independently-invoked, independently-auditable operations on
/// an already-open device; this only governs the one-time decision made
/// while opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum OpenControlLines {
    /// Issue zero `TIOCMBIS`/`TIOCMBIC`/`TIOCMSET` calls during open —
    /// whatever electrical state DTR/RTS are already in is left exactly
    /// alone. "Preserve" means *no write ioctl at all*, not "read the
    /// current level and write it back": on at least some USB-serial
    /// drivers, the underlying MCR (modem control register) is
    /// reprogrammed on *any* `TIOCMSET`-family call regardless of whether
    /// the requested value equals the current one, so even a same-value
    /// write can still produce the hardware pulse this mode exists to
    /// avoid. See `port_io.rs`'s `plan_open_sequence` — this variant is
    /// what makes that plan contain zero `SetControlLine` steps.
    #[default]
    Preserve,
    /// Explicitly drive DTR/RTS to these levels once, right after open.
    Assert { dtr: bool, rts: bool },
}

/// User-facing serial port configuration — the "設定項" this task's issue
/// enumerates. Shared, per-device state (see `device_profile.rs`): one
/// `PortConfig` per `DeviceId`, not per client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortConfig {
    /// Baud rate. Any positive value is accepted — see the module docs for
    /// how 74880 (the ESP8266 boot-log rate that motivated this task) gets
    /// encoded on each platform.
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    pub open_control_lines: OpenControlLines,
}

impl Default for PortConfig {
    /// 9600 8N1, no flow control, DTR/RTS untouched on open — the
    /// conventional serial default, paired with this project's safe
    /// default for the control-line footgun.
    fn default() -> Self {
        Self {
            baud: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            open_control_lines: OpenControlLines::Preserve,
        }
    }
}

// ---------------------------------------------------------------------
// Pure encoding functions. Each operates on already-fetched termios field
// values and returns new ones; none of them touch an fd.
// ---------------------------------------------------------------------

/// `BOTHER` from Linux's `asm-generic/termbits.h`. This ABI constant is a
/// stable part of the termios2 kernel interface. Hardcoded here (rather
/// than depending on `libc::BOTHER`, which the `libc` crate only defines
/// when actually compiling *for* Linux) so [`encode_linux_baud`] compiles
/// and is unit-tested on *every* CI runner, including macOS — the same
/// "platform as a plain value, not a `cfg`" approach `port.rs`'s
/// `describe_open_error`/`filter_platform` already use, for the same
/// reason. Verified against `libc` 0.2.189's own
/// `unix/linux_like/linux/arch/generic/mod.rs` (the arch bucket covering
/// x86_64/aarch64, this project's realistic CI/deployment targets).
pub const LINUX_BOTHER: u32 = 0o010000;
/// `CBAUD` (x86_64/aarch64 glibc value — see [`LINUX_BOTHER`]'s docs).
pub const LINUX_CBAUD: u32 = 0o010017;
/// `CIBAUD` (x86_64/aarch64 glibc value — see [`LINUX_BOTHER`]'s docs).
pub const LINUX_CIBAUD: u32 = 0o02003600000;

/// The `termios2` fields [`encode_linux_baud`] computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxBaudFields {
    pub c_cflag: u32,
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

/// Compute the Linux termios2 fields for `baud`, starting from an
/// already-fetched `c_cflag`. Bit-for-bit mirror of `serialport` 4.9.0's
/// own (private) Linux `set_baud_rate` — see the module docs — reproduced
/// here as a pure function so the encoding itself is unit-testable
/// independent of any real `ioctl`/fd. Accepts (and is meant to be tested
/// against) any `u32` value, including non-standard ones like 74880: there
/// is no lookup table here to fall back to a nearest standard rate.
pub fn encode_linux_baud(c_cflag: u32, baud: u32) -> LinuxBaudFields {
    let c_cflag = (c_cflag & !(LINUX_CBAUD | LINUX_CIBAUD)) | LINUX_BOTHER;
    LinuxBaudFields {
        c_cflag,
        c_ispeed: baud,
        c_ospeed: baud,
    }
}

/// Compute the value macOS's `IOSSIOSPEED` ioctl should be called with for
/// `baud`. Mirrors `serialport` 4.9.0's own macOS `set_baud_rate`, which
/// passes the raw baud straight through with no encoding and no lookup
/// table (see the module docs) — this function is the identity function on
/// purpose, kept as a named, unit-tested step (rather than inlining `as
/// libc::speed_t` at the call site) so "no rounding happens for 74880" is
/// an assertion in this crate's own test suite, not just a fact about
/// `serialport`'s source. Returns a plain `u32` (not `libc::speed_t`,
/// which is a different width on Linux vs. macOS) so this function itself
/// compiles and is testable on every host OS; the real ioctl call site in
/// `port_io.rs` does the final `as libc::speed_t` cast.
pub fn encode_macos_speed(baud: u32) -> u32 {
    baud
}

/// Force `CREAD` (enable the receiver) and `CLOCAL` (ignore modem control
/// lines for open()/read() blocking purposes — *not* related to DTR/RTS
/// output state, see `port_io.rs`'s open-sequence docs) on unconditionally.
/// Both are mandatory baseline for a byte-exact recorder, not
/// user-configurable settings.
pub fn force_cread_clocal(c_cflag: libc::tcflag_t) -> libc::tcflag_t {
    c_cflag | libc::CREAD | libc::CLOCAL
}

/// Encode data bits/parity/stop bits/(hardware) flow control into
/// `c_cflag`. Shared by both platforms: the *bit values* of `CS8`,
/// `PARENB`, etc. differ between Linux and macOS (`libc` supplies the
/// right ones for whichever target this is actually compiled for), but the
/// logic that combines them is identical, so one function serves both —
/// each platform's own CI leg exercises it against its own real
/// `libc::tcflag_t` width and bit layout.
pub fn encode_format_cflag(
    c_cflag: libc::tcflag_t,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
) -> libc::tcflag_t {
    let mut c_cflag = c_cflag;
    let size = match data_bits {
        DataBits::Five => libc::CS5,
        DataBits::Six => libc::CS6,
        DataBits::Seven => libc::CS7,
        DataBits::Eight => libc::CS8,
    };
    c_cflag &= !libc::CSIZE;
    c_cflag |= size;

    match parity {
        Parity::None => c_cflag &= !(libc::PARENB | libc::PARODD),
        Parity::Odd => c_cflag |= libc::PARENB | libc::PARODD,
        Parity::Even => {
            c_cflag |= libc::PARENB;
            c_cflag &= !libc::PARODD;
        }
    }

    match stop_bits {
        StopBits::One => c_cflag &= !libc::CSTOPB,
        StopBits::Two => c_cflag |= libc::CSTOPB,
    }

    match flow_control {
        FlowControl::None | FlowControl::Software => c_cflag &= !libc::CRTSCTS,
        FlowControl::Hardware => c_cflag |= libc::CRTSCTS,
    }

    c_cflag
}

/// Encode software flow control (`IXON`/`IXOFF`) into `c_iflag`. Separate
/// from [`encode_format_cflag`] because XON/XOFF lives in `c_iflag`, not
/// `c_cflag`, unlike every other setting this task covers.
pub fn encode_flow_control_iflag(
    c_iflag: libc::tcflag_t,
    flow_control: FlowControl,
) -> libc::tcflag_t {
    match flow_control {
        FlowControl::Software => c_iflag | libc::IXON | libc::IXOFF,
        FlowControl::None | FlowControl::Hardware => c_iflag & !(libc::IXON | libc::IXOFF),
    }
}

/// Clear the `c_iflag` bits `cfmakeraw()` would clear (`IGNBRK`, `BRKINT`,
/// `PARMRK`, `ISTRIP`, `INLCR`, `IGNCR`, `ICRNL`, `IXON`). Applied manually
/// (rather than calling `libc::cfmakeraw`) because this crate's Linux path
/// uses the wider `termios2` struct, which `cfmakeraw` (defined over plain
/// `termios`) doesn't accept. Mandatory baseline, not user-configurable:
/// `recorder.rs`'s whole design (`rx` is raw bytes, "not yet
/// line-assembled") depends on the tty never doing its own line editing,
/// signal generation, or character remapping.
pub fn raw_mode_iflag(c_iflag: libc::tcflag_t) -> libc::tcflag_t {
    c_iflag
        & !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON)
}

/// Clear `OPOST` (output post-processing, e.g. `\n` -> `\r\n`) — part of
/// the same raw-mode baseline as [`raw_mode_iflag`].
pub fn raw_mode_oflag(c_oflag: libc::tcflag_t) -> libc::tcflag_t {
    c_oflag & !libc::OPOST
}

/// Clear `ECHO`/`ECHONL`/`ICANON`/`ISIG`/`IEXTEN` — part of the same
/// raw-mode baseline as [`raw_mode_iflag`].
pub fn raw_mode_lflag(c_lflag: libc::tcflag_t) -> libc::tcflag_t {
    c_lflag & !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Acceptance criterion 1: 74880 encodes correctly, no fallback ----

    #[test]
    fn linux_baud_74880_sets_bother_and_raw_ispeed_ospeed_not_a_standard_rate() {
        let fields = encode_linux_baud(0, 74_880);
        assert_eq!(fields.c_ispeed, 74_880, "must be the exact requested rate");
        assert_eq!(fields.c_ospeed, 74_880);
        assert_ne!(
            fields.c_ispeed, 57_600,
            "must not fall back to a nearby standard rate"
        );
        assert_ne!(
            fields.c_ispeed, 115_200,
            "must not fall back to a nearby standard rate"
        );
        // `CBAUD` is a multi-bit *selector field*, not independent flags —
        // `BOTHER` is itself one particular value within that field's bit
        // range, meaning "ignore this field, use c_ispeed/c_ospeed
        // instead". So after encoding, `c_cflag & CBAUD` legitimately
        // *equals* `BOTHER` (not zero) — that's the selector being set to
        // the "custom rate" value, which is exactly what should happen.
        assert_eq!(
            fields.c_cflag & LINUX_CBAUD,
            LINUX_BOTHER,
            "the CBAUD selector field must be set to exactly the BOTHER value"
        );
        // CIBAUD (the separate input-speed selector field, used only when
        // input/output speeds differ) does not overlap BOTHER's bits and
        // must be fully cleared, not left stale from a previous config.
        assert_eq!(
            fields.c_cflag & LINUX_CIBAUD,
            0,
            "the CIBAUD field must be cleared, not left stale"
        );
    }

    #[test]
    fn linux_baud_encoding_preserves_unrelated_cflag_bits() {
        let unrelated_bit = 1u32 << 30; // well outside CBAUD/CIBAUD/BOTHER's bit range
        let fields = encode_linux_baud(unrelated_bit, 74_880);
        assert_eq!(
            fields.c_cflag & unrelated_bit,
            unrelated_bit,
            "encoding the baud rate must not clobber other c_cflag bits (e.g. CS8/PARENB/CREAD)"
        );
    }

    #[test]
    fn macos_baud_74880_passes_through_unchanged_not_a_standard_rate() {
        assert_eq!(encode_macos_speed(74_880), 74_880);
        assert_ne!(encode_macos_speed(74_880), 57_600);
        assert_ne!(encode_macos_speed(74_880), 115_200);
    }

    #[test]
    fn macos_baud_encoding_is_the_identity_function_for_standard_rates_too() {
        // Demonstrates there is no branching/lookup table at all — 9600
        // and 74880 are treated identically by this function.
        for baud in [9600, 19_200, 74_880, 115_200, 250_000, 1_500_000] {
            assert_eq!(encode_macos_speed(baud), baud);
        }
    }

    // ---- Format-bit encoding (data bits / parity / stop bits / flow) ----

    #[test]
    fn data_bits_set_csize_and_clear_previous_value() {
        let base = libc::CS7; // start from a stale CS7 to prove it gets replaced
        let c_cflag = encode_format_cflag(
            base,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(c_cflag & libc::CSIZE, libc::CS8);
    }

    #[test]
    fn parity_none_clears_parenb_and_parodd() {
        let base = libc::PARENB | libc::PARODD;
        let c_cflag = encode_format_cflag(
            base,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(c_cflag & (libc::PARENB | libc::PARODD), 0);
    }

    #[test]
    fn parity_odd_sets_parenb_and_parodd() {
        let c_cflag = encode_format_cflag(
            0,
            DataBits::Eight,
            Parity::Odd,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(c_cflag & libc::PARENB, libc::PARENB);
        assert_eq!(c_cflag & libc::PARODD, libc::PARODD);
    }

    #[test]
    fn parity_even_sets_parenb_without_parodd() {
        let c_cflag = encode_format_cflag(
            libc::PARODD,
            DataBits::Eight,
            Parity::Even,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(c_cflag & libc::PARENB, libc::PARENB);
        assert_eq!(c_cflag & libc::PARODD, 0);
    }

    #[test]
    fn stop_bits_two_sets_cstopb_one_clears_it() {
        let c_cflag_two = encode_format_cflag(
            0,
            DataBits::Eight,
            Parity::None,
            StopBits::Two,
            FlowControl::None,
        );
        assert_eq!(c_cflag_two & libc::CSTOPB, libc::CSTOPB);
        let c_cflag_one = encode_format_cflag(
            libc::CSTOPB,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(c_cflag_one & libc::CSTOPB, 0);
    }

    #[test]
    fn hardware_flow_control_sets_crtscts_others_clear_it() {
        let hw = encode_format_cflag(
            0,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::Hardware,
        );
        assert_eq!(hw & libc::CRTSCTS, libc::CRTSCTS);
        let none = encode_format_cflag(
            libc::CRTSCTS,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::None,
        );
        assert_eq!(none & libc::CRTSCTS, 0);
        let sw = encode_format_cflag(
            libc::CRTSCTS,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::Software,
        );
        assert_eq!(
            sw & libc::CRTSCTS,
            0,
            "software flow control must not also assert CRTSCTS"
        );
    }

    #[test]
    fn software_flow_control_sets_ixon_ixoff_in_iflag_only() {
        let iflag = encode_flow_control_iflag(0, FlowControl::Software);
        assert_eq!(iflag & (libc::IXON | libc::IXOFF), libc::IXON | libc::IXOFF);
        let cflag = encode_format_cflag(
            0,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::Software,
        );
        assert_eq!(cflag & libc::CRTSCTS, 0);
    }

    #[test]
    fn hardware_and_none_flow_control_clear_ixon_ixoff() {
        let base = libc::IXON | libc::IXOFF;
        assert_eq!(encode_flow_control_iflag(base, FlowControl::None) & base, 0);
        assert_eq!(
            encode_flow_control_iflag(base, FlowControl::Hardware) & base,
            0
        );
    }

    // ---- Raw-mode baseline ----

    #[test]
    fn raw_mode_clears_canonical_echo_and_signal_generation() {
        let base = libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN | libc::ECHONL;
        assert_eq!(raw_mode_lflag(base), 0);
    }

    #[test]
    fn raw_mode_clears_output_post_processing() {
        assert_eq!(raw_mode_oflag(libc::OPOST), 0);
    }

    #[test]
    fn raw_mode_clears_input_translation_and_software_flow_control() {
        let base = libc::ICRNL
            | libc::INLCR
            | libc::IGNCR
            | libc::ISTRIP
            | libc::IXON
            | libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK;
        assert_eq!(raw_mode_iflag(base), 0);
    }

    #[test]
    fn force_cread_clocal_sets_both_bits_without_clearing_others() {
        let c_cflag = force_cread_clocal(libc::CS8);
        assert_eq!(c_cflag & libc::CREAD, libc::CREAD);
        assert_eq!(c_cflag & libc::CLOCAL, libc::CLOCAL);
        assert_eq!(
            c_cflag & libc::CSIZE,
            libc::CS8,
            "must not disturb unrelated bits"
        );
    }

    // ---- Defaults ----

    #[test]
    fn default_port_config_is_9600_8n1_no_flow_control_preserve_lines() {
        let config = PortConfig::default();
        assert_eq!(config.baud, 9600);
        assert_eq!(config.data_bits, DataBits::Eight);
        assert_eq!(config.parity, Parity::None);
        assert_eq!(config.stop_bits, StopBits::One);
        assert_eq!(config.flow_control, FlowControl::None);
        assert_eq!(config.open_control_lines, OpenControlLines::Preserve);
    }

    // ---- Serialization shape (profile persistence / event payloads rely on this) ----

    #[test]
    fn port_config_round_trips_through_json() {
        let config = PortConfig {
            baud: 74_880,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
            open_control_lines: OpenControlLines::Assert {
                dtr: true,
                rts: false,
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: PortConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }
}
