//! Timestamp parsing/formatting for `serialwrap tail` (issue #7 /
//! `TASKS.md` T1.5).

use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};

/// Default display format for a record's `t_wall`: local wall-clock time,
/// millisecond precision, no date.
///
/// # Why this format
///
/// - **Local wall clock, not relative-to-session-start or a bare delta**:
///   the [UX design
///   wiki](https://github.com/SheldonChangL/serialwrap/wiki/UX-design)
///   documents all three as valid *GUI* modes, but this tool's whole job
///   (per issue #7: "後續所有任務 debug 的地板") is being the
///   ground-truth cross-check when something else looks wrong — that
///   means correlating against wall-clock reality a human already has in
///   their head ("I plugged it in at 14:32", another terminal's own
///   timestamps, `dmesg`), which a relative-to-invocation counter can't
///   do.
/// - **`t_wall` as stored, not `t_mono`**: the wiki is explicit that
///   `t_wall` reflects "host-side arrival, not device emission" — exactly
///   what a human watching a live tail wants to know ("when did *I* see
///   this"), and reusing the daemon's own computed value keeps this tool
///   from re-deriving timing it doesn't own.
/// - **Millisecond precision**: matches `t_wall`'s own stored precision
///   and this project's documented timing floor (USB latency-timer
///   granularity is ~16ms by default per `Cargo.toml`'s own commentary on
///   FTDI adapters) — finer would be false precision, coarser would lose
///   real ordering information between rapid lines.
/// - **No date**: a live debugging session lives inside one day almost
///   always; dropping the date keeps every line's timestamp column a
///   constant width and easy to scan. `t_wall` itself (available via the
///   raw wire reply, not this CLI's rendering) still carries the full
///   RFC 3339 timestamp for anyone who needs to cross a day boundary.
pub fn format_timestamp(t_wall: &str) -> String {
    match DateTime::parse_from_rfc3339(t_wall) {
        Ok(dt) => dt.with_timezone(&Local).format("%H:%M:%S%.3f").to_string(),
        // Never hide a record because its timestamp didn't parse — print
        // whatever the daemon sent verbatim instead of dropping the line
        // or panicking. This should be unreachable in practice: the
        // daemon always emits `t_wall` via `chrono`'s own RFC 3339
        // encoder.
        Err(_) => t_wall.to_string(),
    }
}

/// Parse `tail --since`'s argument: either an absolute RFC 3339 timestamp,
/// or a relative duration shorthand (`10m`, `2h`, `30s`, `1d`) measured
/// back from now.
pub fn parse_since(input: &str) -> Result<DateTime<Utc>, String> {
    let trimmed = input.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    if trimmed.len() < 2 {
        return Err(invalid_since_message(input));
    }
    let split_at = trimmed.len() - 1;
    let (digits, unit) = trimmed.split_at(split_at);
    let amount: i64 = digits.parse().map_err(|_| invalid_since_message(input))?;
    let delta = match unit {
        "s" => ChronoDuration::seconds(amount),
        "m" => ChronoDuration::minutes(amount),
        "h" => ChronoDuration::hours(amount),
        "d" => ChronoDuration::days(amount),
        _ => return Err(invalid_since_message(input)),
    };
    Ok(Utc::now() - delta)
}

fn invalid_since_message(input: &str) -> String {
    format!(
        "not a recognized timestamp or duration: {input:?} (expected RFC 3339, e.g. \
         2026-07-27T10:00:00+08:00, or a duration like 10m/2h/30s/1d)"
    )
}

/// Whether a record's `t_wall` is at or after `threshold` — used by
/// `--since` to select history. A `t_wall` that fails to parse is kept
/// rather than dropped, for the same reason [`format_timestamp`] never
/// hides an unparsable one: this tool must never silently make the
/// daemon's recording look emptier than it is.
pub fn passes_since(t_wall: &str, threshold: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(t_wall) {
        Ok(dt) => dt.with_timezone(&Utc) >= threshold,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_as_local_time_with_millisecond_precision() {
        let formatted = format_timestamp("2026-07-27T10:34:12.443+00:00");
        // Exact wall value depends on the test runner's local timezone, but
        // the shape must always be HH:MM:SS.mmm.
        assert_eq!(formatted.len(), "10:34:12.443".len());
        assert!(formatted.contains('.'));
    }

    #[test]
    fn unparsable_timestamp_is_returned_verbatim_not_dropped() {
        assert_eq!(format_timestamp("not-a-timestamp"), "not-a-timestamp");
    }

    #[test]
    fn parses_relative_duration_shorthand() {
        let before = Utc::now();
        let since = parse_since("10m").expect("10m parses");
        let expected = before - ChronoDuration::minutes(10);
        let diff = (since - expected).num_milliseconds().abs();
        assert!(diff < 2000, "parsed --since drifted by {diff}ms");
    }

    #[test]
    fn parses_absolute_rfc3339_timestamp() {
        let since = parse_since("2026-07-27T10:00:00+08:00").expect("rfc3339 parses");
        assert_eq!(since.to_rfc3339(), "2026-07-27T02:00:00+00:00");
    }

    #[test]
    fn rejects_an_unrecognized_unit() {
        let err = parse_since("10x").unwrap_err();
        assert!(err.contains("10x"), "error was: {err}");
    }

    #[test]
    fn passes_since_keeps_records_at_or_after_the_threshold() {
        let threshold = DateTime::parse_from_rfc3339("2026-07-27T10:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert!(passes_since("2026-07-27T10:00:00+00:00", threshold));
        assert!(passes_since("2026-07-27T10:00:01+00:00", threshold));
        assert!(!passes_since("2026-07-27T09:59:59+00:00", threshold));
    }
}
