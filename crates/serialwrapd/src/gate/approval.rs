//! `TASKS.md` T4.2's pending-approval queue (issue #15): every write the
//! rule engine (`super::rules`) sends to `Pending`/`ForcePending` sits here
//! until a human decides (or the configured timeout auto-denies it — see
//! `super::Gate::submit_write`, which spawns that timeout race).
//!
//! # Why `decide` is the single point of truth
//!
//! Both a human's `serialwrap approvals approve/deny` *and* the timeout
//! task race to resolve the same pending entry. Rather than have two
//! separate code paths that both try to send on a channel (and reason
//! about which one "wins"), both go through [`PendingQueue::decide`], which
//! does exactly one thing atomically under the queue's mutex:
//! `HashMap::remove`. Whichever caller's `remove` actually returns
//! `Some(entry)` is the one whose decision is delivered — the removal *is*
//! the arbitration, not a separate flag or generation counter. The loser
//! (whichever call happens second, human or timeout) gets
//! [`DecideError::NotFound`], which a CLI caller sees as "already resolved"
//! and the timeout task itself simply ignores (it lost the race to a human
//! who decided first, which is exactly what should happen). This is also
//! why a decided entry disappears from `PendingQueue::list` immediately —
//! there is no separate "resolved" state to linger in.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Serialize;
use tokio::sync::oneshot;

use wrap_proto::ClientType;

use crate::recorder::Recorder;

/// How a pending write was resolved.
#[derive(Debug, Clone)]
pub enum Decision {
    Approved { approved_by: String },
    Denied { reason: String },
}

/// [`PendingQueue::decide`]'s only failure: `id` isn't in the queue — either
/// it was never valid, or it already got resolved (by a human or by
/// timeout) before this call. Deliberately one variant: a caller (CLI or
/// future GUI) doesn't get to distinguish "never existed" from "already
/// decided" over the wire, since both mean the same actionable thing —
/// "there is nothing left here for you to decide".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecideError;

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no such pending approval (already resolved, timed out, or an unknown id)"
        )
    }
}

impl std::error::Error for DecideError {}

/// Everything an approval card (CLI today, GUI in T5.4) needs to show a
/// human about one pending write — see the Security-model wiki's approval
/// payload spec: requester identity, bytes in both forms, which rule
/// forced this (if any) and why, and the log lines immediately before the
/// request, so the operator has context instead of a bare command to guess
/// at.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalSnapshot {
    pub id: u64,
    pub device: String,
    pub requester_name: String,
    pub requester_pid: u32,
    pub requester_type: ClientType,
    /// How many write requests this same client has sent this session
    /// (this one included) — lets an operator see "this agent has already
    /// written 12 times without incident" or "this is its first write and
    /// it's already asking for `flash_erase`" at a glance.
    pub session_request_no: u64,
    pub bytes_b64: String,
    /// Lossy UTF-8 rendering of the bytes — the "readable" form.
    pub bytes_text: String,
    /// Uppercase, space-separated hex — the "raw" form, same style
    /// `serialwrap write --hex` accepts back.
    pub bytes_hex: String,
    /// `Some("danger:<pattern>")` if a danger rule forced this to
    /// approval; `None` for a default-pending write (nothing matched
    /// either list).
    pub matched_rule: Option<String>,
    /// The matched danger rule's rationale (see `rules::BUILTIN_DANGER`'s
    /// doc comment) — `None` alongside `matched_rule: None`.
    pub danger_reason: Option<String>,
    /// The log lines immediately preceding this request, oldest first —
    /// see the Security-model wiki: "a `flash_erase` immediately after
    /// `ota: image invalid, rollback armed` may be entirely correct;
    /// without that context an operator is just guessing."
    pub log_context: Vec<String>,
    pub age_s: f64,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Everything about one pending write except the channel to resolve it —
/// kept in its own struct so [`PendingQueue::list`] can build a snapshot
/// without touching the `oneshot::Sender`.
struct PendingMeta {
    device: String,
    requester_name: String,
    requester_pid: u32,
    requester_type: ClientType,
    session_request_no: u64,
    bytes: Vec<u8>,
    matched_rule: Option<String>,
    danger_reason: Option<String>,
    log_context: Vec<String>,
    created_at: Instant,
    /// This device's recorder, kept so [`PendingQueue::decide`] can append
    /// the `approve`/`deny` audit record without its caller (a CLI request
    /// handler that may not even know which device an opaque approval `id`
    /// belongs to) having to look it up separately.
    recorder: Arc<Recorder>,
}

