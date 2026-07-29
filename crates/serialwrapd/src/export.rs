//! Export: turning a device's recorded stream into a portable jsonl/txt/bin
//! artifact (`TASKS.md` T2.4, issue #11). See the [Event stream and storage
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Event-stream-and-storage)'s
//! "Export formats" section for the authoritative guarantee each format
//! makes; this module is what implements them.
//!
//! # Why this lives in `serialwrapd`, not the CLI
//!
//! `TASKS.md` T2.4 is explicit: "GUI 匯出（T5.5）走同一個 daemon API，不另做
//! 一套" — the GUI's export dialog (a later task, out of this one's scope)
//! must produce byte-identical output to the CLI for the same parameters.
//! The only way to guarantee that without maintaining two implementations is
//! to put the range resolution, filtering, and format rendering here, where
//! any future in-process caller (an embedded GUI backend linking this crate
//! directly — the same pattern `presentation::present` already established)
//! can call [`export_range`] itself, exactly like the CLI does over the wire
//! via `wrap_proto::Request::Export` (see `protocol::session`'s dispatch
//! arm). The CLI (`crates/serialwrap/src/cli/export.rs`) is deliberately
//! thin: argument parsing, `--boot`/`--last` resolution (via the existing
//! `query_events`/wall-clock-duration primitives — no new daemon logic
//! needed for those), and writing the result to a file or stdout.
//!
//! # Record-level (`jsonl`/`bin`) vs. line-level (`txt`)
//!
//! `jsonl` and `bin` are deliberately **not** line-assembled: the wiki's own
//! words are "jsonl: stream as stored" and "bin: rx payloads concatenated"
//! — both promises are about the *stored records*, not about
//! `query::DeviceQueryState`'s derived notion of a device "line". Line
//! assembly is a separate, independently re-derivable transform (see
//! `query.rs`'s own module docs on why it isn't done in the recorder
//! either); re-deriving it for `bin` would risk exactly the kind of silent
//! transformation this format exists to rule out, and would make `jsonl`
//! non-replayable (replaying it should reproduce the exact chunk boundaries
//! and timing the device actually produced).
//!
//! `txt` is the one human-readable format, so it *does* assemble lines
//! (mirroring `query::AssembledLine`'s byte-level algorithm, reimplemented
//! here as a plain batch pass — see [`assemble_txt_lines`] — since a
//! one-shot export has no need for `DeviceQueryState`'s long-lived
//! `Mutex`/`Notify` machinery). Unlike the live query layer, an export's
//! trailing not-yet-newline-terminated bytes (if any) are still surfaced as
//! a final row: nothing more is ever coming for a bounded snapshot, so
//! holding them back (as the live tail view correctly does) would just be
//! silent data loss here.
//!
//! # Filter semantics
//!
//! A `filter` (same `wrap_proto::Filter` shape `tail`/`read_since` use)
//! narrows which `rx` **records** contribute to `jsonl`/`txt` output — never
//! `tx`/`event`/`gate` records, which are out-of-band and always kept
//! (matching `query.rs`'s own "filters narrow lines, never suppress the
//! fact that something happened" rule). This is deliberately per-*record*,
//! not per-assembled-*line* as `tail`'s `Filter` is: filtering before line
//! assembly keeps `jsonl`'s core guarantee intact (every record it emits is
//! either the complete original record or entirely absent, never
//! surgically altered), and `txt`'s line assembly then runs over exactly
//! that same filtered record set, so the two formats never disagree about
//! which bytes survived. The tradeoff: if one device "line" happens to
//! span two `rx` reads and the filter pattern only appears in one of them,
//! only that one read's bytes survive — a rarer case in practice than it
//! sounds, since most real serial devices emit one `rx` chunk per
//! newline-flushed write, but worth calling out plainly. `format: Bin`
//! rejects any filter outright (see [`ExportError::FilterNotAllowedForBin`])
//! — filtering is definitionally incompatible with "byte-exact,
//! unconditionally".

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Map;

use wrap_proto::{ClientType, ExportBound, ExportFormat, Filter, Record};

use crate::recorder::{ReadSinceError, Recorder};

/// Upper bound on how many bytes of raw JSONL one `Recorder::read_since`
/// page pulls per call while paging through an export range. Mirrors
/// `query::MAX_INGEST_BYTES`'s reasoning: generous enough that a 100k-row
/// export completes in a small handful of pages rather than being a knob
/// anyone needs to tune.
const PAGE_BYTES: usize = 16 * 1024 * 1024;

