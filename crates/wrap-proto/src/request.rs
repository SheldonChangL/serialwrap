use serde::{Deserialize, Serialize};

use crate::hello::Permission;

/// Every request after the handshake (`TASKS.md` T1.4). Carries no `id`
/// field itself — the wiki says "requests carry an `id`; responses echo
/// it", but `id` is deliberately kept out of this enum's own shape so a
/// caller can look it up generically (any request line is `{"id": ...,
/// "op": ..., ...}`; a plain `serde_json::Value` lookup for `"id"` before
/// deserializing the rest into `Request` avoids needing every one of the
/// 15 variants below to repeat an identical `id: u64` field, and — more
/// importantly — means a request that fails to deserialize into `Request`
/// (`invalid_request`) can still have its `id` recovered and echoed in the
/// error reply, exactly as the wiki's error table expects). See
/// `serialwrapd::protocol::session` for where that split is applied.
///
/// One request enum, shared verbatim by the daemon, the CLI (T1.5), and the
/// MCP bridge (T3.1) — see the crate's module docs for why `wrap-proto`
/// exists at all. Response *bodies*, by contrast, are **not** modelled here:
/// several of them (`get_config`, `set_config`) need `serialwrapd`-only
/// types (`PortConfig`, `ErrorCounts` — see `port_config.rs`/
/// `error_counts.rs`), which this crate must never depend on (dependency
/// direction is `serialwrap -> serialwrapd -> wrap-proto`). Request fields
/// therefore stay to primitives/generic JSON (e.g. `set_config`'s config
/// fields are a flattened JSON object, merged against the current
/// `PortConfig` daemon-side) precisely so every variant here is
/// constructible without reaching into a downstream crate's types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    ListDevices,
    GetConfig {
        device: String,
    },
    /// `config` carries whatever subset of `PortConfig`'s fields
    /// (`baud`/`data_bits`/`parity`/`stop_bits`/`flow_control`/
    /// `open_control_lines`) the client wants to change — "any config
    /// field" per the wiki's request-set table, i.e. a partial patch
    /// merged onto the device's current configuration, not a full replace.
    SetConfig {
        device: String,
        #[serde(flatten)]
        config: serde_json::Map<String, serde_json::Value>,
    },
    SetControlLine {
        device: String,
        #[serde(default)]
        dtr: Option<bool>,
        #[serde(default)]
        rts: Option<bool>,
    },
    DtrPulse {
        device: String,
        duration_ms: u64,
    },
    Tail {
        device: String,
        n: usize,
        #[serde(default)]
        filter: Option<Filter>,
    },
    ReadSince {
        device: String,
        cursor: u64,
        #[serde(default)]
        max_bytes: Option<usize>,
        #[serde(default)]
        filter: Option<Filter>,
    },
    /// Matches only fully assembled lines — see `serialwrapd::query`'s
    /// module docs for why a half-line can never satisfy this.
    WaitFor {
        device: String,
        pattern: String,
        timeout_s: f64,
    },
    /// `since_cursor`, when given, has exactly `read_since`'s `cursor`
    /// semantics ("push everything with `seq >= since_cursor`, then keep
    /// pushing"): a client that already called `tail`/`read_since` and got
    /// back a cursor can pass it straight here and never miss (or
    /// re-receive) a record in between, closing the "tail history, then
    /// subscribe" gap that existed when `subscribe` only ever started from
    /// whatever was current at dispatch time (see the [Client protocol
    /// wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)).
    /// Omitted (or `null`), it falls back to that old start-from-now
    /// behavior. A `since_cursor` older than this device's retained window
    /// fails the same way `read_since` does: a structured `data_aged_out`
    /// error, never a silent skip to "now".
    Subscribe {
        device: String,
        #[serde(default)]
        filter: Option<Filter>,
        #[serde(default)]
        since_cursor: Option<u64>,
    },
    QueryEvents {
        device: String,
        #[serde(default)]
        kinds: Vec<String>,
        #[serde(default)]
        since_seq: Option<u64>,
        #[serde(default)]
        until_seq: Option<u64>,
    },
    /// `TASKS.md` T4.1/T4.2 (issues #14/#15): what happens to the decoded
    /// bytes depends on the connection's [`Permission`] — `human`
    /// (`ReadWrite`) passes straight through, always audited; `agent`
    /// (`ReadGatedWrite`) goes through `serialwrapd::gate`'s rule engine
    /// (allow / pending / force-pending, the last two blocking this
    /// request's reply until a decision or timeout); `tool` (`LeaseOnly`)
    /// still gets a structured `permission_denied` — it has no byte-level
    /// write path at all, only `LeaseAcquire`.
    Write {
        device: String,
        #[serde(default)]
        data_b64: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        line_ending: LineEnding,
    },
    /// Interface only (`TASKS.md` T2.2 implements the actual fd handoff).
    LeaseAcquire {
        device: String,
        command: String,
        #[serde(default)]
        timeout_s: Option<f64>,
    },
    /// Interface only (`TASKS.md` T2.2).
    LeaseRelease {
        token: String,
        exit_code: i32,
    },
    ListClients,
    Kick {
        client_id: u64,
    },
    Demote {
        client_id: u64,
        permission: Permission,
    },
    /// Render a device's recorded stream as a portable artifact (`TASKS.md`
    /// T2.4, issue #11). See the [Event stream and storage
    /// wiki](https://github.com/SheldonChangL/serialwrap/wiki/Event-stream-and-storage)'s
    /// "Export formats" section for what each [`ExportFormat`] guarantees.
    /// This is the one API both the CLI and the future GUI export (T5.5)
    /// call — see `serialwrapd::export`'s module docs for why the
    /// range-resolution/formatting logic lives daemon-side rather than in
    /// the CLI.
    ///
    /// `from`/`to` bound the range (omitted = open on that end: from the
    /// oldest retained record / up to the current tip). `filter` narrows
    /// `rx` content for `jsonl`/`txt` only — `format: Bin` combined with a
    /// `filter` is a structured `invalid_request` error, never a silent
    /// ignore (the wiki: "bin 不允許過濾，保證完整性").
    Export {
        device: String,
        format: ExportFormat,
        #[serde(default)]
        from: Option<ExportBound>,
        #[serde(default)]
        to: Option<ExportBound>,
        #[serde(default)]
        filter: Option<Filter>,
    },
    /// List every write currently sitting in the gate's pending-approval
    /// queue (`TASKS.md` T4.2, issue #15). The one op `serialwrap approvals`
    /// and the future GUI approval card (T5.4) both call — see
    /// `serialwrapd::gate`'s module docs for why list/approve/deny are kept
    /// as this same small API both consume.
    ApprovalsList,
    /// Approve a pending write by its gate-assigned `approval_id` (from
    /// `ApprovalsList`'s reply — never a recorder `seq` or a `client_id`).
    ///
    /// Deliberately named `approval_id`, not `id`: every request already
    /// carries a top-level `id` used purely for reply correlation (see
    /// `Request`'s own module docs — it's kept out of this enum's shape
    /// generically, but stays present as a sibling field on the same flat
    /// wire object `serde` deserializes this enum from). A field on
    /// *this* variant also named `id` would collide with that — same JSON
    /// key, two different meanings — silently forcing a caller to always
    /// pick its own request-tracking id equal to the approval id it's
    /// acting on. `approval_id` keeps the two namespaces independent: a
    /// GUI (T5.4) can track this request under whatever `id` its own
    /// bookkeeping wants while still naming any pending approval it likes.
    ///
    /// The approving identity is always the daemon's own kernel-verified
    /// `name:pid` for this connection, never a client-supplied field — same
    /// convention `changed_by` uses everywhere else.
    ApprovalApprove {
        approval_id: u64,
    },
    /// Deny a pending write by its gate-assigned `approval_id` (see
    /// `ApprovalApprove`'s doc comment for why not `id`), with an optional
    /// human-readable reason (a generic operator-denied label is used if
    /// omitted — see `serialwrapd::gate::Decision::Denied`). Also how a
    /// timed-out request resolves internally, with reason `"timeout_60s"`
    /// (or whatever `rules.toml`'s `[approval] timeout_s` is configured
    /// to) — never silently, per the Security-model wiki's "denial is
    /// never silent".
    ApprovalDeny {
        approval_id: u64,
        #[serde(default)]
        reason: Option<String>,
    },
}

