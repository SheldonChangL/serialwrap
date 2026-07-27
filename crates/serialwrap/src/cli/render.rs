//! Line rendering for `serialwrap tail` (issue #7 / `TASKS.md` T1.5).
//!
//! Presentation follows the [UX design
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/UX-design)'s log
//! line vocabulary verbatim, so the CLI and the eventual GUI never
//! disagree about what a "data row" vs. an "event row" looks like:
//!
//! - **Data rows** (`kind: rx`): `<timestamp> <content>` — device output,
//!   untrusted, rendered plain.
//! - **Event rows** (`event`/`gate`): `# <timestamp> <content>` — broker
//!   fact, prefixed so it can never be mistaken for something the device
//!   said. This distinction is exactly the "log-as-data boundary" issue
//!   #7 calls out: bytes from the board are data, broker events are
//!   system truth, and the two must stay visually distinguishable.
//!
//! # Byte-exact binary rendering
//!
//! Issue #32 fixed the protocol layer's own byte fidelity: a `tail`/
//! `read_since`/`subscribe` line reply now carries `raw_b64` — the line's
//! exact original bytes, base64-encoded — whenever those bytes weren't
//! valid UTF-8 (see `serialwrapd::query::AssembledLine` and
//! `serialwrapd::protocol::session::line_json`'s docs for the presence
//! rule and why not every line needs it). [`line_bytes`] is what recovers
//! the real device bytes for a line from that wire shape: `raw_b64` when
//! present, otherwise `text`'s own UTF-8 bytes — which, for a line the
//! server found to already be valid UTF-8, *are* the original bytes
//! verbatim (nothing was lost turning them into `text` in the first
//! place). Either way this CLI now renders the same bytes the device
//! actually sent, never a lossy stand-in, matching the "length plus
//! 64-byte hex preview" convention the [Client protocol
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
//! documents for the MCP tool surface's binary regions.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::Value;

use crate::cli::time::format_timestamp;

/// How many bytes of hex to show for a binary-flagged line — matches the
/// wiki's own MCP-tool-surface convention ("binary regions show length
/// plus 64-byte hex preview") so a human reading `tail` and an agent
/// reading the MCP bridge see the same-shaped summary.
const HEX_PREVIEW_BYTES: usize = 64;

/// Render one assembled data line (`kind: rx`): `<timestamp> <content>`.
/// `line` is the wire's JSON object for one assembled line (`text`, `seq`,
/// `t_mono`, `t_wall`, and optionally `raw_b64`) — see the module docs and
/// [`line_bytes`] for how the real device bytes are recovered from it.
pub fn render_data_line(t_wall: &str, line: &Value) -> String {
    format!(
        "{} {}",
        format_timestamp(t_wall),
        render_data_content(&line_bytes(line))
    )
}

/// Render one out-of-band record (`event`/`gate`): `# <timestamp>
/// <content>`.
pub fn render_event_line(t_wall: &str, record: &Value) -> String {
    format!("# {} {}", format_timestamp(t_wall), event_content(record))
}

/// Recover a line's exact original device bytes from the wire's JSON
/// shape: `raw_b64`, base64-decoded, when present; otherwise `text`'s own
/// UTF-8 bytes. See the module docs for why the latter is still
/// byte-exact, not an approximation, whenever `raw_b64` is absent.
fn line_bytes(line: &Value) -> Vec<u8> {
    if let Some(b64) = line.get("raw_b64").and_then(Value::as_str) {
        if let Ok(bytes) = BASE64.decode(b64) {
            return bytes;
        }
        // Malformed `raw_b64` from a future/misbehaving daemon build:
        // fall through to `text` rather than losing the line entirely.
    }
    line.get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .as_bytes()
        .to_vec()
}

