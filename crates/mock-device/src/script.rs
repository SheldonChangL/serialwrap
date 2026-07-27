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
}
