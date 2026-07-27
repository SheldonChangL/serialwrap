use serde::{Deserialize, Serialize};

/// Structured error codes returned to clients over the UDS protocol.
///
/// The wiki's [Client protocol
/// page](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
/// documents seven codes — `device_not_found`, `device_disconnected`,
/// `data_aged_out`, `write_denied`, `lease_held`, `permission_denied`,
/// `invalid_request` — all present below. `Timeout` and `Internal` are
/// *not* on that table: they're implementation-only additions (request
/// deadlines such as `wait_for`, and unclassified daemon-side failures),
/// called out separately so nobody mistakes the wiki table for incomplete
/// when cross-referencing this enum later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No such device ID.
    DeviceNotFound,
    /// Known device, not currently attached.
    DeviceDisconnected,
    /// Cursor points into evicted data; includes oldest available `seq`.
    DataAgedOut,
    /// Gate rejected; includes reason and matched rule.
    WriteDenied,
    /// Another lease is active; includes holder.
    LeaseHeld,
    /// Client's permission level insufficient.
    PermissionDenied,
    /// Malformed request: bad JSON, unknown request type, missing fields.
    InvalidRequest,
    /// Request exceeded its deadline (e.g. `wait_for`). Implementation
    /// addition — not in the wiki's error table.
    Timeout,
    /// Unclassified daemon-side failure. Implementation addition — not in
    /// the wiki's error table.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_using_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::DeviceNotFound).unwrap(),
            "\"device_not_found\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::DeviceDisconnected).unwrap(),
            "\"device_disconnected\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::DataAgedOut).unwrap(),
            "\"data_aged_out\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::WriteDenied).unwrap(),
            "\"write_denied\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::LeaseHeld).unwrap(),
            "\"lease_held\""
        );
    }
}
