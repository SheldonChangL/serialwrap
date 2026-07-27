//! Write gate: the rule engine (`TASKS.md` T4.1, issue #14) and approval
//! workflow (`TASKS.md` T4.2, issue #15) that stand between an `agent`
//! client's write request and bytes actually going out the port.
//!
//! # Why this exists at all
//!
//! A serial write can be physically irreversible — a flash erase, a blown
//! fuse, a bricked bootloader. An LLM agent that misreads a log line and
//! confidently issues the wrong command does real, sometimes
//! un-fixable-without-hardware damage, and it does so with entirely good
//! intentions, which is exactly why "the agent should just be careful"
//! isn't a mitigation. See the [Security-model
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model)
//! for the full policy this module implements; the short version:
//!
//! - **Humans bypass the gate; agents don't.** The gate answers to whoever
//!   operates it — gating a human only teaches them to turn the gate off.
//!   A human's write is still always audited, just never blocked (see
//!   `protocol::session`'s `Request::Write` handler, T2.1's territory).
//! - **Timeout means deny, fail-safe.** An unattended pending approval
//!   resolves to denial, never to allow-by-default — see [`approval`]'s
//!   module docs.
//! - **A danger pattern can never be whitelisted away at decision time.**
//!   The only sanctioned way to change what counts as dangerous is editing
//!   `rules.toml` itself (a deliberate, considered admin action) — never a
//!   checkbox on an approval card in the moment, when the operator is in
//!   exactly the wrong state of mind to make that call calmly. See
//!   [`rules::RuleSet::evaluate`].
//!
//! # Module layout
//!
//! - [`rules`]: `rules.toml` loading and the pure whitelist/danger/priority
//!   matching logic ([`rules::RuleSet`]).
//! - [`approval`]: the pending-approval queue, timeout race, and audit
//!   trail ([`approval::PendingQueue`]).
//! - [`notify`]: best-effort desktop notification ([`notify::Notifier`]).
//!
//! [`Gate`] is what `protocol::session` actually holds one of (via
//! `protocol::server::Shared::gate`) and calls into per write request.

pub mod approval;
pub mod notify;
pub mod rules;

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use wrap_proto::ClientType;

use crate::recorder::Recorder;
use approval::{ApprovalSnapshot, DecideError, Decision, NewPending, PendingQueue};
use notify::Notifier;
use rules::{RuleSet, RuleVerdict};

/// Outcome of [`Gate::submit_write`] — T4.1's required decision shape
/// (`TASKS.md`: "判定結果型別：`allow(reason)` / `pending(id)` /
/// `force_pending(id, matched_rule)`"), verbatim.
///
/// `Pending`/`ForcePending` are not final outcomes by themselves — they
/// mean "sitting in the approval queue under this `id`"; the caller (see
/// `protocol::session`'s `Request::Write` handler) awaits the
/// [`approval::Decision`] that eventually arrives on the receiver
/// [`Gate::submit_write`] also returns, exactly like `wait_for`'s existing
/// blocking-this-one-request'S-own-task pattern (this request's spawned
/// task blocks; other requests on the same connection are unaffected — see
/// `protocol::session`'s module docs on why every request is its own
/// task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// A whitelist rule matched (and no danger rule did) — write proceeds
    /// immediately, no queue involved.
    Allow { reason: String },
    /// Neither list matched: default-pending, no rule identifies why.
    Pending { id: u64 },
    /// A danger rule matched, forcing approval regardless of any whitelist
    /// match (see [`rules::RuleSet::evaluate`]).
    ForcePending { id: u64, matched_rule: String },
}

/// Everything [`Gate::submit_write`] needs about *who* is asking, beyond
/// the bytes themselves — looked up fresh by the caller from
/// `protocol::registry::ClientRegistry` (never cached), same "ask again
/// every time" convention `type_and_permission` already documents.
pub struct RequesterCtx {
    pub device: String,
    pub name: String,
    pub pid: u32,
    pub client_type: ClientType,
    /// This client's Nth write request this session (1-based, this
    /// request included) — see `ApprovalSnapshot::session_request_no`.
    pub session_request_no: u64,
}

/// How many lines of preceding log context an approval payload carries —
/// see the Security-model wiki: a bare `flash_erase` looks alarming; the
/// same line right after `ota: image invalid, rollback armed` may be
/// exactly correct, and an operator with no context is just guessing.
pub const DEFAULT_LOG_CONTEXT_LINES: usize = 20;