/// Output format for [`Request::Export`]. See the wiki's Event stream and
/// storage page, "Export formats" section, for the guarantee each one
/// makes (`jsonl` lossless/round-trippable, `txt` human-readable, `bin`
/// byte-exact `rx`-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Jsonl,
    Txt,
    Bin,
}

/// One end of an [`Request::Export`] range: an exact sequence number, or an
/// RFC 3339 wall-clock timestamp string. Untagged so the wire value is just
/// a bare integer or a bare string — matching the wiki's own phrasing for
/// `--from`/`--to`, "wall time or seq" — and parsed (including the wall
/// timestamp string) entirely daemon-side in `serialwrapd::export`: this
/// crate stays shape-only and must not depend on `chrono` (see the crate's
/// module docs on the dependency direction this enforces).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExportBound {
    Seq(u64),
    Wall(String),
}

/// Narrows which assembled lines a `tail`/`read_since`/`subscribe` call
/// returns. Never applies to out-of-band events — see the wiki: "Filters
/// narrow which log lines are interesting; they never suppress the fact
/// that the stream was interrupted." Enforcing that is
/// `serialwrapd::query`'s job; this crate only carries the shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// Regex applied against each assembled line's text.
    pub pattern: String,
    /// `false` (default): keep only lines the pattern matches. `true`:
    /// keep only lines it does *not* match.
    #[serde(default)]
    pub exclude: bool,
}

