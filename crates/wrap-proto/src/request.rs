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
    /// Interface only at this stage (`TASKS.md` T4.1 is the real write
    /// gate): the daemon accepts this shape and returns a structured
    /// `permission_denied` for every request, so the wire contract is fixed
    /// before the rule engine lands.
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
}
