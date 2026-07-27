//! Write gate: rule engine and approval workflow that stands between a
//! client's write request and bytes actually going out the port.
//!
//! Not yet implemented — see `TASKS.md` T4.1 (rule engine) and T4.2
//! (approval queue, timeouts, notifications).

/// Outcome of evaluating a write request against the gate.
///
/// Not implemented yet; this is the shape future rule-engine code will
/// produce (`TASKS.md` T4.1's `allow` / `pending` / `force_pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Pending { id: u64 },
    Deny { reason: String },
}
