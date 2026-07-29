//! Scripted output generators.
//!
//! These are pure functions (no I/O) so tests can assert exact expected
//! bytes without any PTY involved, and then separately assert that
//! [`crate::MockDevice::write_device_output`] delivers those same bytes
//! byte-exact to a reader on the slave side.

/// A boot banner: the line a real device typically prints first on power-up
/// (used to verify recording starts "as soon as the device appears" and
/// captures the very first line).
pub fn boot_banner() -> Vec<u8> {
    b"mock-device boot: firmware v1.0.0 ready\n".to_vec()
}

/// One line of a periodic stream, e.g. for verifying follow/timestamp/delta
/// behavior against a fixed interval.
pub fn periodic_line(seq: u64) -> Vec<u8> {
    format!("tick seq={seq}\n").into_bytes()
}

/// `times` repeats of `line` (each newline-terminated), for verifying
/// repeated-line folding.
pub fn repeated_line(line: &str, times: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity((line.len() + 1) * times);
    for _ in 0..times {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Line-ending convention for [`lines_with_ending`]/
/// [`repeated_line_with_ending`] — the generator-side counterpart of the
/// read-side line-assembly conventions `serialwrapd::query`'s
/// `LineTerminatorMode`/auto-detection now handles (issue #52), and of the
/// write-side conventions `serialwrap write -e lf|crlf|cr` already exposed.
/// Before issue #52, every mock-device fixture in this crate was hardcoded
/// to `Lf`, which is exactly why none of this project's tests exercised
/// CR-only assembly until this issue's own fixtures (in `serialwrapd::query`
/// and this module's own tests below) added it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Bare `\n`.
    Lf,
    /// `\r\n`.
    Crlf,
    /// Bare `\r`, no `\n` at all — the RTL8735B/AmebaPro2 convention issue
    /// #52 was filed against.
    Cr,
}

impl LineEnding {
    fn terminator(self) -> &'static [u8] {
        match self {
            LineEnding::Lf => b"\n",
            LineEnding::Crlf => b"\r\n",
            LineEnding::Cr => b"\r",
        }
    }
}

/// `lines`, each terminated per `ending` — the line-ending-aware,
/// multi-line generator [`repeated_line_with_ending`] delegates to. Useful
/// for fixtures that need several *distinct* lines (e.g. a CR-only boot
/// sequence), not just one line repeated.
pub fn lines_with_ending(lines: &[&str], ending: LineEnding) -> Vec<u8> {
    let term = ending.terminator();
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(term);
    }
    out
}

/// [`repeated_line`], generalized to any [`LineEnding`] — for exercising
/// `serialwrapd::query`'s CR-only/CRLF/LF line assembly and auto-detection
/// (issue #52) with mock-device fixtures instead of hand-built byte
/// literals in every test.
pub fn repeated_line_with_ending(line: &str, times: usize, ending: LineEnding) -> Vec<u8> {
    lines_with_ending(&vec![line; times], ending)
}

/// `len` bytes of deterministic, non-UTF-8 binary content.
///
/// Cycles through every byte value 0x00..=0xFF, so for any `len >= 1` it is
/// guaranteed to contain byte values that are never valid standalone UTF-8
/// (e.g. 0xFF, 0x80..=0xBF as a lone byte), while still being fully
/// reproducible: no golden file needed, tests can regenerate the same bytes
/// to compare against. Also reused as the payload for the sustained
/// high-throughput test — its content doesn't matter there, only its size
/// and that it's cheap to generate deterministically.
pub fn binary_chunk(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 256) as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_chunk_contains_invalid_utf8() {
        let chunk = binary_chunk(300);
        assert_eq!(chunk.len(), 300);
        assert!(std::str::from_utf8(&chunk).is_err());
        assert_eq!(chunk[255], 0xFF);
        assert_eq!(chunk[256], 0x00);
    }

    #[test]
    fn repeated_line_produces_expected_bytes() {
        let out = repeated_line("hello", 3);
        assert_eq!(out, b"hello\nhello\nhello\n".to_vec());
    }

    #[test]
    fn periodic_line_includes_seq() {
        assert_eq!(periodic_line(42), b"tick seq=42\n".to_vec());
    }

    // ---- Issue #52: line-ending-aware generators ----

    #[test]
    fn lines_with_ending_lf_matches_plain_repeated_line() {
        assert_eq!(
            lines_with_ending(&["hello", "hello", "hello"], LineEnding::Lf),
            repeated_line("hello", 3),
            "LF generator must produce byte-for-byte the same output as the pre-existing \
             LF-only helper"
        );
    }

    #[test]
    fn lines_with_ending_crlf_produces_expected_bytes() {
        assert_eq!(
            lines_with_ending(&["a", "b"], LineEnding::Crlf),
            b"a\r\nb\r\n".to_vec()
        );
    }

    #[test]
    fn lines_with_ending_cr_produces_bare_cr_no_lf_at_all() {
        let out = lines_with_ending(&["a", "b"], LineEnding::Cr);
        assert_eq!(out, b"a\rb\r".to_vec());
        assert!(
            !out.contains(&b'\n'),
            "a CR-only fixture must never contain an LF byte, or it isn't actually exercising \
             CR-only assembly: {out:?}"
        );
    }

    #[test]
    fn repeated_line_with_ending_cr_repeats_correctly() {
        assert_eq!(
            repeated_line_with_ending("tick", 3, LineEnding::Cr),
            b"tick\rtick\rtick\r".to_vec()
        );
    }
}
