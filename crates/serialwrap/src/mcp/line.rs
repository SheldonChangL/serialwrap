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

/// Reconstruct a [`serialwrapd::query::AssembledLine`] from one daemon
/// `tail`/`read_since` wire line object (`TASKS.md` T3.2, issue #13).
///
/// This bridge is a *separate process* from the daemon, so it can never
/// reach into a live [`serialwrapd::query::DeviceQueryState`] directly —
/// but it can, and does, link `serialwrapd` as an ordinary library
/// dependency (see that crate's `presentation` module docs on why this is
/// the intended reuse story for both this bridge and the future GUI). The
/// wire line JSON already carries every field losslessly (see the
/// raw_b64 rule above), so reconstructing the exact same struct
/// [`serialwrapd::presentation::present`] operates on is just a matter of
/// reading them back out — never a lossy approximation.
pub fn assembled_line_from_wire(line: &Value) -> serialwrapd::query::AssembledLine {
    serialwrapd::query::AssembledLine {
        raw: exact_bytes(line),
        text: line
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        seq: line.get("seq").and_then(Value::as_u64).unwrap_or(0),
        t_mono: line.get("t_mono").and_then(Value::as_f64).unwrap_or(0.0),
        t_wall: line
            .get("t_wall")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_bytes_falls_back_to_texts_own_bytes_when_raw_b64_absent() {
        let line = json!({"text": "hello", "seq": 1, "t_mono": 1.0, "t_wall": "t"});
        assert_eq!(exact_bytes(&line), b"hello".to_vec());
    }

    #[test]
    fn assembled_line_from_wire_round_trips_every_field() {
        let original: Vec<u8> = vec![0xFF, 0xFE, b'z'];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&original);
        let wire = json!({
            "text": String::from_utf8_lossy(&original),
            "raw_b64": b64,
            "seq": 42,
            "t_mono": 3.5,
            "t_wall": "2026-07-27T00:00:00Z",
        });
        let reconstructed = assembled_line_from_wire(&wire);
        assert_eq!(reconstructed.raw, original);
        assert_eq!(reconstructed.seq, 42);
        assert_eq!(reconstructed.t_mono, 3.5);
        assert_eq!(reconstructed.t_wall, "2026-07-27T00:00:00Z");
    }

    #[test]
    fn assembled_line_from_wire_recovers_raw_from_text_when_raw_b64_absent() {
        let wire = json!({"text": "plain", "seq": 1, "t_mono": 0.0, "t_wall": "t"});
        let reconstructed = assembled_line_from_wire(&wire);
        assert_eq!(reconstructed.raw, b"plain".to_vec());
        assert_eq!(reconstructed.text, "plain");
    }

    #[test]
    fn assembled_line_from_wire_derives_raw_from_raw_b64_not_from_lossy_text() {
        // Deliberately invalid UTF-8 bytes the daemon would have sent as
        // raw_b64, with `text` holding the lossy U+FFFD replacement form —
        // exactly issue #32's scenario.
        let original: Vec<u8> = vec![0xFF, 0xFE, b'x'];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&original);
        let wire = json!({
            "text": String::from_utf8_lossy(&original),
            "raw_b64": b64,
            "seq": 5,
            "t_mono": 2.0,
            "t_wall": "t",
        });
        let reconstructed = assembled_line_from_wire(&wire);
        assert_eq!(reconstructed.raw, original);
        // The replacement character's own UTF-8 bytes (ef bf bd) must never
        // appear in the reconstructed raw bytes — proving they weren't
        // derived from the lossy `text` field.
        assert_ne!(reconstructed.raw, "\u{FFFD}\u{FFFD}x".as_bytes());
    }

    #[test]
    fn hex_encode_matches_expected_space_separated_format() {
        assert_eq!(hex_encode(&[0x1b, 0x00, 0xff]), "1b 00 ff");
    }
}