/// Ties the rule engine, the pending queue, and desktop notification
/// together. One instance lives on `protocol::server::Shared` for the
/// whole daemon's lifetime.
pub struct Gate {
    rules: RuleSet,
    // `Arc`-wrapped (not a plain field) specifically so `submit_write`'s
    // timeout task — a `tokio::spawn`ed `'static` future — can hold its own
    // clone rather than a reference into `self`. `Gate` lives on
    // `protocol::server::Shared` for the daemon's whole lifetime in
    // production, but tests construct short-timeout `Gate`s that can be
    // dropped (e.g. at the end of a test function) before a spawned
    // timeout task has actually run; a raw/borrowed reference would make
    // that a use-after-free, whereas an `Arc` clone simply keeps the queue
    // alive for exactly as long as anything still needs it.
    queue: Arc<PendingQueue>,
    notifier: Arc<dyn Notifier>,
}

impl Gate {
    pub fn new(rules: RuleSet, notifier: Arc<dyn Notifier>) -> Self {
        Self {
            rules,
            queue: Arc::new(PendingQueue::new()),
            notifier,
        }
    }

    /// The production default: [`RuleSet::builtin`] (danger patterns only,
    /// no whitelist, 60s timeout) plus real desktop notifications. What
    /// `protocol::server::Shared::new` constructs by default — call
    /// [`crate::protocol::Shared::with_gate`] to override (e.g. with a
    /// loaded `rules.toml`, a short test timeout, or a notifier double).
    pub fn builtin() -> Self {
        Self::new(RuleSet::builtin(), Arc::new(notify::DesktopNotifier))
    }

