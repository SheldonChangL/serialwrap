//! Daemon core library for serialwrap.
//!
//! `serialwrapd` owns the serial device exclusively and is the only thing
//! that ever calls into termios/ioctl directly. Everything else (the CLI,
//! the MCP bridge, the web GUI) is a client that connects over the UDS
//! protocol defined in `wrap-proto`.
//!
//! This crate depends on `wrap-proto` and must never depend on the
//! `serialwrap` binary crate (dependency direction:
//! `serialwrap` -> `serialwrapd` -> `wrap-proto`).
//!
//! At this stage (`TASKS.md` T0.1) these are module skeletons only — no
//! device I/O, no recording, no protocol handling. See the module docs for
//! which later task fills each one in.

pub mod gate;
pub mod port;
pub mod query;
pub mod recorder;

/// Entry point the `serialwrap daemon` subcommand will call.
///
/// Placeholder for now: starts nothing and returns immediately. The real
/// implementation will bring up the UDS listener, the recorder, and the
/// device-detection loop (see `TASKS.md` T1.1-T1.4).
pub async fn run() -> std::io::Result<()> {
    Ok(())
}
