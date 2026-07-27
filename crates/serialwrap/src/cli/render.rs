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
//! # Known limitation: binary rendering is best-effort, not byte-exact
//!
//! The wire's `tail`/`read_since` replies carry only
//! [`serialwrapd::query::AssembledLine::text`] — a `String` produced by
//! `String::from_utf8_lossy` over the original device bytes (see that
//! module's `ingest`). By the time a line reaches this CLI, any byte
//! sequence that wasn't valid UTF-8 has *already* been replaced with
//! U+FFFD server-side; the original bytes are gone and there is no wire op
//! that returns them (`Tail`/`ReadSince`/`Subscribe` all return assembled
//! *lines*, never a raw record). So "binary 不污染終端" is implemented
//! here as: detect a line that isn't safely printable (contains a
//! replacement character or a non-tab control byte) and render *the
//! lossy-decoded text's own bytes* as a length + hex preview, using the
//! same "length plus 64-byte hex preview" convention the [Client protocol
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
//! documents for the MCP tool surface's binary regions. This achieves the
//! acceptance criterion's actual goal — never blast raw control bytes/
//! escape sequences at the user's terminal — but is not byte-for-byte
//! identical to what the device originally sent. A future task that wants
//! true byte-exact binary display would need `AssembledLine` (and the
//! `tail`/`read_since` wire reply) to carry the original bytes alongside
//! `text`, which is a `serialwrapd`/`wrap-proto` change out of this
//! issue's scope (see issue #7: "不要修改 crates/serialwrapd/ 或
//! crates/wrap-proto/").

use serde_json::Value;

use crate::cli::time::format_timestamp;

/// How many bytes of hex to show for a binary-flagged line — matches the
/// wiki's own MCP-tool-surface convention ("binary regions show length
/// plus 64-byte hex preview") so a human reading `tail` and an agent
/// reading the MCP bridge see the same-shaped summary.
const HEX_PREVIEW_BYTES: usize = 64;

/// Render one assembled data line (`kind: rx`): `<timestamp> <content>`.
pub fn render_data_line(t_wall: &str, text: &str) -> String {
    format!("{} {}", format_timestamp(t_wall), render_data_content(text))
}

/// Render one out-of-band record (`event`/`gate`): `# <timestamp>
/// <content>`.
pub fn render_event_line(t_wall: &str, record: &Value) -> String {
    format!("# {} {}", format_timestamp(t_wall), event_content(record))
}

/// `text` if it's safe to print directly to a terminal, otherwise a
/// `[N bytes binary — hex...]` summary. See the module docs for exactly
/// what "binary" means here and its known limitation.
fn render_data_content(text: &str) -> String {
    if is_terminal_safe(text) {
        return text.to_string();
    }
    let bytes = text.as_bytes();
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

/// A line is terminal-safe when it contains no U+FFFD (this CLI's only
/// remaining signal that the original bytes weren't valid UTF-8 — see the
/// module docs) and no control character other than tab (control
/// characters — especially ESC — can reprogram the terminal itself, which
/// is exactly the "binary 不污染終端" failure mode this guards against).
fn is_terminal_safe(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c == '\u{FFFD}' || (c.is_control() && c != '\t'))
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

    #[test]
    fn data_line_has_no_hash_prefix() {
        let line = render_data_line("2026-07-27T10:34:12.443+08:00", "boot ok");
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
        assert_eq!(render_data_content("Temp: 25.7 C"), "Temp: 25.7 C");
    }

    #[test]
    fn replacement_character_triggers_binary_rendering() {
        let text = "\u{FFFD}\u{FFFD}\u{FFFD}";
        let rendered = render_data_content(text);
        assert!(rendered.starts_with('['), "rendered was: {rendered}");
        assert!(
            rendered.contains("bytes binary"),
            "rendered was: {rendered}"
        );
        assert!(!rendered.contains('\u{FFFD}'));
    }

    #[test]
    fn control_bytes_never_reach_the_terminal_raw() {
        let text = "before\x1b[31mred\x1b[0mafter";
        let rendered = render_data_content(text);
        assert!(!rendered.contains('\x1b'), "rendered was: {rendered:?}");
        assert!(
            rendered.contains("bytes binary"),
            "rendered was: {rendered}"
        );
    }

    #[test]
    fn binary_preview_is_capped_at_64_bytes_of_hex() {
        let text: String = "\u{FFFD}".repeat(100);
        let rendered = render_data_content(&text);
        // Each U+FFFD is 3 bytes in UTF-8, so 100 of them is 300 bytes —
        // well past the 64-byte preview cap.
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
    }

    #[test]
    fn tab_alone_is_still_terminal_safe() {
        assert!(is_terminal_safe("a\tb"));
    }
}
