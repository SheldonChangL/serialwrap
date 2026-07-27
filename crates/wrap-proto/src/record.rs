use serde::{Deserialize, Serialize};

use crate::client::ClientType;

/// Discriminant for [`Record`] variants; mirrors the `kind` field of the
/// on-disk JSONL schema (see the [Event stream and storage wiki
/// page](https://github.com/SheldonChangL/serialwrap/wiki/Event-stream-and-storage)).
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
/// Every record carries `seq`, `t_mono`, `t_wall`, and `kind` regardless of
/// variant — per the wiki: "Every record carries a sequence number and
/// both clocks." `seq` is the monotonic, gap-free cursor used by
/// `read_since`/`tail`; `t_mono` is for interval math (immune to
/// wall-clock adjustment); `t_wall` (RFC 3339) is for display and range
/// selection.
///
/// This is still a skeleton shape: the wiki documents additional
/// variant-specific fields not modeled yet (e.g. `tx`'s
/// `client_pid`/`approved_by`, `gate`'s `matched_rule`, the `gate`/`action`
/// enums' full value sets). Those land with the recorder (T1.2) and write
/// gate (T4.1).
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
        t_mono: f64,
        t_wall: String,
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
        t_mono: f64,
        t_wall: String,
        event: String,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// Write-gate decision (allow/deny/pending) on a requested write.
    Gate {
        seq: u64,
        t_mono: f64,
        t_wall: String,
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

    /// Monotonic-clock timestamp (seconds), common to every variant.
    pub fn t_mono(&self) -> f64 {
        match self {
            Record::Rx { t_mono, .. }
            | Record::Tx { t_mono, .. }
            | Record::Event { t_mono, .. }
            | Record::Gate { t_mono, .. } => *t_mono,
        }
    }

    /// Wall-clock timestamp (RFC 3339), common to every variant.
    pub fn t_wall(&self) -> &str {
        match self {
            Record::Rx { t_wall, .. }
            | Record::Tx { t_wall, .. }
            | Record::Event { t_wall, .. }
            | Record::Gate { t_wall, .. } => t_wall,
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
    use serde_json::json;

    #[test]
    fn rx_record_matches_wiki_schema_example() {
        // Verbatim shape from the wiki's rx example, field-for-field.
        let record = Record::Rx {
            seq: 812_044,
            t_mono: 123_456.789_012,
            t_wall: "2026-07-27T10:34:12.443+08:00".to_string(),
            data_b64: String::new(),
        };
        let actual = serde_json::to_value(&record).unwrap();
        let expected = json!({
            "seq": 812044,
            "t_mono": 123456.789012,
            "t_wall": "2026-07-27T10:34:12.443+08:00",
            "kind": "rx",
            "data_b64": ""
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn tx_record_matches_wiki_schema_shape() {
        // Verbatim shape from the wiki's tx example (client_pid/approved_by
        // are documented but not modeled yet at this skeleton stage; only
        // the fields this crate actually defines are asserted here).
        let record = Record::Tx {
            seq: 812_046,
            t_mono: 123_456.999,
            t_wall: "2026-07-27T10:34:13.000+08:00".to_string(),
            client: "claude-code".to_string(),
            client_type: ClientType::Agent,
            gate: "whitelist".to_string(),
            data_b64: "c3RhdHVzCg==".to_string(),
        };
        let actual = serde_json::to_value(&record).unwrap();
        let expected = json!({
            "seq": 812046,
            "t_mono": 123456.999,
            "t_wall": "2026-07-27T10:34:13.000+08:00",
            "kind": "tx",
            "client": "claude-code",
            "client_type": "agent",
            "gate": "whitelist",
            "data_b64": "c3RhdHVzCg=="
        });
        assert_eq!(actual, expected);
    }

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
        assert_eq!(back.t_mono(), 123_456.789_012);
        assert_eq!(back.t_wall(), "2026-07-27T10:34:12.443+08:00");
        assert_eq!(back.kind(), Kind::Rx);
    }

    #[test]
    fn event_record_preserves_extra_fields() {
        // TASKS.md's schema example, with the timestamp fields the wiki
        // requires on every record (TASKS.md's example line omitted them).
        let json = r#"{"seq":812045,"t_mono":123456.9,"t_wall":"2026-07-27T10:34:12.500+08:00","kind":"event","event":"lease_start","client":"esptool","pid":5311}"#;
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
        assert_eq!(record.t_mono(), 123_456.9);
    }

    #[test]
    fn tx_record_round_trips_through_json() {
        let record = Record::Tx {
            seq: 812_046,
            t_mono: 123_457.0,
            t_wall: "2026-07-27T10:34:13.000+08:00".to_string(),
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

    #[test]
    fn gate_record_round_trips_through_json() {
        let record = Record::Gate {
            seq: 812_047,
            t_mono: 123_457.5,
            t_wall: "2026-07-27T10:34:13.500+08:00".to_string(),
            action: "deny".to_string(),
            reason: "timeout_60s".to_string(),
            request_seq: 812_040,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: Record = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert_eq!(back.kind(), Kind::Gate);
        assert_eq!(back.t_mono(), 123_457.5);
    }
}
