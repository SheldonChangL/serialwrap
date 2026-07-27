use serde::{Deserialize, Serialize};

use crate::client::ClientType;

/// Discriminant for [`Record`] variants; mirrors the `kind` field of the
/// on-disk JSONL schema described in `TASKS.md` ("0. 範圍與技術基線").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Rx,
    Tx,
    Event,
    Gate,
}

/// One line of the append-only JSONL event stream.
///
/// This is a skeleton shape only: field sets will grow as the recorder
/// (T1.2), write gate (T4.1), and query layer (T1.4) get implemented. `seq`
/// is monotonically increasing across the *whole* stream regardless of
/// variant, and doubles as the cursor used by `read_since`/`tail`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// Bytes read from the device, base64-encoded, not yet line-assembled.
    Rx {
        seq: u64,
        t_mono: f64,
        t_wall: String,
        data_b64: String,
    },
    /// Bytes written to the device by a client, subject to the write gate.
    Tx {
        seq: u64,
        client: String,
        client_type: ClientType,
        gate: String,
        data_b64: String,
    },
    /// Out-of-band occurrence: device attach/detach, lease start/end, config
    /// change, recovery, etc. `event` names the specific occurrence;
    /// unrecognized fields are preserved via `extra` for forward
    /// compatibility (this schema will keep growing new event shapes).
    Event {
        seq: u64,
        event: String,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// Write-gate decision (allow/deny/pending) on a requested write.
    Gate {
        seq: u64,
        action: String,
        reason: String,
        request_seq: u64,
    },
}

impl Record {
    /// The monotonically increasing sequence number, common to every variant.
    pub fn seq(&self) -> u64 {
        match self {
            Record::Rx { seq, .. }
            | Record::Tx { seq, .. }
            | Record::Event { seq, .. }
            | Record::Gate { seq, .. } => *seq,
        }
    }

    /// The [`Kind`] discriminant for this record.
    pub fn kind(&self) -> Kind {
        match self {
            Record::Rx { .. } => Kind::Rx,
            Record::Tx { .. } => Kind::Tx,
            Record::Event { .. } => Kind::Event,
            Record::Gate { .. } => Kind::Gate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_record_round_trips_through_json() {
        let record = Record::Rx {
            seq: 812_044,
            t_mono: 123_456.789_012,
            t_wall: "2026-07-27T10:34:12.443+08:00".to_string(),
            data_b64: String::new(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: Record = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert_eq!(back.seq(), 812_044);
        assert_eq!(back.kind(), Kind::Rx);
    }

    #[test]
    fn event_record_preserves_extra_fields() {
        // Exact example from TASKS.md's schema.
        let json =
            r#"{"seq":812045,"kind":"event","event":"lease_start","client":"esptool","pid":5311}"#;
        let record: Record = serde_json::from_str(json).unwrap();
        match &record {
            Record::Event { event, extra, .. } => {
                assert_eq!(event, "lease_start");
                assert_eq!(
                    extra.get("client").and_then(|v| v.as_str()),
                    Some("esptool")
                );
                assert_eq!(extra.get("pid").and_then(|v| v.as_i64()), Some(5311));
            }
            other => panic!("expected Event variant, got {other:?}"),
        }
        assert_eq!(record.kind(), Kind::Event);
    }

    #[test]
    fn tx_record_round_trips_through_json() {
        let record = Record::Tx {
            seq: 812_046,
            client: "claude-code".to_string(),
            client_type: ClientType::Agent,
            gate: "whitelist".to_string(),
            data_b64: "c3RhdHVzCg==".to_string(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: Record = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert_eq!(back.kind(), Kind::Tx);
    }
}