impl PendingMeta {
    fn to_snapshot(&self, id: u64) -> ApprovalSnapshot {
        ApprovalSnapshot {
            id,
            device: self.device.clone(),
            requester_name: self.requester_name.clone(),
            requester_pid: self.requester_pid,
            requester_type: self.requester_type,
            session_request_no: self.session_request_no,
            bytes_b64: BASE64.encode(&self.bytes),
            bytes_text: String::from_utf8_lossy(&self.bytes).into_owned(),
            bytes_hex: to_hex(&self.bytes),
            matched_rule: self.matched_rule.clone(),
            danger_reason: self.danger_reason.clone(),
            log_context: self.log_context.clone(),
            age_s: self.created_at.elapsed().as_secs_f64(),
        }
    }
}

/// Parameters for [`PendingQueue::submit`] — a plain data bag so
/// `super::Gate::submit_write` (which already has all of these to hand)
/// doesn't have to call a nine-argument function.
pub(crate) struct NewPending {
    pub device: String,
    pub requester_name: String,
    pub requester_pid: u32,
    pub requester_type: ClientType,
    pub session_request_no: u64,
    pub bytes: Vec<u8>,
    pub matched_rule: Option<String>,
    pub danger_reason: Option<String>,
    pub log_context: Vec<String>,
    pub recorder: Arc<Recorder>,
}

struct PendingEntry {
    meta: PendingMeta,
    tx: oneshot::Sender<Decision>,
}

/// The pending-approval table itself. See the module docs for why `decide`
/// (used by both a human's decision and an internal timeout) is the single
/// serialization point that makes concurrent resolution safe.
#[derive(Default)]
pub(crate) struct PendingQueue {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, PendingEntry>>,
}

