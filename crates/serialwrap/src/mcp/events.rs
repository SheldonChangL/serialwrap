//! Per-device out-of-band event watermarks.
//!
//! Every one of this bridge's five read tools must carry any out-of-band
//! events (disconnect, lease activity, config change) that happened on the
//! relevant device since the agent last looked — not just `tail`/
//! `read_since`, whose own daemon replies already carry an `events` array
//! for free (see `serialwrapd::query`'s module docs: events are never
//! dropped by a filter, only ever range-bounded). `get_config`, `wait_for`,
//! and `list_devices` have no such field in their own daemon reply, so this
//! bridge fetches it separately via `Request::QueryEvents` — see
//! `tools.rs`'s `fetch_new_events`.
//!
//! [`EventWatermarks`] is what makes that fetch return only *new* events
//! rather than the device's entire history every time: one high-water mark
//! per device, advanced past the highest `seq` this bridge has already
//! handed back (from *any* tool, not just `QueryEvents` — `tail`/
//! `read_since`'s own embedded events advance it too), so the very next
//! read tool call after a disconnect is guaranteed to include that
//! disconnect event exactly once, per the "斷線發生時，下一次任何讀取工具
//! 的結果都含 disconnect 事件" acceptance criterion.
//!
//! Scoped to this bridge process's lifetime only (in-memory, not persisted)
//! — a fresh `serialwrap mcp` session starts every device's watermark at 0,
//! meaning its first call for a device sees that device's entire recorded
//! event history (including e.g. its initial `connect`). That is
//! deliberate: within one session, "new to the agent" and "new since last
//! checked" coincide.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// Reconstruct a [`serialwrapd::query::OobRecord`] from one daemon wire
/// event object (`TASKS.md` T3.2, issue #13) — the mirror image of
/// `crate::mcp::line::assembled_line_from_wire`, needed for the same
/// reason: this bridge calls `serialwrapd::presentation::present` directly
/// (reusing the daemon crate's own logic rather than reimplementing it),
/// which takes real `OobRecord`s, not wire JSON. Every field this produces
/// round-trips back through `serialwrapd::presentation::event_to_json` to
/// the identical wire shape the daemon itself sent (same field names as
/// `serialwrapd::protocol::session`'s private `oob_json`).
pub fn oob_from_wire(v: &Value) -> serialwrapd::query::OobRecord {
    use serialwrapd::query::OobRecord;
    use wrap_proto::Kind;

    let kind = match v.get("kind").and_then(Value::as_str) {
        Some("rx") => Kind::Rx,
        Some("tx") => Kind::Tx,
        Some("gate") => Kind::Gate,
        _ => Kind::Event,
    };
    let name = v
        .get("event")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let mut extra = serde_json::Map::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !matches!(k.as_str(), "seq" | "t_mono" | "t_wall" | "kind" | "event") {
                extra.insert(k.clone(), val.clone());
            }
        }
    }
    OobRecord {
        seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
        t_mono: v.get("t_mono").and_then(Value::as_f64).unwrap_or(0.0),
        t_wall: v
            .get("t_wall")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        kind,
        name,
        extra,
    }
}

#[derive(Default)]
pub struct EventWatermarks {
    /// device id -> lowest event `seq` not yet delivered.
    next_seq: Mutex<HashMap<String, u64>>,
}

