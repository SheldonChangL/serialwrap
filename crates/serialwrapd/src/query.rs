//! Query layer: line assembly, cursors, filters, and result collapsing over
//! the recorded stream.
//!
//! Not yet implemented — see `TASKS.md` T1.4 (`tail`/`read_since`/`wait_for`)
//! and T3.2 (context-protection collapsing for the MCP bridge).

/// Opaque cursor into the record stream.
///
/// Currently just the underlying `seq`; kept as a distinct type so callers
/// don't assume it stays a bare integer once cross-segment queries land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor(pub u64);