/// `bytes` rendered as text if it's safe to print directly to a terminal,
/// otherwise a `[N bytes binary — hex...]` summary of those same bytes —
/// see the module docs for why this is always the device's real bytes now,
/// never a lossy stand-in.
fn render_data_content(bytes: &[u8]) -> String {
    if is_terminal_safe(bytes) {
        // Safe by construction: `is_terminal_safe` only returns `true`
        // after confirming `bytes` is valid UTF-8.
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let preview: Vec<String> = bytes
        .iter()
        .take(HEX_PREVIEW_BYTES)
        .map(|b| format!("{b:02x}"))
        .collect();
    let hex = preview.join(" ");
    if bytes.len() > HEX_PREVIEW_BYTES {
        format!("[{} bytes binary — {hex}...]", bytes.len())
    } else {
        format!("[{} bytes binary — {hex}]", bytes.len())
    }
}

/// `bytes` is terminal-safe when it's valid UTF-8 and contains no control
/// byte other than tab (control bytes — especially ESC — can reprogram the
/// terminal itself, which is exactly the "binary 不污染終端" failure mode
/// this guards against). Invalid UTF-8 is never safe: it can't be
/// meaningfully rendered as text at all, byte-exact or otherwise.
fn is_terminal_safe(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(s) => !s.chars().any(|c| c.is_control() && c != '\t'),
        Err(_) => false,
    }
}

/// Render an out-of-band record's content: its event/gate label, plus
/// every other field flattened as `key=value` (sorted for deterministic,
/// diffable output).
fn event_content(record: &Value) -> String {
    let kind = record
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("event");
    let label = if kind == "event" {
        record
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("event")
            .to_string()
    } else {
        kind.to_string()
    };

    let mut extras: Vec<(String, String)> = record
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !matches!(k.as_str(), "seq" | "t_mono" | "t_wall" | "kind" | "event"))
        .map(|(k, v)| (k.clone(), scalar(v)))
        .collect();
    extras.sort();

    if extras.is_empty() {
        label
    } else {
        let joined = extras
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{label} {joined}")
    }
}

