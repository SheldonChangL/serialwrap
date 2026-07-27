use serde::{Deserialize, Serialize};

/// Category of client connected to the daemon over the UDS protocol.
///
/// Mirrors the `client_type` field in the record schema (`TASKS.md`) and
/// drives write-gate policy: `Human` writes are audited but not gated,
/// `Agent` writes go through the rule engine, and `Tool` can only act while
/// holding a lease (see `TASKS.md` T4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    Human,
    Agent,
    Tool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_using_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&ClientType::Agent).unwrap(),
            "\"agent\""
        );
        assert_eq!(
            serde_json::to_string(&ClientType::Human).unwrap(),
            "\"human\""
        );
        assert_eq!(
            serde_json::to_string(&ClientType::Tool).unwrap(),
            "\"tool\""
        );
    }
}
