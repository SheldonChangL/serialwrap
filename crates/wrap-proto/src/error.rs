use serde::{Deserialize, Serialize};

/// Structured error codes returned to clients over the UDS protocol.
///
/// Intentionally small at this stage — this is a skeleton type so the shape
/// exists for other crates to compile against. The full request/response
/// surface (and the errors each request can produce) lands with the UDS
/// protocol itself (`TASKS.md` T1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request: bad JSON, unknown request type, missing fields.
    InvalidRequest,
    /// Referenced device is not known to the daemon.
    DeviceNotFound,
    /// Client's identity/permissions do not allow this action.
    PermissionDenied,
    /// Request exceeded its deadline (e.g. `wait_for`).
    Timeout,
    /// Unclassified daemon-side failure.
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
    }
}
