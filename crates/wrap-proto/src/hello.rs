use serde::{Deserialize, Serialize};

use crate::client::ClientType;

/// The first message on every UDS connection (`TASKS.md` T1.4, the
/// [Client protocol wiki
/// page](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)'s
/// "Handshake and identity" section). Carries no `id`: it precedes the
/// request/response `id`-echoing convention that governs every later
/// message on the connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloRequest {
    /// Always `"hello"` on the wire. A plain `String` (rather than a unit
    /// enum) so a client that sends the wrong literal here is reported as a
    /// normal `invalid_request`, not a silent deserialize failure on a
    /// field the caller never gets to see named in the error.
    pub op: String,
    /// Client-chosen label. A name, not an identity — see [`HelloAck::pid`].
    pub name: String,
    #[serde(rename = "type")]
    pub client_type: ClientType,
    pub version: String,
}

/// The daemon's reply to [`HelloRequest`]: what permission this client was
/// granted, and — the whole reason this handshake exists — the pid the
/// *kernel* reports for the peer, not whatever the client claimed in
/// `HelloRequest::name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloAck {
    pub ok: bool,
    pub permission: Permission,
    /// Peer pid from `SO_PEERCRED` (Linux) / `LOCAL_PEERPID` (macOS) — see
    /// `serialwrapd::protocol::peer_cred`. Never the client-supplied value.
    pub pid: u32,
    pub server: String,
}

/// Access level granted to a connection, derived from [`ClientType`] at
/// handshake time (`TASKS.md` T1.4; enforcement of the write side is T4.1's
/// rule engine — this crate only defines the shape and the wiki's exact
/// wire strings).
///
/// Wire strings match the [Client protocol
/// wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)'s
/// literal handshake example (`"permission": "read+gated_write"`) verbatim
/// — hence explicit `#[serde(rename = ...)]` per variant rather than
/// `rename_all`, since `+` isn't a valid `rename_all` transform of a
/// CamelCase variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    /// `human`: writes allowed directly, always audited (never gated).
    #[serde(rename = "read+write")]
    ReadWrite,
    /// `agent`: reads freely; writes go through the whitelist/danger/pending
    /// rule engine (T4.1, not yet implemented — see `serialwrapd::gate`).
    #[serde(rename = "read+gated_write")]
    ReadGatedWrite,
    /// `tool`: no byte-level write path at all; can only act while holding
    /// a lease (T2.2, interface-only for now — see `Request::LeaseAcquire`).
    #[serde(rename = "lease_only")]
    LeaseOnly,
}

impl Permission {
    /// The permission granted to a freshly-handshaked client of this
    /// [`ClientType`], per the wiki's Security-model policy table.
    pub fn for_client_type(client_type: ClientType) -> Self {
        match client_type {
            ClientType::Human => Permission::ReadWrite,
            ClientType::Agent => Permission::ReadGatedWrite,
            ClientType::Tool => Permission::LeaseOnly,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_serializes_to_the_wiki_literal_strings() {
        assert_eq!(
            serde_json::to_string(&Permission::ReadWrite).unwrap(),
            "\"read+write\""
        );
        assert_eq!(
            serde_json::to_string(&Permission::ReadGatedWrite).unwrap(),
            "\"read+gated_write\""
        );
        assert_eq!(
            serde_json::to_string(&Permission::LeaseOnly).unwrap(),
            "\"lease_only\""
        );
    }

    #[test]
    fn client_type_maps_to_the_documented_default_permission() {
        assert_eq!(
            Permission::for_client_type(ClientType::Human),
            Permission::ReadWrite
        );
        assert_eq!(
            Permission::for_client_type(ClientType::Agent),
            Permission::ReadGatedWrite
        );
        assert_eq!(
            Permission::for_client_type(ClientType::Tool),
            Permission::LeaseOnly
        );
    }

    #[test]
    fn hello_request_round_trips_matching_the_wiki_example() {
        let json = r#"{"op":"hello","name":"claude-code","type":"agent","version":"0.1.0"}"#;
        let hello: HelloRequest = serde_json::from_str(json).unwrap();
        assert_eq!(hello.op, "hello");
        assert_eq!(hello.name, "claude-code");
        assert_eq!(hello.client_type, ClientType::Agent);
        assert_eq!(hello.version, "0.1.0");
    }

    #[test]
    fn hello_ack_matches_the_wiki_example_shape() {
        let ack = HelloAck {
            ok: true,
            permission: Permission::ReadGatedWrite,
            pid: 5140,
            server: "0.1.0".to_string(),
        };
        let value = serde_json::to_value(&ack).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "ok": true,
                "permission": "read+gated_write",
                "pid": 5140,
                "server": "0.1.0",
            })
        );
    }
}