/// How many leading bytes of a `txt` binary annotation's hex to show.
/// Mirrors `cli::render`'s / `presentation`'s own 64-byte preview
/// convention (reimplemented independently here — see the module docs on
/// why this crate doesn't reuse either of those).
const TXT_HEX_PREVIEW_BYTES: usize = 64;

/// A resolved export range. `from`/`to` are `None` when that end is open:
/// `from: None` starts at the oldest retained record, `to: None` runs to
/// the current tip (a snapshot of whatever is durably on disk *right now*
/// — not a live-following `tail -f`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportRange {
    pub from: Option<ExportBound>,
    pub to: Option<ExportBound>,
}

/// Failure modes for [`export_range`]. Note that a range partially or
/// wholly overlapping ring-evicted data is *not* one of these — see
/// [`ExportResult::truncated_start`].
#[derive(Debug)]
pub enum ExportError {
    /// `format: Bin` combined with a filter — rejected outright, never
    /// silently ignored (the wiki: "bin 不允許過濾，保證完整性").
    FilterNotAllowedForBin,
    /// The filter's regex failed to compile.
    InvalidPattern(String),
    /// A `from`/`to` wall-clock bound failed to parse as RFC 3339.
    InvalidTimestamp(String),
    /// Unexpected I/O failure reading the recorder's segments.
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::FilterNotAllowedForBin => {
                write!(f, "--filter is not allowed with the bin format (it would silently break byte-exactness)")
            }
            ExportError::InvalidPattern(msg) => write!(f, "invalid filter pattern: {msg}"),
            ExportError::InvalidTimestamp(msg) => write!(f, "invalid timestamp: {msg}"),
            ExportError::Io(e) => write!(f, "export I/O error: {e}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// Result of a successful [`export_range`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportResult {
    pub bytes: Vec<u8>,
    pub format: ExportFormat,
    pub record_count: usize,
    /// Highest `seq` actually included, if any.
    pub last_seq: Option<u64>,
    /// `Some(oldest_available_seq)` when the requested start had to be
    /// clamped forward because ring eviction already unlinked earlier data
    /// — surfaced so a caller can warn, rather than silently returning a
    /// result that looks like a complete export but isn't (the wiki:
    /// "產生警告與截斷結果，不靜默").
    pub truncated_start: Option<u64>,
}

/// Render `recorder`'s stream for `range` as `format`, applying `filter`
/// (see the module docs for its exact semantics). This is the one function
/// both the CLI (over the wire, via `Request::Export`) and any future
/// in-process caller (an embedded GUI backend) call — see the module docs.
pub fn export_range(
    recorder: &Recorder,
    range: &ExportRange,
    format: ExportFormat,
    filter: Option<&Filter>,
) -> Result<ExportResult, ExportError> {
    if format == ExportFormat::Bin && filter.is_some() {
        return Err(ExportError::FilterNotAllowedForBin);
    }
    let compiled = compile_filter(filter)?;

    let from_seq_hint = match &range.from {
        Some(ExportBound::Seq(s)) => *s,
        _ => 0,
    };
    let to_seq_hint = match &range.to {
        Some(ExportBound::Seq(s)) => Some(*s),
        _ => None,
    };
    let from_wall = match &range.from {
        Some(ExportBound::Wall(s)) => Some(parse_wall(s)?),
        _ => None,
    };
    let to_wall = match &range.to {
        Some(ExportBound::Wall(s)) => Some(parse_wall(s)?),
        _ => None,
    };

    let (mut records, truncated_start) = collect_range(recorder, from_seq_hint, to_seq_hint)?;

    if let Some(from_wall) = from_wall {
        records.retain(|r| wall_at_or_after(r.t_wall(), from_wall));
    }
    if let Some(to_wall) = to_wall {
        records.retain(|r| wall_at_or_before(r.t_wall(), to_wall));
    }

    let records = apply_filter(records, compiled.as_ref());
    let last_seq = records.last().map(Record::seq);
    let record_count = records.len();
    let bytes = match format {
        ExportFormat::Jsonl => render_jsonl(&records),
        ExportFormat::Bin => render_bin(&records),
        ExportFormat::Txt => render_txt(&records),
    };

    Ok(ExportResult {
        bytes,
        format,
        record_count,
        last_seq,
        truncated_start,
    })
}

fn parse_wall(s: &str) -> Result<DateTime<Utc>, ExportError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| ExportError::InvalidTimestamp(format!("{s:?}: {e}")))
}