    /// Evaluate and, if needed, enqueue a write for approval.
    ///
    /// `recorder` is this write's target device's recorder (used to append
    /// the `gate` audit trail for pending/force-pending writes — an
    /// immediate `Allow` doesn't get one: the eventual `tx` record's own
    /// `gate` field is that write's audit trail, see `protocol::session`'s
    /// `Request::Write` handler). `log_context` is the caller-supplied
    /// preceding log lines (fetched via `protocol::registry::QueryRegistry`
    /// before calling in, since `Gate` itself has no reason to know about
    /// query state — see this crate's dependency-narrow philosophy already
    /// applied to `protocol::backend::DeviceBackend`).
    ///
    /// Returns the decision plus, for `Pending`/`ForcePending`, the
    /// receiver the caller must await for the eventual [`Decision`] — the
    /// pending entry's timeout race is already running by the time this
    /// returns (see below).
    pub fn submit_write(
        &self,
        recorder: &Arc<Recorder>,
        bytes: &[u8],
        ctx: RequesterCtx,
        log_context: Vec<String>,
    ) -> (
        GateDecision,
        Option<tokio::sync::oneshot::Receiver<Decision>>,
    ) {
        // A single match on one `evaluate` call — evaluating twice (once to
        // check for `Allow`, again to destructure `ForcePending`) would
        // also work but wastefully re-runs every regex in the rule set a
        // second time for no reason.
        let (matched_rule, danger_reason) = match self.rules.evaluate(bytes) {
            RuleVerdict::Allow { reason } => return (GateDecision::Allow { reason }, None),
            RuleVerdict::ForcePending {
                matched_rule,
                danger_reason,
            } => (Some(matched_rule), Some(danger_reason)),
            RuleVerdict::Pending => (None, None),
        };

        // Grabbed before `ctx` is partially moved into `NewPending` below —
        // all four are `Copy`/cheap-to-clone and still needed afterward: the
        // notification body (keyed by device+pid for a human glancing at a
        // popup, not by the daemon-internal `client_id`), and the
        // `write_request` audit event below (T4.3, issue #16), which needs
        // the full requester identity alongside the bytes.
        let device = ctx.device.clone();
        let pid = ctx.pid;
        let requester_name = ctx.name.clone();
        let requester_type = ctx.client_type;
        let session_request_no = ctx.session_request_no;

        let (id, rx) = self.queue.submit(NewPending {
            device: ctx.device,
            requester_name: ctx.name,
            requester_pid: ctx.pid,
            requester_type: ctx.client_type,
            session_request_no: ctx.session_request_no,
            bytes: bytes.to_vec(),
            matched_rule: matched_rule.clone(),
            danger_reason: danger_reason.clone(),
            log_context,
            recorder: Arc::clone(recorder),
        });

        // Audit the request itself — the one on-disk trace of a write
        // attempt that might never produce a `tx` record at all (denied or
        // timed out). `request_seq` is this same `id`: see
        // `approval::PendingQueue::decide`'s doc comment on why a shared
        // correlation id, not a literal recorder `seq`, is what this field
        // is for.
        let request_label = matched_rule
            .clone()
            .unwrap_or_else(|| "default_pending".to_string());
        if let Err(e) = recorder.append_gate("request", request_label, id) {
            eprintln!(
                "serialwrapd: gate: failed to append gate request audit record for pending \
                 {id}: {e}"
            );
        }

        // Full-payload audit trail (`TASKS.md` T4.3, issue #16): a denied or
        // timed-out write never produces a `tx` record at all (see
        // `protocol::session`'s `write_and_reply` doc comment — only an
        // eventual *successful* write gets one), so without this, the one
        // thing an operator most wants to know after a denial — "what did
        // it actually try to send?" — would be unrecoverable the moment
        // this pending entry resolves and drops out of
        // `PendingQueue::list`. Deliberately *not* a new field on
        // `Record::Gate` (which would mean extending `recorder.rs`'s/
        // `wrap-proto`'s on-disk schema): `Recorder::append_event` already
        // accepts arbitrary `extra` fields, so a distinctly-named event
        // carries the full requester identity + bytes with no schema
        // change at all — exactly the "audit is a query view over the
        // existing stream, not a second store" stance this task's own docs
        // insist on. `request_id` mirrors `request_seq` above: the same
        // correlation id a `serialwrap audit` view joins the eventual
        // approve/deny `gate` record against.
        let mut request_extra = serde_json::Map::new();
        request_extra.insert("request_id".to_string(), id.into());
        request_extra.insert("device".to_string(), device.clone().into());
        request_extra.insert("requester_name".to_string(), requester_name.into());
        request_extra.insert("requester_pid".to_string(), pid.into());
        request_extra.insert(
            "requester_type".to_string(),
            serde_json::to_value(requester_type).unwrap_or(serde_json::Value::Null),
        );
        request_extra.insert("session_request_no".to_string(), session_request_no.into());
        request_extra.insert("bytes_b64".to_string(), BASE64.encode(bytes).into());
        request_extra.insert(
            "matched_rule".to_string(),
            matched_rule
                .clone()
                .map(Into::into)
                .unwrap_or(serde_json::Value::Null),
        );
        request_extra.insert(
            "danger_reason".to_string(),
            danger_reason
                .map(Into::into)
                .unwrap_or(serde_json::Value::Null),
        );
        if let Err(e) = recorder.append_event("write_request", request_extra) {
            eprintln!(
                "serialwrapd: gate: failed to append write_request audit record for pending \
                 {id}: {e}"
            );
        }

        // Fire-and-forget: a slow, hanging, or outright missing
        // notification backend must never delay (let alone block) the
        // approval flow itself — see `notify`'s module docs. `spawn_blocking`
        // additionally keeps a subprocess spawn off the async runtime's
        // worker threads.
        let notifier = Arc::clone(&self.notifier);
        let title = "serialwrap: write needs approval".to_string();
        let body = match &matched_rule {
            Some(rule) => format!("agent (pid {pid}) wants to write to {device} — matched {rule}"),
            None => format!("agent (pid {pid}) wants to write to {device}"),
        };
        tokio::task::spawn_blocking(move || notifier.notify(&title, &body));

        // Fail-safe timeout: after `self.rules.timeout`, auto-deny via the
        // exact same `decide` path a human's CLI call uses — see
        // `approval`'s module docs on why there is only one resolution
        // path, not two racing senders.
        let timeout = self.rules.timeout;
        let queue = Arc::clone(&self.queue);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let timeout_label = format!("timeout_{}s", timeout.as_secs());
            // `Err(DecideError)` here just means a human already decided
            // first — not a bug, the expected common case.
            let _ = queue.decide(
                id,
                Decision::Denied {
                    reason: timeout_label,
                },
            );
        });

        let decision = match matched_rule {
            Some(matched_rule) => GateDecision::ForcePending { id, matched_rule },
            None => GateDecision::Pending { id },
        };
        (decision, Some(rx))
    }

    /// Resolve a pending write by id — used by both
    /// `protocol::session`'s `ApprovalApprove`/`ApprovalDeny` handlers and,
    /// internally, [`Self::submit_write`]'s own timeout task.
    pub fn decide(&self, id: u64, decision: Decision) -> Result<(), DecideError> {
        self.queue.decide(id, decision)
    }

    /// Every currently pending write — `serialwrap approvals` and the
    /// future GUI approval list (T5.4) both call this via
    /// `Request::ApprovalsList`.
    pub fn list(&self) -> Vec<ApprovalSnapshot> {
        self.queue.list()
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Synthetic "bytes" [`Gate::submit_write`] matches a `dtr_pulse` request
/// against (`TASKS.md` T4.4, issue #17) — see `protocol::session`'s
/// `Request::DtrPulse` handler, the only caller. A `dtr_pulse` request is
/// not itself a byte payload sent to the device (it never reaches
/// `protocol::backend::DeviceBackend::write_bytes`; once allowed/approved,
/// the handler calls `DeviceBackend::dtr_pulse` directly, which appends its
/// own `dtr_pulse` event via `device_profile::append_dtr_pulse_event`) —
/// this string exists purely so the *same* whitelist/danger rule-matching
/// machinery `write` uses can also name `dtr_pulse` explicitly (e.g. an
/// operator's own `pattern = "dtr_pulse"` in `rules.toml`), per the
/// Security-model wiki's policy table ("Toggle DTR/RTS, dtr_pulse: Gated —
/// physically resets most boards") and this task's own reasoning for why
/// `dtr_pulse` is a distinct, named action rather than a `set_config`
/// parameter: "這樣規則引擎能單獨比對它，稽核讀起來是「reset 了板子」而不是
/// 「設了一條控制線」". With no rule matching it at all (the built-in
/// default, no `rules.toml`), this always falls through to
/// [`rules::RuleVerdict::Pending`] — approval required by default, exactly
/// the "Gated" policy — while still leaving an operator free to whitelist
/// it explicitly for a rig where an agent-triggered reset is routine.
pub fn dtr_pulse_gate_bytes(duration_ms: u64) -> Vec<u8> {
    format!("dtr_pulse duration_ms={duration_ms}").into_bytes()
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

    fn ctx(session_request_no: u64) -> RequesterCtx {
        RequesterCtx {
            device: "dev".to_string(),
            name: "claude-code".to_string(),
            pid: 4242,
            client_type: ClientType::Agent,
            session_request_no,
        }
    }

    #[tokio::test]
    async fn whitelisted_write_allows_immediately_with_no_receiver() {
        let (recorder, _dir) = test_recorder();
        let gate = Gate::new(
            rules_with(&["^status$"], &[]),
            Arc::new(notify::DesktopNotifier),
        );
        let (decision, rx) = gate.submit_write(&recorder, b"status", ctx(1), vec![]);
        assert_eq!(
            decision,
            GateDecision::Allow {
                reason: "whitelist:^status$".to_string()
            }
        );
        assert!(rx.is_none());
    }

    #[tokio::test]
    async fn danger_write_force_pends_and_can_be_approved() {
        let (recorder, _dir) = test_recorder();
        let gate = Gate::new(
            rules_with(&[], &["erase"]),
            Arc::new(notify::DesktopNotifier),
        );
        let (decision, rx) = gate.submit_write(&recorder, b"flash_erase", ctx(1), vec![]);
        let id = match decision {
            GateDecision::ForcePending { id, matched_rule } => {
                assert_eq!(matched_rule, "danger:erase");
                id
            }
            other => panic!("expected ForcePending, got {other:?}"),
        };
        gate.decide(
            id,
            Decision::Approved {
                approved_by: "sheldon:1000".to_string(),
            },
        )
        .expect("decide succeeds");
        match rx.expect("force_pending returns a receiver").await.unwrap() {
            Decision::Approved { approved_by } => assert_eq!(approved_by, "sheldon:1000"),
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    // ---- T4.3 acceptance criteria 1 & 2 (issue #16): full traceability, and
    // a denied request keeps its full payload ----

    #[tokio::test]
    async fn a_denied_write_keeps_full_traceability_via_the_write_request_and_gate_events() {
        let (recorder, _dir) = test_recorder();
        let gate = Gate::new(
            rules_with(&[], &["erase"]),
            Arc::new(notify::DesktopNotifier),
        );
        let (decision, rx) = gate.submit_write(&recorder, b"flash_erase all", ctx(3), vec![]);
        let id = match decision {
            GateDecision::ForcePending { id, matched_rule } => {
                assert_eq!(matched_rule, "danger:erase");
                id
            }
            other => panic!("expected ForcePending, got {other:?}"),
        };
        gate.decide(
            id,
            Decision::Denied {
                reason: "denied_by_operator:sheldon:1000".to_string(),
            },
        )
        .expect("decide succeeds");
        rx.expect("force_pending returns a receiver").await.unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;

        // The full payload + requester identity + judgment path survive as
        // a `write_request` event — this is the one place a *denied*
        // write's bytes are recoverable from at all (no `tx` record is
        // ever produced for it).
        let write_request = records
            .iter()
            .find_map(|r| match r {
                wrap_proto::Record::Event { event, extra, .. } if event == "write_request" => {
                    Some(extra.clone())
                }
                _ => None,
            })
            .expect("expected a write_request event");
        assert_eq!(
            write_request.get("request_id").and_then(|v| v.as_u64()),
            Some(id)
        );
        assert_eq!(
            write_request.get("device").and_then(|v| v.as_str()),
            Some("dev")
        );
        assert_eq!(
            write_request.get("requester_name").and_then(|v| v.as_str()),
            Some("claude-code")
        );
        assert_eq!(
            write_request.get("requester_pid").and_then(|v| v.as_u64()),
            Some(4242)
        );
        assert_eq!(
            write_request.get("requester_type").and_then(|v| v.as_str()),
            Some("agent")
        );
        assert_eq!(
            write_request
                .get("session_request_no")
                .and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(
            write_request.get("matched_rule").and_then(|v| v.as_str()),
            Some("danger:erase")
        );
        let bytes_b64 = write_request
            .get("bytes_b64")
            .and_then(|v| v.as_str())
            .expect("bytes_b64 present");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(bytes_b64)
            .unwrap();
        assert_eq!(
            bytes, b"flash_erase all",
            "the full, unmangled payload the agent tried to send must survive the denial"
        );

        // The decision-maker + decision path: the `gate` `request`/`deny`
        // records correlate back to the same `write_request` via
        // `request_seq`/`request_id`.
        let gate_records: Vec<&wrap_proto::Record> = records
            .iter()
            .filter(|r| matches!(r, wrap_proto::Record::Gate { .. }))
            .collect();
        let request_record = gate_records
            .iter()
            .find(|r| matches!(r, wrap_proto::Record::Gate { action, request_seq, .. } if action == "request" && *request_seq == id))
            .expect("expected the request gate record");
        let deny_record = gate_records
            .iter()
            .find(|r| matches!(r, wrap_proto::Record::Gate { action, request_seq, .. } if action == "deny" && *request_seq == id))
            .expect("expected the deny gate record");
        match (request_record, deny_record) {
            (
                wrap_proto::Record::Gate {
                    reason: req_reason, ..
                },
                wrap_proto::Record::Gate {
                    reason: deny_reason,
                    seq: deny_seq,
                    ..
                },
            ) => {
                assert_eq!(req_reason, "danger:erase", "judgment path recoverable");
                assert_eq!(
                    deny_reason, "denied_by_operator:sheldon:1000",
                    "decision-maker recoverable from the deny record's own reason"
                );
                // The record's own `seq` *is* the corresponding log offset
                // — no separate index/join needed, per the "audit is a
                // view over the one event stream" design.
                assert!(
                    *deny_seq < recorder.read_since(0, usize::MAX).unwrap().records.len() as u64
                );
            }
            _ => unreachable!("filtered to Gate records above"),
        }
    }

    #[tokio::test]
    async fn dtr_pulse_gate_bytes_default_pends_with_no_rules_configured() {
        // Default (built-in) rules never mention "dtr_pulse" at all, so a
        // dtr_pulse request must fall through to default-pending — approval
        // required by default, per the Security-model wiki's "Gated" policy
        // for dtr_pulse (T4.4, issue #17).
        let set = RuleSet::builtin();
        let bytes = dtr_pulse_gate_bytes(50);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "dtr_pulse duration_ms=50"
        );
        assert_eq!(set.evaluate(&bytes), RuleVerdict::Pending);
    }

    fn rules_with(whitelist: &[&str], danger: &[&str]) -> RuleSet {
        let mut toml_text = String::from("[approval]\ntimeout_s = 60\n");
        for pattern in whitelist {
            toml_text.push_str(&format!("[[whitelist]]\npattern = {pattern:?}\n"));
        }
        for pattern in danger {
            toml_text.push_str(&format!(
                "[[danger]]\npattern = {pattern:?}\nreason = \"test\"\n"
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, toml_text).unwrap();
        RuleSet::load(&path).expect("test rules.toml is valid")
    }
}
