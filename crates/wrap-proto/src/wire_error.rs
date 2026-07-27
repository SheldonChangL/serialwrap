use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// A structured error reply body (`TASKS.md` T1.4). Every error the daemon
/// sends is one of these — "never bare strings", per the wiki's Error
/// handling section — so a client can always branch on `code` and, for the
/// codes that carry extra context (`data_aged_out`'s `oldest_available_seq`,
/// `write_denied`'s `reason`/`matched_rule`, `lease_held`'s `holder`,
/// `invalid_request`'s parse failure detail), read it out of `extra`
/// without the wire shape needing a different Rust type per code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
    /// Code-specific extra fields (e.g. `oldest_available_seq`,
    /// `matched_rule`, `holder`). Flattened directly into the JSON object
    /// alongside `code`/`message` rather than nested under an `extra` key,
    /// so e.g. `data_aged_out`'s `oldest_available_seq` reads at the same
    /// level as `code` on the wire — matching the wiki's own
    /// `{"result": "denied", "reason": ..., "matched_rule": ...}` example
    /// shape for a structured decision.
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty", default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl WireError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            extra: serde_json::Map::new(),
        }
    }

    /// Attach one extra field, chainable — e.g.
    /// `WireError::new(ErrorCode::DataAgedOut, "...")
    ///     .with("oldest_available_seq", oldest)`.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_fields_flatten_alongside_code_and_message() {
        let err = WireError::new(ErrorCode::DataAgedOut, "cursor points into evicted data")
            .with("oldest_available_seq", 4096u64);
        let value = serde_json::to_value(&err).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "code": "data_aged_out",
                "message": "cursor points into evicted data",
                "oldest_available_seq": 4096,
            })
        );
    }

    #[test]
    fn no_extra_fields_serializes_without_an_empty_map() {
        let err = WireError::new(ErrorCode::InvalidRequest, "missing field `device`");
        let value = serde_json::to_value(&err).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "code": "invalid_request",
                "message": "missing field `device`",
            })
        );
    }

    #[test]
    fn round_trips_through_json() {
        let err = WireError::new(ErrorCode::WriteDenied, "danger pattern matched")
            .with("matched_rule", "danger:erase");
        let json = serde_json::to_string(&err).unwrap();
        let back: WireError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, back);
    }
}