impl EventWatermarks {
    /// The `since_seq` to pass to `Request::QueryEvents` for `device` right
    /// now — everything at or after this point is "new".
    pub fn since_seq(&self, device: &str) -> u64 {
        *self
            .next_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device)
            .unwrap_or(&0)
    }

    /// Advance `device`'s watermark past the highest `seq` among `events`
    /// (each expected to have a `seq` field, matching `oob_json`'s wire
    /// shape). A no-op if `events` is empty or none of them are newer than
    /// the current watermark — advancing can only ever move forward, never
    /// back, so calling this with a stale/overlapping batch is always safe.
    pub fn advance(&self, device: &str, events: &[Value]) {
        let Some(max_seq) = events
            .iter()
            .filter_map(|e| e.get("seq").and_then(Value::as_u64))
            .max()
        else {
            return;
        };
        let mut map = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(device.to_string()).or_insert(0);
        if max_seq + 1 > *entry {
            *entry = max_seq + 1;
        }
    }

    /// Filter `events` (a full or over-wide batch — e.g. `tail`'s daemon
    /// reply, which always carries a device's *entire* out-of-band event
    /// history, not just what's new — see `query::DeviceQueryState::tail`'s
    /// docs) down to only the ones at/after `device`'s current watermark,
    /// then advance the watermark past whatever was returned.
    ///
    /// Read-filter-advance happens as one critical section under this
    /// struct's own lock (no `.await` anywhere in between — the caller
    /// already has `events` in hand), which is what makes "each event
    /// handed back to a tool call exactly once" hold even under two
    /// concurrent calls for the same device: unlike
    /// [`Self::since_seq`]/[`Self::advance`] called as two separate steps
    /// around an `.await` (see `tools.rs`'s `fetch_new_events`, which needs
    /// its own separate serialization for exactly this reason), there is no
    /// window here for another call to observe the same pre-advance
    /// watermark.
    pub fn take_new(&self, device: &str, events: &[Value]) -> Vec<Value> {
        let mut map = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let since = *map.get(device).unwrap_or(&0);
        let new_events: Vec<Value> = events
            .iter()
            .filter(|e| {
                e.get("seq")
                    .and_then(Value::as_u64)
                    .is_none_or(|seq| seq >= since)
            })
            .cloned()
            .collect();
        if let Some(max_seq) = new_events
            .iter()
            .filter_map(|e| e.get("seq").and_then(Value::as_u64))
            .max()
        {
            let entry = map.entry(device.to_string()).or_insert(0);
            if max_seq + 1 > *entry {
                *entry = max_seq + 1;
            }
        }
        new_events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn oob_from_wire_round_trips_an_event_record() {
        let wire = json!({
            "seq": 3, "t_mono": 1.5, "t_wall": "t3", "kind": "event",
            "event": "disconnect", "device_id": "usb-1",
        });
        let record = oob_from_wire(&wire);
        assert_eq!(record.seq, 3);
        assert_eq!(record.kind, wrap_proto::Kind::Event);
        assert_eq!(record.name.as_deref(), Some("disconnect"));
        assert_eq!(
            record.extra.get("device_id").and_then(Value::as_str),
            Some("usb-1")
        );
        // Round-trips back to the identical wire shape.
        assert_eq!(serialwrapd::presentation::event_to_json(&record), wire);
    }

    #[test]
    fn oob_from_wire_round_trips_a_gate_record() {
        let wire = json!({
            "seq": 9, "t_mono": 2.0, "t_wall": "t9", "kind": "gate",
            "action": "deny", "reason": "timeout_60s", "request_seq": 1,
        });
        let record = oob_from_wire(&wire);
        assert_eq!(record.kind, wrap_proto::Kind::Gate);
        assert!(record.name.is_none());
        assert_eq!(
            record.extra.get("action").and_then(Value::as_str),
            Some("deny")
        );
    }

    #[test]
    fn fresh_device_starts_at_watermark_zero() {
        let w = EventWatermarks::default();
        assert_eq!(w.since_seq("dev"), 0);
    }

    #[test]
    fn advance_moves_the_watermark_past_the_highest_seen_seq() {
        let w = EventWatermarks::default();
        w.advance(
            "dev",
            &[json!({"seq": 3}), json!({"seq": 7}), json!({"seq": 5})],
        );
        assert_eq!(w.since_seq("dev"), 8);
    }

    #[test]
    fn advance_never_moves_the_watermark_backward() {
        let w = EventWatermarks::default();
        w.advance("dev", &[json!({"seq": 10})]);
        assert_eq!(w.since_seq("dev"), 11);
        w.advance("dev", &[json!({"seq": 2})]);
        assert_eq!(
            w.since_seq("dev"),
            11,
            "an older/overlapping batch must not roll the watermark back"
        );
    }

    #[test]
    fn advance_with_no_events_is_a_no_op() {
        let w = EventWatermarks::default();
        w.advance("dev", &[]);
        assert_eq!(w.since_seq("dev"), 0);
    }

    #[test]
    fn watermarks_are_tracked_independently_per_device() {
        let w = EventWatermarks::default();
        w.advance("dev-a", &[json!({"seq": 100})]);
        assert_eq!(w.since_seq("dev-a"), 101);
        assert_eq!(w.since_seq("dev-b"), 0);
    }

    #[test]
    fn take_new_returns_the_full_batch_on_a_fresh_device_then_nothing_on_repeat() {
        let w = EventWatermarks::default();
        let full_history = vec![
            json!({"seq": 0, "event": "connect"}),
            json!({"seq": 3, "event": "disconnect"}),
        ];

        let first = w.take_new("dev", &full_history);
        assert_eq!(first, full_history);

        // The same (unbounded, always-full-history) batch handed to a
        // second call must not repeat anything already delivered -- this
        // is exactly what protects `tail` (whose daemon reply always
        // carries the device's entire event history, not just what's new)
        // from re-delivering the same disconnect on every subsequent call.
        let second = w.take_new("dev", &full_history);
        assert!(second.is_empty(), "repeat delivery: {second:?}");
    }

    #[test]
    fn take_new_returns_only_the_incremental_tail_of_a_growing_batch() {
        let w = EventWatermarks::default();
        let first_batch = vec![json!({"seq": 0}), json!({"seq": 1})];
        assert_eq!(w.take_new("dev", &first_batch), first_batch);

        let grown_batch = vec![json!({"seq": 0}), json!({"seq": 1}), json!({"seq": 2})];
        let incremental = w.take_new("dev", &grown_batch);
        assert_eq!(incremental, vec![json!({"seq": 2})]);
    }
}
