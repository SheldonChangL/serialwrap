//! PTY-backed mock serial device for tests (`TASKS.md` T0.2).
//!
//! This crate exists so integration tests across the workspace can exercise
//! daemon logic (recording, protocol handling, context-protection
//! collapsing, ...) against something that behaves like a real serial
//! device, without ever touching real hardware. That's the whole point of
//! this task: without it, every later milestone's acceptance tests would
//! need a human with a board plugged in, and CI would be decorative.
//!
//! # Crate boundary
//!
//! This is a standalone workspace member, wired in only via
//! `[dev-dependencies]` (see `crates/serialwrapd/Cargo.toml`) — never a
//! normal dependency of `serialwrapd` or the `serialwrap` binary. That
//! guarantees the PTY/libc plumbing here never links into the release
//! binary, and lets any workspace crate's tests (not just `serialwrapd`'s)
//! depend on it later without reaching into another crate's private
//! `#[cfg(test)]` internals.
//!
//! # Platform note
//!
//! PTY behavior differs subtly between macOS and Linux — most notably,
//! what a reader observes right after the master side closes (a clean EOF
//! vs. an `EIO`/`EBADF`-style error). Tests built on this fixture should
//! accept either outcome wherever the acceptance criteria say "EOF/error"
//! rather than assume one specific platform's behavior. See
//! `docs/manual-checklist.md` for the (larger) set of things that need
//! real hardware and can't be covered by this fixture at all.

mod device;
mod pty;
mod responder;
pub mod script;

pub use device::MockDevice;
pub use responder::Pattern;