/// `true` if `t_wall` fails to parse — matches `cli::time::passes_since`'s
/// stance: never hide a record from an export because of our own parsing
/// trouble, not the device's.
fn wall_at_or_after(t_wall: &str, threshold: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(t_wall) {
        Ok(dt) => dt.with_timezone(&Utc) >= threshold,
        Err(_) => true,
    }
}

fn wall_at_or_before(t_wall: &str, threshold: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(t_wall) {
        Ok(dt) => dt.with_timezone(&Utc) <= threshold,
        Err(_) => true,
    }
}

/// Page through `recorder.read_since` from `from_seq` up to (and including)
/// `to_seq` if given, otherwise up to whatever is on disk right now.
/// Returns the collected records plus `Some(oldest_available_seq)` if
/// `from_seq` had already aged out of the ring — clamped forward to that
/// floor rather than erroring, per the module docs.
fn collect_range(
    recorder: &Recorder,
    from_seq: u64,
    to_seq: Option<u64>,
) -> Result<(Vec<Record>, Option<u64>), ExportError> {
    let mut cursor = from_seq;
    let mut truncated_start = None;

    let mut page = match recorder.read_since(cursor, PAGE_BYTES) {
        Ok(page) => page,
        Err(ReadSinceError::DataAgedOut {
            oldest_available_seq,
        }) => {
            truncated_start = Some(oldest_available_seq);
            cursor = oldest_available_seq;
            match recorder.read_since(cursor, PAGE_BYTES) {
                Ok(page) => page,
                Err(ReadSinceError::DataAgedOut {
                    oldest_available_seq,
                }) => {
                    // Unreachable in practice: `cursor` was just resynced to
                    // the recorder's own reported floor. Handled rather than
                    // unwrapped so this function stays panic-free regardless.
                    return Err(ExportError::Io(std::io::Error::other(format!(
                        "DataAgedOut again immediately after resyncing to the reported floor \
                         {oldest_available_seq}"
                    ))));
                }
                Err(ReadSinceError::Io(e)) => return Err(ExportError::Io(e)),
            }
        }
        Err(ReadSinceError::Io(e)) => return Err(ExportError::Io(e)),
    };

    let mut records = Vec::new();
    loop {
        if page.records.is_empty() {
            break;
        }
        let mut hit_to_seq = false;
        for record in page.records {
            if let Some(to_seq) = to_seq {
                if record.seq() > to_seq {
                    hit_to_seq = true;
                    break;
                }
            }
            records.push(record);
        }
        if hit_to_seq || page.next_cursor <= cursor {
            break;
        }
        cursor = page.next_cursor;
        page = match recorder.read_since(cursor, PAGE_BYTES) {
            Ok(p) => p,
            Err(ReadSinceError::DataAgedOut {
                oldest_available_seq,
            }) => {
                // Eviction only ever removes segments older than anything a
                // forward-paging scan has already read past, so this should
                // be unreachable — surfaced as an error rather than
                // silently truncating differently from the documented
                // "aged out at the start" case.
                return Err(ExportError::Io(std::io::Error::other(format!(
                    "unexpected DataAgedOut while paging forward past seq {cursor} \
                     (oldest_available_seq={oldest_available_seq})"
                ))));
            }
            Err(ReadSinceError::Io(e)) => return Err(ExportError::Io(e)),
        };
    }
    Ok((records, truncated_start))
}

struct CompiledFilter {
    re: Regex,
    exclude: bool,
}

fn compile_filter(filter: Option<&Filter>) -> Result<Option<CompiledFilter>, ExportError> {
    match filter {
        None => Ok(None),
        Some(f) => {
            let re =
                Regex::new(&f.pattern).map_err(|e| ExportError::InvalidPattern(e.to_string()))?;
            Ok(Some(CompiledFilter {
                re,
                exclude: f.exclude,
            }))
        }
    }
}

/// Keep `rx` records whose decoded (lossy) bytes match `filter`; every
/// other kind (`tx`/`event`/`gate`) always passes through untouched — see
/// the module docs.
fn apply_filter(records: Vec<Record>, filter: Option<&CompiledFilter>) -> Vec<Record> {
    let Some(filter) = filter else {
        return records;
    };
    records
        .into_iter()
        .filter(|r| match r {
            Record::Rx { data_b64, .. } => {
                let bytes = BASE64.decode(data_b64).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes);
                filter.re.is_match(&text) != filter.exclude
            }
            _ => true,
        })
        .collect()
}

