//! The "raw_b64 rule" for reconstructing a line's exact original bytes.
//!
//! The daemon's line JSON (`serialwrapd::protocol::session::line_json`) only
//! ever carries `raw_b64` when a line's bytes are *not* valid UTF-8 — see
//! that function's docs (issue #32): when `text` is already a lossless
//! decode of the original bytes, shipping a second base64 copy would be
//! pure overhead. The rule this module follows, and the one every caller in
//! this bridge must follow whenever it needs a line's real bytes, is
//! exactly the daemon's own contract: **if `raw_b64` is present, use it; if
//! it's absent, `text`'s own UTF-8 bytes already *are* the raw bytes.**
//! Deriving "the real bytes" from the lossy `text` field when `raw_b64` is
//! present would silently reintroduce the exact byte-loss bug issue #32
//! fixed — this module exists so that mistake can only be made once, here,
//! not separately in every tool that touches a line.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::Value;

/// The exact original bytes of one daemon-supplied line object, per the
/// raw_b64 rule described in the module docs. Never derives bytes from the
/// lossy `text` field when `raw_b64` is present.
pub fn exact_bytes(line: &Value) -> Vec<u8> {
    if let Some(b64) = line.get("raw_b64").and_then(Value::as_str) {
        // Malformed base64 from the daemon should never happen (it only
        // ever encodes bytes it itself just base64'd), but a defensive
        // empty fallback beats a panic on a compromised/buggy peer.
        BASE64.decode(b64).unwrap_or_default()
    } else {
        line.get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .as_bytes()
            .to_vec()
    }
}

/// Space-separated lowercase hex, e.g. `"1b 5b 33 31 6d"` — matches the
/// convention `serialwrap tail`'s own binary rendering already uses (see
/// `cli::render`), so a human cross-checking MCP tool output against CLI
/// output sees the same byte representation either way.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reshape one daemon `tail`/`read_since` line object into this bridge's
/// tool-result shape: always `seq`/`t_mono`/`t_wall`/`text`/`binary`, plus
/// `raw_hex` — computed via [`exact_bytes`], **never** from the lossy
/// `text` field — only when `binary` is true (i.e. only when the daemon
/// sent a `raw_b64`, meaning this line's bytes are not valid UTF-8).
///
/// `binary`/`raw_hex` rather than passing `raw_b64` straight through: an
/// agent consuming this tool's JSON output reads hex far more reliably than
/// base64 (no risk of it being mistaken for arbitrary embeddable text), and
/// the acceptance criterion this exists for is specifically that a binary
/// line's summary is demonstrably derived from the real bytes, not the
/// lossy display string.
pub fn map_line(line: &Value) -> Value {
    let binary = line.get("raw_b64").is_some();
    let mut obj = serde_json::Map::new();
    obj.insert(
        "seq".to_string(),
        line.get("seq").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "t_mono".to_string(),
        line.get("t_mono").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "t_wall".to_string(),
        line.get("t_wall").cloned().unwrap_or(Value::Null),
    );
    obj.insert(
        "text".to_string(),
        line.get("text").cloned().unwrap_or(Value::Null),
    );
    obj.insert("binary".to_string(), Value::Bool(binary));
    if binary {
        obj.insert(
            "raw_hex".to_string(),
            Value::String(hex_encode(&exact_bytes(line))),
        );
    }
    Value::Object(obj)
}

pub fn map_lines(lines: &[Value]) -> Vec<Value> {
    lines.iter().map(map_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_utf8_line_has_no_raw_hex_and_is_not_marked_binary() {
        let line = json!({"text": "hello", "seq": 1, "t_mono": 1.0, "t_wall": "t"});
        let mapped = map_line(&line);
        assert_eq!(mapped["binary"], false);
        assert!(mapped.get("raw_hex").is_none());
        assert_eq!(mapped["text"], "hello");
    }

    #[test]
    fn exact_bytes_falls_back_to_texts_own_bytes_when_raw_b64_absent() {
        let line = json!({"text": "hello", "seq": 1, "t_mono": 1.0, "t_wall": "t"});
        assert_eq!(exact_bytes(&line), b"hello".to_vec());
    }

    #[test]
    fn binary_line_derives_raw_hex_from_raw_b64_not_from_lossy_text() {
        // Deliberately invalid UTF-8 bytes the daemon would have sent as
        // raw_b64, with `text` holding the lossy U+FFFD replacement form —
        // exactly issue #32's scenario.
        let original: Vec<u8> = vec![0xFF, 0xFE, b'x'];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&original);
        let line = json!({
            "text": String::from_utf8_lossy(&original),
            "raw_b64": b64,
            "seq": 5,
            "t_mono": 2.0,
            "t_wall": "t",
        });
        let mapped = map_line(&line);
        assert_eq!(mapped["binary"], true);
        let raw_hex = mapped["raw_hex"].as_str().unwrap();
        assert_eq!(raw_hex, hex_encode(&original));
        // The replacement character's own UTF-8 bytes (ef bf bd) must never
        // appear in the hex derived from raw_b64 — proving it wasn't
        // computed from the lossy `text` field.
        assert!(!raw_hex.contains("ef bf bd"));
    }

    #[test]
    fn hex_encode_matches_expected_space_separated_format() {
        assert_eq!(hex_encode(&[0x1b, 0x00, 0xff]), "1b 00 ff");
    }
}