/// How to terminate bytes sent by a `write` request. An explicit parameter
/// rather than a client-side convention — see the wiki: sending the wrong
/// line ending to a firmware CLI is "a classic source of 'the board ignored
/// my command'".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
    Cr,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_devices_has_no_extra_fields() {
        let json = r#"{"op":"list_devices"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req, Request::ListDevices);
    }

    #[test]
    fn wait_for_round_trips_per_wiki_params() {
        let json = r#"{"op":"wait_for","device":"usb-1a86","pattern":"boot ok","timeout_s":5.0}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::WaitFor {
                device: "usb-1a86".to_string(),
                pattern: "boot ok".to_string(),
                timeout_s: 5.0,
            }
        );
    }

    #[test]
    fn set_config_flattens_arbitrary_config_fields() {
        let json = r#"{"op":"set_config","device":"dev","baud":74880}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::SetConfig { device, config } => {
                assert_eq!(device, "dev");
                assert_eq!(config.get("baud").and_then(|v| v.as_u64()), Some(74880));
            }
            other => panic!("expected SetConfig, got {other:?}"),
        }
    }

    #[test]
    fn line_ending_default_is_lf() {
        assert_eq!(LineEnding::default(), LineEnding::Lf);
        assert_eq!(
            serde_json::to_string(&LineEnding::None).unwrap(),
            "\"none\""
        );
    }

    #[test]
    fn unknown_op_fails_to_deserialize_as_request() {
        let json = r#"{"op":"not_a_real_op"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn filter_defaults_exclude_to_false() {
        let json = r#"{"pattern":"ERROR"}"#;
        let filter: Filter = serde_json::from_str(json).unwrap();
        assert!(!filter.exclude);
    }

    #[test]
    fn subscribe_since_cursor_defaults_to_none_and_round_trips_when_given() {
        let json = r#"{"op":"subscribe","device":"dev"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::Subscribe {
                device: "dev".to_string(),
                filter: None,
                since_cursor: None,
            }
        );

        let json = r#"{"op":"subscribe","device":"dev","since_cursor":42}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::Subscribe {
                device: "dev".to_string(),
                filter: None,
                since_cursor: Some(42),
            }
        );
    }

    #[test]
    fn export_round_trips_with_seq_bounds_and_defaults_optional_fields() {
        let json = r#"{"op":"export","device":"dev","format":"jsonl","from":10,"to":20}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::Export {
                device: "dev".to_string(),
                format: ExportFormat::Jsonl,
                from: Some(ExportBound::Seq(10)),
                to: Some(ExportBound::Seq(20)),
                filter: None,
            }
        );
    }

    #[test]
    fn export_bound_untagged_wire_shape_distinguishes_seq_from_wall() {
        assert_eq!(serde_json::to_string(&ExportBound::Seq(5)).unwrap(), "5");
        assert_eq!(
            serde_json::to_string(&ExportBound::Wall("2026-07-27T10:00:00+08:00".to_string()))
                .unwrap(),
            "\"2026-07-27T10:00:00+08:00\""
        );
        assert_eq!(
            serde_json::from_str::<ExportBound>("5").unwrap(),
            ExportBound::Seq(5)
        );
        assert_eq!(
            serde_json::from_str::<ExportBound>("\"2026-07-27T10:00:00+08:00\"").unwrap(),
            ExportBound::Wall("2026-07-27T10:00:00+08:00".to_string())
        );
    }

    #[test]
    fn export_format_serializes_to_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&ExportFormat::Jsonl).unwrap(),
            "\"jsonl\""
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::Txt).unwrap(),
            "\"txt\""
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::Bin).unwrap(),
            "\"bin\""
        );
    }

    #[test]
    fn approvals_list_has_no_extra_fields() {
        let json = r#"{"op":"approvals_list"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req, Request::ApprovalsList);
    }

    #[test]
    fn approval_approve_round_trips() {
        let json = r#"{"op":"approval_approve","approval_id":42}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req, Request::ApprovalApprove { approval_id: 42 });
    }

    #[test]
    fn approval_approve_field_is_named_approval_id_not_id() {
        // The whole reason it's not called `id`: a full request line also
        // carries a top-level `id` for reply correlation (see
        // `protocol::session::handle_request`), alongside `op` and this
        // variant's own fields, all flattened into one JSON object. If
        // this field were also named `id`, that single JSON key would have
        // to serve both purposes at once.
        let json = r#"{"id":5,"op":"approval_approve","approval_id":42}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req, Request::ApprovalApprove { approval_id: 42 });
    }

    #[test]
    fn approval_deny_defaults_reason_to_none_and_round_trips_when_given() {
        let json = r#"{"op":"approval_deny","approval_id":7}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::ApprovalDeny {
                approval_id: 7,
                reason: None
            }
        );

        let json = r#"{"op":"approval_deny","approval_id":7,"reason":"not right now"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            req,
            Request::ApprovalDeny {
                approval_id: 7,
                reason: Some("not right now".to_string())
            }
        );
    }

    #[test]
    fn export_bin_with_a_filter_still_deserializes_daemon_rejects_it_not_serde() {
        // The wire shape itself must accept `format: bin` + `filter`
        // together — rejection is a deliberate, structured daemon-side
        // decision (`serialwrapd::export::export_range`), not something
        // baked into the request's own deserialization.
        let json = r#"{"op":"export","device":"dev","format":"bin","filter":{"pattern":"x"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Export { format, filter, .. } => {
                assert_eq!(format, ExportFormat::Bin);
                assert!(filter.is_some());
            }
            other => panic!("expected Export, got {other:?}"),
        }
    }
}