impl PendingQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a new pending write, returning its assigned id and the
    /// receiver half of the channel that will carry its eventual
    /// [`Decision`]. The id is globally unique across every device (not
    /// per-device), since it's the one identifier a human or CLI ever
    /// refers to an approval by — a per-device id would require also
    /// naming the device to disambiguate, which `serialwrap approvals
    /// approve <id>` deliberately doesn't require.
    pub(crate) fn submit(&self, new: NewPending) -> (u64, oneshot::Receiver<Decision>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        let meta = PendingMeta {
            device: new.device,
            requester_name: new.requester_name,
            requester_pid: new.requester_pid,
            requester_type: new.requester_type,
            session_request_no: new.session_request_no,
            bytes: new.bytes,
            matched_rule: new.matched_rule,
            danger_reason: new.danger_reason,
            log_context: new.log_context,
            created_at: Instant::now(),
            recorder: new.recorder,
        };
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, PendingEntry { meta, tx });
        (id, rx)
    }

    /// Resolve `id` with `decision` — see the module docs for why this is
    /// the one place both a human decision and an internal timeout must
    /// come through, and why `remove` itself is the arbitration.
    pub(crate) fn decide(&self, id: u64, decision: Decision) -> Result<(), DecideError> {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .ok_or(DecideError)?;

        // Audit trail: `request_seq` here is this same pending `id`, not a
        // recorder-native `seq` — it only needs to correlate this decision
        // back to whichever `"request"` gate record `Gate::submit_write`
        // appended for this same id, which is exactly what a shared
        // correlation id (rather than a literal on-disk seq) is for.
        let (action, detail) = match &decision {
            Decision::Approved { approved_by } => ("approve", format!("approved_by:{approved_by}")),
            Decision::Denied { reason } => ("deny", reason.clone()),
        };
        if let Err(e) = entry.meta.recorder.append_gate(action, detail, id) {
            eprintln!(
                "serialwrapd: gate: failed to append gate audit record for pending approval \
                 {id}: {e}"
            );
        }

        // The receive half may already be gone in a pathological shutdown
        // race (the requesting connection's task was itself aborted); that
        // is not this function's problem to report — the decision is
        // durably audited above regardless, and there is no requester left
        // to deliver it to.
        let _ = entry.tx.send(decision);
        Ok(())
    }

    /// A snapshot of every currently pending write, sorted by ascending
    /// id (oldest request first — also gives test assertions and CLI
    /// output a deterministic order despite the backing `HashMap`).
    pub(crate) fn list(&self) -> Vec<ApprovalSnapshot> {
        let mut snapshots: Vec<ApprovalSnapshot> = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, entry)| entry.meta.to_snapshot(*id))
            .collect();
        snapshots.sort_by_key(|s| s.id);
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::RecorderConfig;

    fn test_recorder() -> (Arc<Recorder>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let recorder =
            Recorder::open(dir.path(), "dev", RecorderConfig::default()).expect("open recorder");
        (Arc::new(recorder), dir)
    }

    fn new_pending(recorder: &Arc<Recorder>, bytes: &[u8]) -> NewPending {
        NewPending {
            device: "dev".to_string(),
            requester_name: "claude-code".to_string(),
            requester_pid: 4242,
            requester_type: ClientType::Agent,
            session_request_no: 1,
            bytes: bytes.to_vec(),
            matched_rule: None,
            danger_reason: None,
            log_context: vec!["boot ok".to_string()],
            recorder: Arc::clone(recorder),
        }
    }

    #[test]
    fn decide_unknown_id_returns_not_found() {
        let queue = PendingQueue::new();
        assert_eq!(
            queue.decide(999, Decision::Denied { reason: "x".into() }),
            Err(DecideError)
        );
    }

    #[tokio::test]
    async fn submit_then_approve_delivers_the_decision() {
        let (recorder, _dir) = test_recorder();
        let queue = PendingQueue::new();
        let (id, rx) = queue.submit(new_pending(&recorder, b"status"));
        queue
            .decide(
                id,
                Decision::Approved {
                    approved_by: "sheldon:1000".to_string(),
                },
            )
            .expect("decide succeeds while pending");
        match rx.await.expect("sender not dropped without sending") {
            Decision::Approved { approved_by } => assert_eq!(approved_by, "sheldon:1000"),
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deciding_twice_returns_not_found_the_second_time() {
        let (recorder, _dir) = test_recorder();
        let queue = PendingQueue::new();
        let (id, _rx) = queue.submit(new_pending(&recorder, b"status"));
        assert!(queue
            .decide(
                id,
                Decision::Denied {
                    reason: "first".into()
                }
            )
            .is_ok());
        assert_eq!(
            queue.decide(
                id,
                Decision::Denied {
                    reason: "second".into()
                }
            ),
            Err(DecideError),
            "an already-resolved id must not be decidable again"
        );
    }

    #[test]
    fn list_reflects_current_pending_entries_sorted_by_id() {
        let (recorder, _dir) = test_recorder();
        let queue = PendingQueue::new();
        let (id_a, _rx_a) = queue.submit(new_pending(&recorder, b"one"));
        let (id_b, _rx_b) = queue.submit(new_pending(&recorder, b"two"));
        let snapshot_ids: Vec<u64> = queue.list().iter().map(|s| s.id).collect();
        assert_eq!(snapshot_ids, vec![id_a, id_b]);

        queue
            .decide(id_a, Decision::Denied { reason: "x".into() })
            .unwrap();
        let remaining: Vec<u64> = queue.list().iter().map(|s| s.id).collect();
        assert_eq!(
            remaining,
            vec![id_b],
            "a resolved entry must disappear from list()"
        );
    }

    #[tokio::test]
    async fn five_concurrent_pendings_resolve_independently() {
        let (recorder, _dir) = test_recorder();
        let queue = PendingQueue::new();
        let mut ids_and_rx = Vec::new();
        for i in 0..5u8 {
            let (id, rx) = queue.submit(new_pending(&recorder, &[i]));
            ids_and_rx.push((id, rx));
        }

        // Decide in reverse order, alternating approve/deny, to prove
        // there's no ordering assumption baked into the queue.
        for (i, (id, _)) in ids_and_rx.iter().enumerate().rev() {
            let decision = if i % 2 == 0 {
                Decision::Approved {
                    approved_by: format!("operator-{i}"),
                }
            } else {
                Decision::Denied {
                    reason: format!("reason-{i}"),
                }
            };
            queue.decide(*id, decision).expect("decide succeeds");
        }

        for (i, (_, rx)) in ids_and_rx.into_iter().enumerate() {
            match rx.await.expect("channel resolved") {
                Decision::Approved { approved_by } if i % 2 == 0 => {
                    assert_eq!(approved_by, format!("operator-{i}"));
                }
                Decision::Denied { reason } if i % 2 != 0 => {
                    assert_eq!(reason, format!("reason-{i}"));
                }
                other => panic!("request {i} got the wrong outcome: {other:?}"),
            }
        }
    }
}