/// `jsonl`: every record verbatim, one JSON object per line, in the same
/// ascending-`seq` order they were recorded — "stream as stored".
fn render_jsonl(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        if let Ok(line) = serde_json::to_vec(r) {
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
    }
    out
}

/// `bin`: only `rx` payloads, decoded and concatenated in `seq` order —
/// every other record kind is entirely absent, not even a marker byte.
fn render_bin(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        if let Record::Rx { data_b64, .. } = r {
            if let Ok(bytes) = BASE64.decode(data_b64) {
                out.extend_from_slice(&bytes);
            }
        }
    }
    out
}

fn is_plain_text(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(s) => !s.chars().any(|c| c.is_control() && c != '\t'),
        Err(_) => false,
    }
}

fn hex_preview(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes
        .iter()
        .take(TXT_HEX_PREVIEW_BYTES)
        .map(|b| format!("{b:02x}"))
        .collect();
    let joined = hex.join(" ");
    if bytes.len() > TXT_HEX_PREVIEW_BYTES {
        format!("{joined}...")
    } else {
        joined
    }
}

/// Render one rx/tx payload's content for a `txt` row: plain lossy text
/// when it's safe to show as-is, otherwise a `[N bytes binary] <hex>`
/// annotation — the wiki's own literal example shape.
fn render_content(bytes: &[u8]) -> String {
    if is_plain_text(bytes) {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        format!("[{} bytes binary] {}", bytes.len(), hex_preview(bytes))
    }
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn sorted_extras(extra: &Map<String, serde_json::Value>) -> String {
    let mut kvs: Vec<(String, String)> =
        extra.iter().map(|(k, v)| (k.clone(), scalar(v))).collect();
    kvs.sort();
    kvs.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn client_type_str(t: ClientType) -> &'static str {
    match t {
        ClientType::Human => "human",
        ClientType::Agent => "agent",
        ClientType::Tool => "tool",
    }
}

/// One line assembled from `rx` bytes across records, for `txt` only
/// (`jsonl`/`bin` stay record-level — see the module docs). Mirrors
/// `query::AssembledLine`'s byte-level algorithm (split on `\n`, strip a
/// preceding `\r`) as a plain batch pass over an already-collected
/// `&[Record]` slice.
struct TxtLine {
    seq: u64,
    t_wall: String,
    raw: Vec<u8>,
}

fn assemble_txt_lines(records: &[Record]) -> Vec<TxtLine> {
    let mut lines = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut partial_pos: Option<(u64, String)> = None;
    for r in records {
        let Record::Rx {
            seq,
            t_wall,
            data_b64,
            ..
        } = r
        else {
            continue;
        };
        let Ok(bytes) = BASE64.decode(data_b64) else {
            continue;
        };
        // Assemble with the same rule the live query layer uses — both `\r`
        // and `\n` terminate, `\r\n` counts once, empty stretches dropped.
        // This used to be a second, `\n`-only implementation, which is why a
        // CR-terminated device's `txt` export came out as hex dumps while
        // `serialwrap tail` showed it fine.
        partial.extend_from_slice(&bytes);
        let (completed, remainder) = crate::query::Terminator::Any.split(&partial);
        partial = remainder;
        for raw in completed {
            lines.push(TxtLine {
                seq: *seq,
                t_wall: t_wall.clone(),
                raw,
            });
        }
        partial_pos = if partial.is_empty() {
            None
        } else {
            Some((*seq, t_wall.clone()))
        };
    }
    // A trailing, never-newline-terminated tail: unlike the live query
    // layer (which deliberately holds this back — see `query.rs` — since
    // more bytes might still complete it), an export is a bounded snapshot
    // with nothing more coming, so surfacing it is the only way not to
    // silently drop the last thing the device said before the cutoff.
    if !partial.is_empty() {
        if let Some((seq, t_wall)) = partial_pos {
            lines.push(TxtLine {
                seq,
                t_wall,
                raw: partial,
            });
        }
    }
    lines
}

/// `txt`: `<t_wall> <content>` per assembled device line; `event`/`gate`/
/// `tx` rows prefixed `# ` (mirroring `cli::render`'s live-tail convention,
/// reimplemented independently here — see the module docs); a binary
/// (non-terminal-safe) payload renders as `[N bytes binary] <hex>` instead
/// of raw lossy text.
fn render_txt(records: &[Record]) -> Vec<u8> {
    let lines = assemble_txt_lines(records);
    let mut rows: Vec<(u64, String)> = Vec::with_capacity(lines.len() + records.len());
    for l in &lines {
        rows.push((l.seq, format!("{} {}", l.t_wall, render_content(&l.raw))));
    }
    for r in records {
        match r {
            Record::Rx { .. } => {}
            Record::Event {
                seq,
                t_wall,
                event,
                extra,
                ..
            } => {
                let extras = sorted_extras(extra);
                let row = if extras.is_empty() {
                    format!("# {t_wall} {event}")
                } else {
                    format!("# {t_wall} {event} {extras}")
                };
                rows.push((*seq, row));
            }
            Record::Gate {
                seq,
                t_wall,
                action,
                reason,
                request_seq,
                ..
            } => {
                rows.push((
                    *seq,
                    format!(
                        "# {t_wall} gate action={action} reason={reason} request_seq={request_seq}"
                    ),
                ));
            }
            Record::Tx {
                seq,
                t_wall,
                client,
                client_type,
                gate,
                data_b64,
                ..
            } => {
                let bytes = BASE64.decode(data_b64).unwrap_or_default();
                rows.push((
                    *seq,
                    format!(
                        "# {t_wall} tx client={client} client_type={} gate={gate} {}",
                        client_type_str(*client_type),
                        render_content(&bytes)
                    ),
                ));
            }
        }
    }
    // Stable sort: ties only occur between multiple lines completed by the
    // *same* rx record (one chunk containing several `\n`s), which are
    // already pushed in left-to-right order above — every other record
    // kind has a globally unique `seq` (recorder.rs allocates one counter
    // per device across every record kind), so no other tie is possible.
    rows.sort_by_key(|(seq, _)| *seq);
    let mut out = String::new();
    for (_, row) in rows {
        out.push_str(&row);
        out.push('\n');
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::RecorderConfig;
    use serde_json::Map as JsonMap;
    use sha2::{Digest, Sha256};

    fn recorder(dir: &std::path::Path) -> Recorder {
        Recorder::open(dir, "dev", RecorderConfig::default()).expect("open recorder")
    }

    fn full_range() -> ExportRange {
        ExportRange::default()
    }

    // ---- Acceptance 1: bin byte-exact ----

    #[test]
    fn bin_export_is_byte_exact_including_invalid_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());

        let chunk_a = b"boot ok\n".to_vec();
        let mut chunk_b = b"status: ".to_vec();
        chunk_b.extend_from_slice(&[0xFF, 0xFE, 0x80]); // invalid UTF-8
        chunk_b.extend_from_slice(b"-done\n");
        let chunk_c = b"final line, no newline".to_vec();

        recorder.append_rx(&chunk_a).unwrap();
        // Interleave a tx/event/gate — bin must ignore all of them.
        recorder
            .append_tx(b"ping\n", "human", ClientType::Human, "human_rw")
            .unwrap();
        recorder.append_event("connect", JsonMap::new()).unwrap();
        recorder.append_rx(&chunk_b).unwrap();
        recorder.append_gate("deny", "danger", 1).unwrap();
        recorder.append_rx(&chunk_c).unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Bin, None).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&chunk_a);
        expected.extend_from_slice(&chunk_b);
        expected.extend_from_slice(&chunk_c);
        assert_eq!(result.bytes, expected, "bin export must be byte-exact");

        let expected_hash = format!("{:x}", Sha256::digest(&expected));
        let actual_hash = format!("{:x}", Sha256::digest(&result.bytes));
        assert_eq!(expected_hash, actual_hash);
    }

    #[test]
    fn bin_with_filter_is_rejected_not_silently_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"hello\n").unwrap();

        let filter = Filter {
            pattern: "hello".to_string(),
            exclude: false,
        };
        let err = export_range(&recorder, &full_range(), ExportFormat::Bin, Some(&filter))
            .expect_err("bin + filter must be rejected");
        assert!(matches!(err, ExportError::FilterNotAllowedForBin));
    }

    // ---- Acceptance 2: jsonl round-trip ----

    #[test]
    fn jsonl_export_round_trips_to_the_exact_original_records() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"line one\n").unwrap();
        recorder
            .append_tx(b"cmd\n", "agent:99", ClientType::Agent, "whitelist")
            .unwrap();
        let mut extra = JsonMap::new();
        extra.insert("device_id".to_string(), "usb-1a86".into());
        recorder.append_event("connect", extra).unwrap();
        recorder.append_gate("allow", "whitelist_match", 1).unwrap();
        recorder.append_rx(b"line two\n").unwrap();

        let original = recorder.read_since(0, usize::MAX).unwrap().records;
        let result = export_range(&recorder, &full_range(), ExportFormat::Jsonl, None).unwrap();

        let replayed: Vec<Record> = String::from_utf8(result.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            replayed, original,
            "jsonl export must replay to exactly the original record stream"
        );
        assert_eq!(result.record_count, original.len());
        assert_eq!(result.last_seq, original.last().map(Record::seq));
    }

    // ---- Acceptance 3: txt format shapes ----

    #[test]
    fn txt_renders_plain_data_rows_without_a_hash_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"Temp: 25.7 C\n").unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        let line = text.lines().next().unwrap();
        assert!(!line.starts_with('#'), "line was: {line}");
        assert!(line.ends_with("Temp: 25.7 C"), "line was: {line}");
    }

    #[test]
    fn txt_renders_event_rows_hash_prefixed_with_sorted_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let mut extra = JsonMap::new();
        extra.insert("device_id".to_string(), "usb-1a86".into());
        recorder.append_event("connect", extra).unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        let line = text.lines().next().unwrap();
        assert!(line.starts_with("# "), "line was: {line}");
        assert!(line.contains("connect"), "line was: {line}");
        assert!(line.contains("device_id=usb-1a86"), "line was: {line}");
    }

    #[test]
    fn txt_renders_gate_rows_with_action_reason_and_request_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"trigger\n").unwrap();
        recorder.append_gate("deny", "danger:erase", 0).unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        let gate_line = text
            .lines()
            .find(|l| l.contains("gate"))
            .expect("expected a gate row");
        assert!(gate_line.starts_with("# "), "line was: {gate_line}");
        assert!(gate_line.contains("action=deny"), "line was: {gate_line}");
        assert!(
            gate_line.contains("reason=danger:erase"),
            "line was: {gate_line}"
        );
        assert!(gate_line.contains("request_seq=0"), "line was: {gate_line}");
    }

    #[test]
    fn txt_annotates_binary_rx_content_with_length_and_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let mut payload = vec![0xFFu8, 0xFE, 0xFD, 0x01];
        payload.push(b'\n');
        recorder.append_rx(&payload).unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        let line = text.lines().next().unwrap();
        assert!(line.contains("[4 bytes binary]"), "line was: {line:?}");
        assert!(line.contains("ff fe fd 01"), "line was: {line:?}");
        assert!(!line.contains('\u{FFFD}'), "line was: {line:?}");
    }

    #[test]
    fn txt_surfaces_a_trailing_unterminated_line_unlike_the_live_query_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"no newline here").unwrap();

        let result = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        assert!(
            text.contains("no newline here"),
            "trailing partial data must not be silently dropped from an export: {text:?}"
        );
    }

    // ---- Acceptance 4 (covered above too): filter semantics ----

    #[test]
    fn filter_drops_non_matching_rx_but_keeps_tx_event_gate_always() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"keep me\n").unwrap();
        recorder.append_rx(b"drop me\n").unwrap();
        recorder
            .append_tx(b"cmd\n", "human", ClientType::Human, "human_rw")
            .unwrap();
        recorder.append_event("disconnect", JsonMap::new()).unwrap();

        let filter = Filter {
            pattern: "keep".to_string(),
            exclude: false,
        };
        // `txt` (not `jsonl`) for this content assertion: `jsonl`
        // base64-encodes `rx` payloads, so a literal "keep me"/"drop me"
        // substring check would never match either way — see the module
        // docs on why `jsonl` stays record-level.
        let result =
            export_range(&recorder, &full_range(), ExportFormat::Txt, Some(&filter)).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        assert!(text.contains("keep me"), "text: {text}");
        assert!(!text.contains("drop me"), "text: {text}");
        assert!(text.contains("tx client=human"), "text: {text}");
        assert!(text.contains("disconnect"), "text: {text}");
    }

    // ---- --from/--to wall-clock bounds ----

    #[test]
    fn wall_time_from_bound_excludes_records_recorded_before_it() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"early\n").unwrap();
        // `t_wall` only carries millisecond precision, so appends within
        // the same millisecond would tie under a wall-clock bound — a
        // short sleep keeps this test's boundary genuinely distinct rather
        // than flaky depending on how fast the test happens to run.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let marker = recorder.append_rx(b"the boundary itself\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        recorder.append_rx(b"late\n").unwrap();

        let range = ExportRange {
            from: Some(ExportBound::Wall(marker.t_wall().to_string())),
            to: None,
        };
        // `txt`, not `jsonl` — see the comment on the filter test above.
        let result = export_range(&recorder, &range, ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        assert!(!text.contains("early"), "text: {text}");
        assert!(text.contains("the boundary itself"), "text: {text}");
        assert!(text.contains("late"), "text: {text}");
    }

    #[test]
    fn wall_time_to_bound_excludes_records_recorded_after_it() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        recorder.append_rx(b"early\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let marker = recorder.append_rx(b"the boundary itself\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        recorder.append_rx(b"late\n").unwrap();

        let range = ExportRange {
            from: None,
            to: Some(ExportBound::Wall(marker.t_wall().to_string())),
        };
        let result = export_range(&recorder, &range, ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(result.bytes).unwrap();
        assert!(text.contains("early"), "text: {text}");
        assert!(text.contains("the boundary itself"), "text: {text}");
        assert!(!text.contains("late"), "text: {text}");
    }

    // ---- Acceptance 6: segment boundary correctness ----

    #[test]
    fn export_across_segment_boundaries_has_no_duplicates_or_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        // `segment_bytes`/record count tuned to produce a handful of
        // segments (not dozens): `Recorder::rotate_segment` does a real
        // `sync_data()` syscall on every rotation regardless of
        // `fsync_interval`, so too many rotations makes this test needlessly
        // slow without adding coverage beyond "crosses at least one
        // boundary".
        let config = RecorderConfig {
            segment_bytes: 500,
            ring_bytes: u64::MAX,
            checkpoint_every: 3,
            checkpoint_bytes: 100,
            fsync_interval: std::time::Duration::from_secs(3600),
        };
        let recorder = Recorder::open(tmp.path(), "dev", config).unwrap();
        for i in 0..80u64 {
            recorder
                .append_rx(format!("payload-{i:04}\n").as_bytes())
                .unwrap();
        }

        let result = export_range(&recorder, &full_range(), ExportFormat::Jsonl, None).unwrap();
        let seqs: Vec<u64> = String::from_utf8(result.bytes)
            .unwrap()
            .lines()
            .map(|line| {
                let record: Record = serde_json::from_str(line).unwrap();
                record.seq()
            })
            .collect();
        assert_eq!(seqs, (0..80).collect::<Vec<_>>());
    }

    #[test]
    fn export_range_respects_an_explicit_seq_upper_bound_across_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            segment_bytes: 500,
            ring_bytes: u64::MAX,
            checkpoint_every: 3,
            checkpoint_bytes: 100,
            fsync_interval: std::time::Duration::from_secs(3600),
        };
        let recorder = Recorder::open(tmp.path(), "dev", config).unwrap();
        for i in 0..80u64 {
            recorder
                .append_rx(format!("payload-{i:04}\n").as_bytes())
                .unwrap();
        }

        let range = ExportRange {
            from: Some(ExportBound::Seq(20)),
            to: Some(ExportBound::Seq(60)),
        };
        let result = export_range(&recorder, &range, ExportFormat::Jsonl, None).unwrap();
        let seqs: Vec<u64> = String::from_utf8(result.bytes)
            .unwrap()
            .lines()
            .map(|line| {
                let record: Record = serde_json::from_str(line).unwrap();
                record.seq()
            })
            .collect();
        assert_eq!(seqs, (20..=60).collect::<Vec<_>>());
    }

    // ---- Acceptance 7: aged-out ranges warn and truncate, never silent ----

    #[test]
    fn export_from_an_aged_out_start_clamps_and_reports_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        // Same rotation-cost reasoning as the segment-boundary tests above:
        // a bigger `segment_bytes` for the same eviction guarantee keeps
        // this test's real `sync_data()` count (one per rotation) small.
        let config = RecorderConfig {
            segment_bytes: 500,
            ring_bytes: 1500,
            checkpoint_every: 3,
            checkpoint_bytes: 100,
            fsync_interval: std::time::Duration::from_secs(3600),
        };
        let recorder = Recorder::open(tmp.path(), "dev", config).unwrap();
        for i in 0..80u64 {
            recorder
                .append_rx(format!("payload-{i:04}\n").as_bytes())
                .unwrap();
        }

        // Ask for the entire history from seq 0 — the ring has certainly
        // evicted some of it by now (small ring_bytes above).
        let range = ExportRange {
            from: Some(ExportBound::Seq(0)),
            to: None,
        };
        let result = export_range(&recorder, &range, ExportFormat::Jsonl, None).unwrap();

        let oldest = result
            .truncated_start
            .expect("export must report the truncation, never silently succeed");
        assert!(oldest > 0, "oldest available seq should have advanced");
        assert!(
            !result.bytes.is_empty(),
            "surviving records must still be exported, not an empty file"
        );
        let first_seq: u64 = String::from_utf8(result.bytes)
            .unwrap()
            .lines()
            .next()
            .map(|line| {
                let record: Record = serde_json::from_str(line).unwrap();
                record.seq()
            })
            .unwrap();
        assert_eq!(first_seq, oldest);
        assert_eq!(result.last_seq, Some(79));
    }

    // ---- Acceptance 8 / integration: S5 (binary + events, all 3 formats) ----

    #[test]
    fn s5_binary_plus_events_stream_all_three_formats_hold_their_guarantees() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());

        recorder.append_rx(b"boot ok\n").unwrap();
        recorder.append_event("connect", JsonMap::new()).unwrap();
        let mut binary_payload = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        binary_payload.push(b'\n');
        recorder.append_rx(&binary_payload).unwrap();
        recorder
            .append_tx(b"cmd\n", "agent:1", ClientType::Agent, "whitelist")
            .unwrap();
        recorder.append_gate("allow", "whitelist_match", 2).unwrap();
        recorder.append_rx(b"after\n").unwrap();

        let all_records = recorder.read_since(0, usize::MAX).unwrap().records;

        // bin: byte-exact, rx-only.
        let bin = export_range(&recorder, &full_range(), ExportFormat::Bin, None).unwrap();
        let mut expected_bin = Vec::new();
        for r in &all_records {
            if let Record::Rx { data_b64, .. } = r {
                expected_bin.extend_from_slice(&BASE64.decode(data_b64).unwrap());
            }
        }
        assert_eq!(bin.bytes, expected_bin);

        // jsonl: lossless round trip of every record, including the binary one.
        let jsonl = export_range(&recorder, &full_range(), ExportFormat::Jsonl, None).unwrap();
        let replayed: Vec<Record> = String::from_utf8(jsonl.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replayed, all_records);

        // txt: binary annotation present, event/gate/tx rows hash-prefixed.
        let txt = export_range(&recorder, &full_range(), ExportFormat::Txt, None).unwrap();
        let text = String::from_utf8(txt.bytes).unwrap();
        assert!(text.contains("[6 bytes binary]"), "text: {text}");
        assert!(text.contains("de ad be ef 00 01"), "text: {text}");
        assert!(
            text.contains("# ") && text.contains("connect"),
            "text: {text}"
        );
        assert!(text.contains("gate action=allow"), "text: {text}");
        assert!(text.contains("tx client=agent:1"), "text: {text}");
    }

    // ---- Acceptance 5: performance (100k rows, ≤5s) ----
    //
    // `#[ignore]`d per the project's own test-suite time budget (`cargo test
    // --all` must stay ~10s; see `TASKS.md`/the PR description) — CI's
    // separate `--ignored` step (`.github/workflows/ci.yml`) runs this.
    #[test]
    #[ignore]
    fn hundred_thousand_row_export_completes_within_five_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        for i in 0..100_000u64 {
            recorder
                .append_rx(format!("line {i:06} of the performance fixture\n").as_bytes())
                .unwrap();
        }

        let start = std::time::Instant::now();
        let result = export_range(&recorder, &full_range(), ExportFormat::Jsonl, None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(result.record_count, 100_000);
        assert!(
            elapsed.as_secs_f64() <= 5.0,
            "100k-row export took {elapsed:?}, budget is 5s"
        );
        eprintln!("100k-row jsonl export took {elapsed:?}");
    }
}