/// Render one JSON value compactly for a `key=value` pair: strings without
/// their surrounding quotes (so `reason=timeout_60s`, not
/// `reason="timeout_60s"`), everything else as compact JSON.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wire-shaped line `Value` the way a real `tail`/`read_since`
    /// reply would: `text` always, plus `raw_b64` only when `raw` isn't
    /// valid UTF-8 — mirroring `serialwrapd::protocol::session::line_json`'s
    /// presence rule, so these tests exercise the real wire shape rather
    /// than a shortcut.
    fn line_value(raw: &[u8]) -> Value {
        let text = String::from_utf8_lossy(raw).into_owned();
        let mut obj = serde_json::Map::new();
        obj.insert("text".to_string(), text.into());
        obj.insert("seq".to_string(), 0.into());
        obj.insert("t_mono".to_string(), 0.0.into());
        obj.insert("t_wall".to_string(), "2026-07-27T10:34:12.443+08:00".into());
        if std::str::from_utf8(raw).is_err() {
            obj.insert("raw_b64".to_string(), BASE64.encode(raw).into());
        }
        Value::Object(obj)
    }

    #[test]
    fn data_line_has_no_hash_prefix() {
        let line = render_data_line("2026-07-27T10:34:12.443+08:00", &line_value(b"boot ok"));
        assert!(!line.starts_with('#'), "line was: {line}");
        assert!(line.ends_with("boot ok"), "line was: {line}");
    }

    #[test]
    fn event_line_is_hash_prefixed() {
        let record = serde_json::json!({
            "seq": 1, "t_mono": 0.1, "t_wall": "2026-07-27T10:34:12.443+08:00",
            "kind": "event", "event": "connect", "device_id": "usb-1a86",
        });
        let line = render_event_line("2026-07-27T10:34:12.443+08:00", &record);
        assert!(line.starts_with("# "), "line was: {line}");
        assert!(line.contains("connect"), "line was: {line}");
        assert!(line.contains("device_id=usb-1a86"), "line was: {line}");
    }

    #[test]
    fn gate_event_uses_the_gate_kind_as_its_label() {
        let record = serde_json::json!({
            "seq": 2, "t_mono": 0.2, "t_wall": "2026-07-27T10:34:13.000+08:00",
            "kind": "gate", "action": "deny", "reason": "timeout_60s", "request_seq": 1,
        });
        let line = render_event_line("2026-07-27T10:34:13.000+08:00", &record);
        assert!(line.starts_with("# "), "line was: {line}");
        assert!(line.contains(" gate "), "line was: {line}");
        assert!(line.contains("reason=timeout_60s"), "line was: {line}");
    }

    #[test]
    fn printable_text_renders_unchanged() {
        assert_eq!(render_data_content(b"Temp: 25.7 C"), "Temp: 25.7 C");
    }

    #[test]
    fn line_bytes_prefers_raw_b64_over_the_lossy_text_field() {
        // Invalid UTF-8 mixed with ordinary text — exactly what
        // `String::from_utf8_lossy` cannot round-trip. `raw_b64` must carry
        // the true bytes regardless of what `text` looks like.
        let original: Vec<u8> = {
            let mut v = b"a-".to_vec();
            v.extend_from_slice(&[0xFF, 0xFE]);
            v.extend_from_slice(b"-b");
            v
        };
        let line = line_value(&original);
        assert!(
            line.get("raw_b64").is_some(),
            "invalid-UTF-8 line must carry raw_b64: {line}"
        );
        assert_eq!(
            line_bytes(&line),
            original,
            "line_bytes must recover the exact original bytes, not the lossy text"
        );
    }

    #[test]
    fn line_bytes_falls_back_to_text_when_raw_b64_is_absent() {
        let line = line_value(b"plain ascii");
        assert!(line.get("raw_b64").is_none());
        assert_eq!(line_bytes(&line), b"plain ascii");
    }

    #[test]
    fn invalid_utf8_bytes_trigger_binary_rendering_with_the_exact_hex() {
        let original: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01];
        let line = line_value(&original);
        let rendered = render_data_line("2026-07-27T10:34:12.443+08:00", &line);
        assert!(
            rendered.contains("4 bytes binary"),
            "rendered was: {rendered}"
        );
        let expected_hex = original
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            rendered.contains(&expected_hex),
            "rendered was: {rendered}, expected hex {expected_hex}"
        );
        // The lossy U+FFFD replacement text must never leak into the
        // rendered output — only the real bytes' hex.
        assert!(!rendered.contains('\u{FFFD}'), "rendered was: {rendered}");
    }

    #[test]
    fn control_bytes_never_reach_the_terminal_raw() {
        let bytes = b"before\x1b[31mred\x1b[0mafter";
        let rendered = render_data_content(bytes);
        assert!(!rendered.contains('\x1b'), "rendered was: {rendered:?}");
        assert!(
            rendered.contains("bytes binary"),
            "rendered was: {rendered}"
        );
    }

    #[test]
    fn binary_preview_is_capped_at_64_bytes_of_hex() {
        // 300 invalid-UTF-8 bytes — well past the 64-byte preview cap.
        let bytes: Vec<u8> = vec![0xFFu8; 300];
        let rendered = render_data_content(&bytes);
        assert!(
            rendered.contains("300 bytes binary"),
            "rendered was: {rendered}"
        );
        assert!(rendered.contains("..."), "rendered was: {rendered}");
        // Pull out just the hex preview section (between the em dash and
        // the trailing `...]`) rather than counting hex-looking characters
        // over the whole rendered string — words like "bytes"/"binary"
        // themselves contain valid hex digits (`b`, `0`), which would
        // over-count.
        let hex_section = rendered
            .split('—')
            .nth(1)
            .expect("rendered content has an em-dash separator")
            .trim()
            .trim_end_matches("...]");
        let byte_count = hex_section.split_whitespace().count();
        assert_eq!(
            byte_count, HEX_PREVIEW_BYTES,
            "hex section was: {hex_section:?}"
        );
        // And the hex itself must be the real bytes (all 0xff), not a
        // stand-in derived from a lossy-decoded string.
        assert!(
            hex_section.split_whitespace().all(|h| h == "ff"),
            "hex section was: {hex_section:?}"
        );
    }

    #[test]
    fn tab_alone_is_still_terminal_safe() {
        assert!(is_terminal_safe(b"a\tb"));
    }

    #[test]
    fn valid_utf8_containing_the_actual_replacement_character_is_still_terminal_safe() {
        // A device that deliberately emits the U+FFFD glyph as valid UTF-8
        // (EF BF BD) is not the same thing as invalid bytes that got
        // lossily replaced — now that byte-exactness no longer depends on
        // spotting U+FFFD as a heuristic, this must render as plain text,
        // not trigger a false-positive binary summary.
        let bytes = "temp=\u{FFFD}C".as_bytes();
        assert!(std::str::from_utf8(bytes).is_ok());
        assert!(is_terminal_safe(bytes));
        assert_eq!(render_data_content(bytes), "temp=\u{FFFD}C");
    }
}
