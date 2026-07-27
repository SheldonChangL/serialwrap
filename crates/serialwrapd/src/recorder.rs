//! Recorder: appends bytes as `rx` (and `tx`/`event`/`gate`) records into
//! append-only JSONL segments, independent of line boundaries.
//!
//! This is the foundation the rest of the project's promises rest on ("you
//! never lose the boot log", "every write is traceable", "a human and an
//! agent referencing the same `seq` point at the same line"). See the
//! wiki's [Event stream and
//! storage](https://github.com/SheldonChangL/serialwrap/wiki/Event-stream-and-storage)
//! page for the authoritative on-disk schema this module implements, and
//! `TASKS.md` T1.2 for the task this closes.
//!
//! # Storage layout
//!
//! ```text
//! <data_dir>/devices/<device_id>/
//!   segments/000000000000.jsonl   (filename = starting seq, zero-padded)
//!   segments/000000524288.jsonl
//!   index.jsonl                   (seq -> (segment, byte offset) checkpoints)
//! ```
//!
//! # Write semantics
//!
//! - `seq` is per-device, monotonic, and gap-free: it is allocated under the
//!   same lock that performs the append, so concurrent writers can never
//!   observe (or produce) a gap — a gap would be indistinguishable from data
//!   loss.
//! - `t_mono` comes from `clock_gettime(CLOCK_MONOTONIC)` directly (see
//!   [`monotonic_seconds`]) because `std::time::Instant` is an opaque
//!   handle with no stable epoch and cannot be serialized to an absolute
//!   seconds value.
//! - `t_wall` is RFC 3339 with a numeric UTC offset and millisecond
//!   precision, e.g. `2026-07-27T10:34:12.443+08:00`.
//! - `rx` payloads are raw chunks as read from the device: no line assembly,
//!   no re-encoding, no line-ending normalization. A half-line held in
//!   memory waiting for its newline is exactly the half-line a crash would
//!   lose, and the last few lines before a crash are usually the ones worth
//!   finding. Line assembly is a query-layer concern (T1.4) that can be
//!   redone from the stored bytes at any time; the recorder cannot redo a
//!   write it never made.
//! - Segments are capped at 64 MiB (default, configurable); a per-device
//!   ring budget (2 GiB default, configurable) evicts whole segments
//!   (`unlink`, never partial truncation) oldest-first.
//! - Durability: an fsync runs once per second (configurable); data loss on
//!   an OS/power-level failure is bounded by that window, not by how the
//!   process happens to die (a `kill -9` alone doesn't lose page-cache
//!   writes — see the recovery tests for what's actually being verified).
//!
//! # Startup recovery
//!
//! Opening a device replays its newest segment's last line. If it fails to
//! parse, it was a partial write; the file is truncated to drop it and a
//! `recovery` event is appended recording how many bytes were discarded.
//! Everything before that line is guaranteed intact by the format (each
//! record is one self-contained JSON object per line).
//!
//! # Query interface (minimal, for T1.4 to build on)
//!
//! [`Recorder::read_since`] returns records after a cursor plus the next
//! cursor to use. If the cursor points into a range ring eviction has
//! already unlinked, it returns [`ReadSinceError::DataAgedOut`] carrying the
//! oldest sequence number still available — never an empty result, which
//! would be indistinguishable from "you missed 40 minutes". Line assembly,
//! filtering, collapsing, and result bounding belong to the query layer
//! (T1.4/T3.2) and are deliberately not done here.
//!
//! # Ingestion seam for later tasks
//!
//! [`Recorder::append_rx`]/[`append_tx`]/[`append_event`]/[`append_gate`]
//! are the intended "event source" boundary: T1.1's device-detection loop,
//! T1.3's port-config-change events, and T4.1's gate decisions all just call
//! the relevant `append_*` method with their payload. `Recorder` is
//! `Send + Sync` and every `append_*` method takes `&self`, so any of those
//! call sites (sync, async, or a background thread) can hold a shared
//! `Recorder` and call in directly without needing a channel or a trait
//! object — that thin, already-thread-safe method surface *is* the
//! abstraction; nothing device-specific is implemented here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::Map;
use wrap_proto::{ClientType, ErrorCode, Record};

/// Default per-segment size cap: 64 MiB.
pub const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Default per-device ring budget: 2 GiB.
pub const DEFAULT_RING_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default index checkpoint cadence: one checkpoint every this many records.
pub const DEFAULT_CHECKPOINT_EVERY: u64 = 4096;
/// Default index checkpoint cadence, by bytes: a checkpoint is also forced
/// after this many bytes even if `checkpoint_every` records haven't been
/// reached yet. Large records (e.g. 64KB rx chunks) would otherwise make
/// the record-count trigger effectively never fire within one 64MB
/// segment, leaving `read_since` to rescan a whole segment from offset 0
/// for any cursor not exactly at a segment boundary.
pub const DEFAULT_CHECKPOINT_BYTES: u64 = 1024 * 1024;
/// Default fsync cadence.
pub const DEFAULT_FSYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Segment filenames are the starting `seq` zero-padded to this width.
const SEGMENT_FILENAME_WIDTH: usize = 12;

/// Tunable knobs for a [`Recorder`].
///
/// All fields have production defaults ([`RecorderConfig::default`]) but
/// every one is overridable so tests can use tiny values (e.g. a few
/// hundred bytes per segment) to exercise rotation/eviction without
/// generating gigabytes of data.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Roll to a new segment file once the current one would exceed this
    /// many bytes.
    pub segment_bytes: u64,
    /// Evict the oldest whole segment (via `unlink`) whenever a device's
    /// total on-disk size exceeds this. The currently-open segment is
    /// never evicted.
    pub ring_bytes: u64,
    /// Write an `index.jsonl` checkpoint every this many records (plus
    /// always at the first record of a new segment).
    pub checkpoint_every: u64,
    /// Also force a checkpoint after this many bytes since the last one,
    /// whichever of the two triggers comes first. Bounds the worst-case
    /// `read_since` in-segment scan distance independent of record size.
    pub checkpoint_bytes: u64,
    /// Minimum interval between `fsync` calls.
    pub fsync_interval: Duration,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            ring_bytes: DEFAULT_RING_BYTES,
            checkpoint_every: DEFAULT_CHECKPOINT_EVERY,
            checkpoint_bytes: DEFAULT_CHECKPOINT_BYTES,
            fsync_interval: DEFAULT_FSYNC_INTERVAL,
        }
    }
}

/// Freshly allocated `seq` + both clocks for one record.
///
/// Handed to the closure passed to [`Recorder::append`]; allocating this and
/// writing the resulting bytes happen under the same lock, which is what
/// keeps `seq` gap-free under concurrent writers.
#[derive(Debug, Clone)]
pub struct Timestamp {
    pub seq: u64,
    pub t_mono: f64,
    pub t_wall: String,
}

/// Error from [`Recorder::read_since`].
#[derive(Debug)]
pub enum ReadSinceError {
    /// `cursor` points at or before data that ring eviction has already
    /// unlinked. Carries the oldest sequence number still on disk so the
    /// caller can resynchronize. Must never be confused with "no new data
    /// yet", which returns an empty `records` list with this error absent.
    DataAgedOut { oldest_available_seq: u64 },
    /// Unexpected I/O failure reading a segment/index file that the
    /// recorder's own bookkeeping says should exist.
    Io(io::Error),
}

impl ReadSinceError {
    /// The wire error code this maps to (see `wrap_proto::ErrorCode`).
    pub fn code(&self) -> ErrorCode {
        match self {
            ReadSinceError::DataAgedOut { .. } => ErrorCode::DataAgedOut,
            ReadSinceError::Io(_) => ErrorCode::Internal,
        }
    }
}

impl From<io::Error> for ReadSinceError {
    fn from(e: io::Error) -> Self {
        ReadSinceError::Io(e)
    }
}

impl std::fmt::Display for ReadSinceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadSinceError::DataAgedOut {
                oldest_available_seq,
            } => write!(
                f,
                "cursor points into evicted data; oldest available seq is {oldest_available_seq}"
            ),
            ReadSinceError::Io(e) => write!(f, "recorder I/O error: {e}"),
        }
    }
}

impl std::error::Error for ReadSinceError {}

/// Result of a successful [`Recorder::read_since`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadSince {
    /// Records with `seq >= cursor` (subject to `max_bytes` bounding), in
    /// ascending seq order.
    pub records: Vec<Record>,
    /// Pass this back as `cursor` to continue exactly where this call left
    /// off. Equal to the input `cursor` when no new records were returned.
    pub next_cursor: u64,
}

#[derive(Debug, Clone)]
struct SegmentMeta {
    start_seq: u64,
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone, Copy)]
struct IndexCheckpoint {
    seq: u64,
    segment_start: u64,
    offset: u64,
}

struct Inner {
    /// Ascending by `start_seq`; the last entry is always the
    /// currently-open-for-write segment.
    segments: Vec<SegmentMeta>,
    next_seq: u64,
    total_bytes: u64,
    current_file: File,
    current_offset: u64,
    records_since_checkpoint: u64,
    bytes_since_checkpoint: u64,
    index_file: File,
    /// Ascending by `seq`. A performance aid only — `read_since` falls back
    /// to scanning a segment from its start if no checkpoint covers it, so
    /// losing entries here (e.g. a checkpoint written for a since-evicted
    /// segment) never affects correctness.
    index: Vec<IndexCheckpoint>,
    last_fsync: Instant,
}

/// Appends the event stream for one device to append-only JSONL segments
/// and answers the minimal `read_since` query needed by tests and by later
/// tasks (T1.4 builds the real UDS-facing query layer on top of this).
pub struct Recorder {
    segments_dir: PathBuf,
    config: RecorderConfig,
    inner: Mutex<Inner>,
    /// Held for its `Drop` side effect only (releases the `flock` — see
    /// [`acquire_exclusive_lock`]): never read again after `open()`.
    #[allow(dead_code)]
    lock_file: File,
}

impl Recorder {
    /// Open (creating if necessary) the recorder for `device_id` under
    /// `data_dir`. Runs startup recovery on the newest segment before
    /// returning: see the module docs' "Startup recovery" section.
    ///
    /// `data_dir` is caller-supplied on purpose — production code should
    /// resolve it via [`default_data_dir`]; tests must always pass an
    /// explicit tempdir so they never touch the user's real data directory.
    pub fn open(
        data_dir: impl AsRef<Path>,
        device_id: impl AsRef<str>,
        config: RecorderConfig,
    ) -> io::Result<Self> {
        let device_id = device_id.as_ref();
        if device_id.is_empty() || device_id.contains('/') || device_id == "." || device_id == ".."
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid device id: {device_id:?}"),
            ));
        }
        let device_dir = data_dir.as_ref().join("devices").join(device_id);
        let segments_dir = device_dir.join("segments");
        fs::create_dir_all(&segments_dir)?;

        // Exclusive per-device lock: two `Recorder`s writing the same
        // device concurrently (e.g. a stale daemon still running plus a
        // freshly started one) would each allocate their own `next_seq`
        // and interleave appends into the same segment file, producing
        // duplicate seqs and corrupted lines that no amount of recovery
        // logic can untangle. Held for this `Recorder`'s whole lifetime;
        // released automatically (by the OS) when its fd closes on drop.
        let lock_file = acquire_exclusive_lock(&device_dir)?;

        let mut segments = scan_segments(&segments_dir)?;
        let mut discarded_bytes = 0u64;
        if let Some(newest) = segments.last_mut() {
            let outcome = recover_segment(&newest.path)?;
            discarded_bytes = outcome.discarded_bytes;
            newest.size = fs::metadata(&newest.path)?.len();
        }
        if segments.is_empty() {
            segments.push(SegmentMeta {
                start_seq: 0,
                path: segment_path(&segments_dir, 0),
                size: 0,
            });
        }

        let total_bytes = segments.iter().map(|s| s.size).sum();
        let newest = segments.last().expect("just ensured non-empty above");
        let next_seq = last_seq_in_segment(&newest.path)?
            .map(|s| s + 1)
            .unwrap_or(newest.start_seq);
        let current_offset = newest.size;
        let current_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&newest.path)?;

        let index_path = device_dir.join("index.jsonl");
        let index = load_index(&index_path);
        let index_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?;

        let inner = Inner {
            segments,
            next_seq,
            total_bytes,
            current_file,
            current_offset,
            records_since_checkpoint: 0,
            bytes_since_checkpoint: 0,
            index_file,
            index,
            last_fsync: Instant::now(),
        };

        let recorder = Recorder {
            segments_dir,
            config,
            inner: Mutex::new(inner),
            lock_file,
        };

        if discarded_bytes > 0 {
            let mut extra = Map::new();
            extra.insert("discarded_bytes".to_string(), discarded_bytes.into());
            recorder.append_event("recovery", extra)?;
        }

        Ok(recorder)
    }

    /// [`Recorder::open`] against the production data directory
    /// ([`default_data_dir`]). Never used by tests.
    pub fn open_default(device_id: impl AsRef<str>, config: RecorderConfig) -> io::Result<Self> {
        Self::open(default_data_dir()?, device_id, config)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Allocate the next `seq` + both clocks, hand them to `build` to
    /// produce a fully-formed [`Record`], and append it. `build` must
    /// stamp the record with exactly the given [`Timestamp`]; allocation
    /// and the write happen under the same lock so `seq` stays gap-free
    /// even under concurrent callers.
    pub fn append(&self, build: impl FnOnce(Timestamp) -> Record) -> io::Result<Record> {
        let mut inner = self.lock();
        let seq = inner.next_seq;
        let ts = Timestamp {
            seq,
            t_mono: monotonic_seconds(),
            t_wall: rfc3339_now(),
        };
        let record = build(ts);
        assert_eq!(
            record.seq(),
            seq,
            "Recorder::append closure must stamp the allocated seq \
             (allocated {seq}, got {})",
            record.seq()
        );
        self.append_locked(&mut inner, record)
    }

    /// Append raw bytes read from the device as an `rx` record. Not line
    /// assembled, not re-encoded, not newline-normalized — see the module
    /// docs.
    pub fn append_rx(&self, data: &[u8]) -> io::Result<Record> {
        let data_b64 = BASE64.encode(data);
        self.append(|ts| Record::Rx {
            seq: ts.seq,
            t_mono: ts.t_mono,
            t_wall: ts.t_wall,
            data_b64,
        })
    }

    /// Append bytes written to the device by `client`, subject to the
    /// write gate's decision (`gate`, e.g. `"whitelist"`/`"human_rw"`/
    /// `"approved"` per the wiki).
    pub fn append_tx(
        &self,
        data: &[u8],
        client: impl Into<String>,
        client_type: ClientType,
        gate: impl Into<String>,
    ) -> io::Result<Record> {
        let data_b64 = BASE64.encode(data);
        let client = client.into();
        let gate = gate.into();
        self.append(|ts| Record::Tx {
            seq: ts.seq,
            t_mono: ts.t_mono,
            t_wall: ts.t_wall,
            client,
            client_type,
            gate,
            data_b64,
        })
    }

    /// Append an out-of-band event (`connect`/`disconnect`/`lease_start`/
    /// `recovery`/...). `extra` carries whatever fields that `event` kind
    /// needs (see the wiki's per-subtype field list).
    pub fn append_event(
        &self,
        event: impl Into<String>,
        extra: Map<String, serde_json::Value>,
    ) -> io::Result<Record> {
        let event = event.into();
        self.append(|ts| Record::Event {
            seq: ts.seq,
            t_mono: ts.t_mono,
            t_wall: ts.t_wall,
            event,
            extra,
        })
    }

    /// Append a write-gate decision (`request`/`allow`/`deny`/`approve`).
    pub fn append_gate(
        &self,
        action: impl Into<String>,
        reason: impl Into<String>,
        request_seq: u64,
    ) -> io::Result<Record> {
        let action = action.into();
        let reason = reason.into();
        self.append(|ts| Record::Gate {
            seq: ts.seq,
            t_mono: ts.t_mono,
            t_wall: ts.t_wall,
            action,
            reason,
            request_seq,
        })
    }

    fn append_locked(&self, inner: &mut Inner, record: Record) -> io::Result<Record> {
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');

        // Rotate before writing if this write would push the current
        // segment over its cap — unless the segment is still empty, in
        // which case always write into it (otherwise one oversized record
        // would rotate forever without ever fitting).
        if inner.current_offset > 0
            && inner.current_offset + line.len() as u64 > self.config.segment_bytes
        {
            self.rotate_segment(inner, record.seq())?;
        }

        let offset_before = inner.current_offset;
        let line_len = line.len() as u64;
        if let Err(write_err) = inner.current_file.write_all(&line) {
            // `write_all` may have written a prefix before hitting a hard
            // error (e.g. ENOSPC partway through) — a regular file has no
            // way to "undo" those bytes. Resync bookkeeping to the file's
            // *real* on-disk length so checkpoint offsets, rotation, and
            // ring accounting don't silently drift from disk truth from
            // here on. If this left a torn trailing line behind, the next
            // restart's recovery finds and discards it exactly like any
            // other unclean shutdown.
            if let Ok(meta) = inner.current_file.metadata() {
                let real_len = meta.len();
                inner.total_bytes = inner
                    .total_bytes
                    .saturating_sub(inner.current_offset)
                    .saturating_add(real_len);
                inner.current_offset = real_len;
                if let Some(last) = inner.segments.last_mut() {
                    last.size = real_len;
                }
            }
            return Err(write_err);
        }
        inner.current_offset += line_len;
        inner.total_bytes += line_len;
        if let Some(last) = inner.segments.last_mut() {
            last.size = inner.current_offset;
        }
        // The record is now durably appended: `next_seq` must advance
        // regardless of what happens below. Checkpointing/fsync/eviction
        // are housekeeping, not part of "was this record recorded" — a
        // failure in any of them must not make this call return `Err` for
        // a record that is, in fact, already on disk (an `Err` here would
        // read as "not recorded" to a caller, inviting a retry that
        // duplicates the payload under a new seq).
        inner.next_seq = record.seq() + 1;

        inner.records_since_checkpoint += 1;
        inner.bytes_since_checkpoint += line_len;
        if inner.records_since_checkpoint >= self.config.checkpoint_every
            || inner.bytes_since_checkpoint >= self.config.checkpoint_bytes
        {
            if let Err(e) = self.write_checkpoint(inner, record.seq(), offset_before) {
                eprintln!(
                    "serialwrapd: recorder index checkpoint failed near seq {} (record itself \
                     is safely on disk; read_since falls back to a full-segment scan near this \
                     point until a later checkpoint succeeds): {e}",
                    record.seq()
                );
            }
            inner.records_since_checkpoint = 0;
            inner.bytes_since_checkpoint = 0;
        }

        if let Err(e) = self.maybe_fsync(inner) {
            eprintln!(
                "serialwrapd: recorder fsync failed after seq {} (record already appended; \
                 durability window may be wider than the configured {:?} until this heals): {e}",
                record.seq(),
                self.config.fsync_interval
            );
        }
        if let Err(e) = self.maybe_evict(inner) {
            eprintln!(
                "serialwrapd: recorder ring eviction failed (device may temporarily exceed its \
                 configured {}-byte ring budget): {e}",
                self.config.ring_bytes
            );
        }

        Ok(record)
    }

    fn rotate_segment(&self, inner: &mut Inner, new_start_seq: u64) -> io::Result<()> {
        // Don't leave the segment we're rolling away from relying solely
        // on the next periodic fsync to become durable. Best-effort: a
        // failure here doesn't prevent writing into the new segment, it
        // just means the outgoing segment's durability is only as good as
        // the last periodic fsync (same risk window as normal operation).
        if let Err(e) = inner.current_file.sync_data() {
            eprintln!(
                "serialwrapd: recorder fsync of the outgoing segment failed during rotation \
                 to seq {new_start_seq}: {e}"
            );
        }

        let path = segment_path(&self.segments_dir, new_start_seq);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        inner.segments.push(SegmentMeta {
            start_seq: new_start_seq,
            path,
            size: 0,
        });
        inner.current_file = file;
        inner.current_offset = 0;
        // Always checkpoint the first record of a new segment, independent
        // of the periodic counter, so cross-segment lookups never have to
        // guess a new segment's start.
        if let Err(e) = self.write_checkpoint(inner, new_start_seq, 0) {
            eprintln!(
                "serialwrapd: recorder index checkpoint failed for new segment {new_start_seq}: {e}"
            );
        }
        inner.records_since_checkpoint = 0;
        inner.bytes_since_checkpoint = 0;
        Ok(())
    }

    fn write_checkpoint(&self, inner: &mut Inner, seq: u64, offset: u64) -> io::Result<()> {
        let segment_start = inner
            .segments
            .last()
            .expect("current segment always present")
            .start_seq;
        let entry = IndexCheckpoint {
            seq,
            segment_start,
            offset,
        };
        let mut line = serde_json::to_vec(&serde_json::json!({
            "seq": entry.seq,
            "segment": entry.segment_start,
            "offset": entry.offset,
        }))?;
        line.push(b'\n');
        inner.index_file.write_all(&line)?;
        inner.index.push(entry);
        Ok(())
    }

    fn maybe_fsync(&self, inner: &mut Inner) -> io::Result<()> {
        if inner.last_fsync.elapsed() >= self.config.fsync_interval {
            inner.current_file.sync_data()?;
            inner.index_file.sync_data()?;
            inner.last_fsync = Instant::now();
        }
        Ok(())
    }

    fn maybe_evict(&self, inner: &mut Inner) -> io::Result<()> {
        // Never evict the last (current, open-for-write) segment.
        while inner.total_bytes > self.config.ring_bytes && inner.segments.len() > 1 {
            // Unlink *before* mutating in-memory bookkeeping: if the
            // unlink fails (permissions, already gone, I/O error), leave
            // `segments`/`total_bytes` untouched so state stays consistent
            // and eviction can simply be retried on the next append,
            // rather than silently losing track of the segment's bytes
            // (which would inflate `total_bytes` forever and collapse the
            // ring toward a single segment).
            let path = inner.segments[0].path.clone();
            fs::remove_file(&path)?;
            let oldest = inner.segments.remove(0);
            inner.total_bytes = inner.total_bytes.saturating_sub(oldest.size);
        }
        Ok(())
    }

    /// Returns records with `seq >= cursor` (ascending), bounded by
    /// `max_bytes` of serialized JSONL (always includes at least one
    /// record if any are available, even if it alone exceeds `max_bytes`),
    /// plus the cursor to pass next to continue exactly where this call
    /// left off.
    ///
    /// If `cursor` is older than the oldest sequence number still on disk
    /// (ring eviction already unlinked it), returns
    /// [`ReadSinceError::DataAgedOut`] with that oldest seq — never an
    /// empty result, which would be indistinguishable from "no new data
    /// yet".
    pub fn read_since(&self, cursor: u64, max_bytes: usize) -> Result<ReadSince, ReadSinceError> {
        let inner = self.lock();

        let oldest_available_seq = inner
            .segments
            .first()
            .map(|s| s.start_seq)
            .unwrap_or(inner.next_seq);
        if cursor < oldest_available_seq {
            return Err(ReadSinceError::DataAgedOut {
                oldest_available_seq,
            });
        }
        if cursor >= inner.next_seq {
            return Ok(ReadSince {
                records: Vec::new(),
                next_cursor: cursor,
            });
        }

        let seg_idx = inner
            .segments
            .partition_point(|s| s.start_seq <= cursor)
            .saturating_sub(1);

        let mut records = Vec::new();
        let mut bytes_used = 0usize;
        let mut next_cursor = cursor;

        'segments: for seg in &inner.segments[seg_idx..] {
            let start_offset = inner
                .index
                .iter()
                .filter(|c| c.segment_start == seg.start_seq && c.seq <= cursor)
                .map(|c| c.offset)
                .max()
                .unwrap_or(0);

            let file = File::open(&seg.path)?;
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(start_offset))?;
            let mut line = Vec::new();
            loop {
                line.clear();
                let n = reader.read_until(b'\n', &mut line)?;
                if n == 0 {
                    break; // EOF of this segment
                }
                let trimmed: &[u8] = if line.last() == Some(&b'\n') {
                    &line[..line.len() - 1]
                } else {
                    &line[..]
                };
                if trimmed.is_empty() {
                    continue;
                }
                let record: Record = match serde_json::from_slice(trimmed) {
                    Ok(r) => r,
                    // Defensive only: startup recovery (which requires a
                    // trailing newline, not just a parseable tail — see
                    // `recover_segment`) guarantees every *stored* line
                    // parses, so this should never trigger for data this
                    // process wrote. Log rather than silently swallow, so
                    // unrelated on-disk corruption (e.g. bit rot) is at
                    // least visible instead of surfacing as an invisible
                    // gap in query results.
                    Err(e) => {
                        eprintln!(
                            "serialwrapd: recorder skipping unparseable line in {:?}: {e}",
                            seg.path
                        );
                        continue;
                    }
                };
                if record.seq() < cursor {
                    continue;
                }
                if !records.is_empty() && bytes_used + line.len() > max_bytes {
                    break 'segments;
                }
                bytes_used += line.len();
                next_cursor = record.seq() + 1;
                records.push(record);
                if bytes_used >= max_bytes {
                    break 'segments;
                }
            }
        }

        Ok(ReadSince {
            records,
            next_cursor,
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Best-effort: a graceful shutdown shouldn't have to wait out the
        // next periodic fsync to make its tail durable.
        let inner = self.lock();
        let _ = inner.current_file.sync_data();
        let _ = inner.index_file.sync_data();
    }
}

/// Resolve the production data directory: `~/.local/share/serialwrap` on
/// Linux, `~/Library/Application Support/serialwrap` on macOS. Tests must
/// never call this — construct [`Recorder::open`] with an explicit tempdir
/// instead.
pub fn default_data_dir() -> io::Result<PathBuf> {
    directories::ProjectDirs::from("", "", "serialwrap")
        .map(|d| d.data_dir().to_path_buf())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not resolve a home directory for the default serialwrap data dir",
            )
        })
}

/// Take an exclusive, non-blocking `flock` on a lock file inside
/// `device_dir`, failing clearly if another process already holds it
/// instead of silently allowing two writers to corrupt the same segments.
fn acquire_exclusive_lock(device_dir: &Path) -> io::Result<File> {
    use std::os::unix::io::AsRawFd;

    let lock_path = device_dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another process already holds the recorder lock for {device_dir:?} — \
                     only one Recorder may write a device's segments at a time"
                ),
            ));
        }
        return Err(err);
    }
    Ok(file)
}

fn segment_path(dir: &Path, start_seq: u64) -> PathBuf {
    dir.join(format!(
        "{start_seq:0width$}.jsonl",
        width = SEGMENT_FILENAME_WIDTH
    ))
}

fn scan_segments(dir: &Path) -> io::Result<Vec<SegmentMeta>> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(start_seq) = stem.parse::<u64>() else {
            continue;
        };
        let size = entry.metadata()?.len();
        segments.push(SegmentMeta {
            start_seq,
            path,
            size,
        });
    }
    segments.sort_by_key(|s| s.start_seq);
    Ok(segments)
}

/// Outcome of validating (and, if needed, truncating) a segment's last
/// line at startup.
struct RecoveryOutcome {
    discarded_bytes: u64,
}

/// Bounds of the final newline-delimited "line" in `bytes`: returns the
/// byte offset it starts at, and whether it was newline-terminated.
fn last_line_bounds(bytes: &[u8]) -> (usize, bool) {
    let has_trailing_newline = bytes.last() == Some(&b'\n');
    let search_end = if has_trailing_newline {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let start = bytes[..search_end]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, has_trailing_newline)
}

fn parse_last_line(bytes: &[u8]) -> Option<Record> {
    if bytes.is_empty() {
        return None;
    }
    let (start, has_trailing_newline) = last_line_bounds(bytes);
    let end = if has_trailing_newline {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let line = &bytes[start..end];
    if line.is_empty() {
        return None;
    }
    serde_json::from_slice(line).ok()
}

/// Validate a segment file's last line, truncating and reporting the
/// discarded byte count if it fails to parse (an incomplete/torn write).
/// Per the wiki: "at startup the last line of the newest segment is
/// parsed; if it fails, it was a partial write and is truncated" — data
/// before that line is guaranteed intact by the one-record-per-line format.
fn recover_segment(path: &Path) -> io::Result<RecoveryOutcome> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(RecoveryOutcome { discarded_bytes: 0 })
        }
        Err(e) => return Err(e),
    };
    if bytes.is_empty() {
        return Ok(RecoveryOutcome { discarded_bytes: 0 });
    }
    let (start, has_trailing_newline) = last_line_bounds(&bytes);
    let end = if has_trailing_newline {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let last_line = &bytes[start..end];
    // A last line must be newline-terminated to count as durably written,
    // even if its content happens to parse: a torn write that lands
    // exactly on a complete record's closing brace but loses the trailing
    // `\n` byte would otherwise be declared clean, and the next append
    // would concatenate directly onto it with no separator — corrupting
    // both records permanently with no error and no `recovery` event.
    let valid = has_trailing_newline
        && !last_line.is_empty()
        && serde_json::from_slice::<Record>(last_line).is_ok();
    if valid {
        return Ok(RecoveryOutcome { discarded_bytes: 0 });
    }

    let discarded = (bytes.len() - start) as u64;
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(start as u64)?;
    Ok(RecoveryOutcome {
        discarded_bytes: discarded,
    })
}

/// The `seq` of the last valid record in `path`, or `None` if the segment
/// has no valid records (missing, empty, or fully discarded by recovery).
/// Must be called *after* [`recover_segment`] has already truncated any
/// torn trailing line, so the file's last line (if any) is always valid.
fn last_seq_in_segment(path: &Path) -> io::Result<Option<u64>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(parse_last_line(&bytes).map(|r| r.seq()))
}

/// Load `index.jsonl` checkpoints, silently skipping any line that fails
/// to parse. The index is a performance aid, not authoritative storage —
/// segments are authoritative — so a damaged tail here only costs a
/// (still-correct) linear scan from a segment's start, never correctness.
fn load_index(path: &Path) -> Vec<IndexCheckpoint> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let (Some(seq), Some(segment_start), Some(offset)) = (
            value.get("seq").and_then(|v| v.as_u64()),
            value.get("segment").and_then(|v| v.as_u64()),
            value.get("offset").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        out.push(IndexCheckpoint {
            seq,
            segment_start,
            offset,
        });
    }
    out
}

/// Seconds from `CLOCK_MONOTONIC`. Deliberately not `std::time::Instant`:
/// `Instant` is an opaque handle with no guaranteed epoch and cannot be
/// serialized to an absolute value another process (or a later run of this
/// one) could compare against — a raw `clock_gettime` call is the only way
/// to get a serializable monotonic-seconds number.
fn monotonic_seconds() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(
        ret,
        0,
        "clock_gettime(CLOCK_MONOTONIC) failed: {}",
        io::Error::last_os_error()
    );
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0
}

/// RFC 3339 wall-clock timestamp with millisecond precision and a numeric
/// UTC offset, e.g. `2026-07-27T10:34:12.443+08:00`.
fn rfc3339_now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small segments, small ring: forces both rotation *and* eviction.
    /// Use only in tests that specifically want eviction to kick in.
    fn tiny_config() -> RecorderConfig {
        RecorderConfig {
            segment_bytes: 300,
            ring_bytes: 900,
            checkpoint_every: 3,
            checkpoint_bytes: 100,
            fsync_interval: Duration::from_millis(0),
        }
    }

    /// Small segments, effectively unbounded ring: forces rotation across
    /// segment boundaries without ever evicting anything, so tests can
    /// assert on the *full* recorded range.
    fn tiny_rotation_config() -> RecorderConfig {
        RecorderConfig {
            segment_bytes: 300,
            ring_bytes: u64::MAX,
            checkpoint_every: 3,
            checkpoint_bytes: 100,
            fsync_interval: Duration::from_millis(0),
        }
    }

    fn open(dir: &Path, device_id: &str, config: RecorderConfig) -> Recorder {
        Recorder::open(dir, device_id, config).expect("open recorder")
    }

    #[test]
    fn appends_are_monotonic_and_gap_free_single_threaded() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        let mut seqs = Vec::new();
        for i in 0..50 {
            let r = recorder.append_rx(format!("line {i}").as_bytes()).unwrap();
            seqs.push(r.seq());
        }
        let expected: Vec<u64> = (0..50).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn concurrent_appends_produce_a_gap_free_contiguous_seq_range() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = std::sync::Arc::new(open(tmp.path(), "dev", tiny_rotation_config()));

        const THREADS: usize = 8;
        const PER_THREAD: usize = 300;
        let collected: std::sync::Arc<Mutex<Vec<u64>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let recorder = std::sync::Arc::clone(&recorder);
            let collected = std::sync::Arc::clone(&collected);
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::with_capacity(PER_THREAD);
                for i in 0..PER_THREAD {
                    let r = recorder.append_rx(format!("t{t}-{i}").as_bytes()).unwrap();
                    local.push(r.seq());
                }
                collected.lock().unwrap().extend(local);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // The seq every append() call actually returned must itself be a
        // gap-free, duplicate-free contiguous range — this is the core
        // invariant (allocation + write under the same lock), independent
        // of read-back or eviction.
        let mut seqs = collected.lock().unwrap().clone();
        seqs.sort_unstable();
        let expected: Vec<u64> = (0..(THREADS * PER_THREAD) as u64).collect();
        assert_eq!(
            seqs, expected,
            "seq must be gap-free and duplicate-free under concurrent writers"
        );

        // Cross-check against what's actually on disk too (unbounded ring
        // in this config, so nothing should have been evicted).
        let result = recorder.read_since(0, usize::MAX).unwrap();
        let on_disk: Vec<u64> = result.records.iter().map(|r| r.seq()).collect();
        assert_eq!(on_disk, expected);
    }

    #[test]
    fn segment_rotation_creates_new_files_named_by_starting_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", tiny_rotation_config());
        for i in 0..40 {
            recorder
                .append_rx(format!("payload-{i:03}").as_bytes())
                .unwrap();
        }
        let segments_dir = tmp.path().join("devices/dev/segments");
        let mut names: Vec<String> = fs::read_dir(&segments_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(
            names.len() > 1,
            "expected rotation to produce multiple segments, got {names:?}"
        );
        assert_eq!(names[0], "000000000000.jsonl");
        for name in &names {
            assert_eq!(name.len(), "000000000000.jsonl".len());
        }
    }

    #[test]
    fn ring_eviction_deletes_only_the_oldest_segment_and_keeps_the_rest_contiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", tiny_config());
        for i in 0..200 {
            recorder
                .append_rx(format!("payload-{i:04}").as_bytes())
                .unwrap();
        }
        let segments_dir = tmp.path().join("devices/dev/segments");
        let count_before = fs::read_dir(&segments_dir).unwrap().count();
        assert!(
            count_before >= 2,
            "test needs at least 2 segments to prove eviction is selective"
        );

        // The ring budget itself must actually be respected on disk, not
        // just in the recorder's in-memory bookkeeping (which is what a
        // `total_bytes` accounting bug — e.g. an eviction that updates the
        // in-memory list but not the real file, or vice versa — would
        // otherwise let slip through unnoticed).
        let on_disk_total: u64 = fs::read_dir(&segments_dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        let tiny = tiny_config();
        assert!(
            on_disk_total <= tiny.ring_bytes + tiny.segment_bytes,
            "on-disk total ({on_disk_total} bytes) exceeds the configured ring budget \
             ({} bytes) by more than one segment's worth of slack",
            tiny.ring_bytes
        );

        // Aged-out cursor: seq 0 must now be gone.
        let err = recorder.read_since(0, 1).unwrap_err();
        let ReadSinceError::DataAgedOut {
            oldest_available_seq,
        } = err
        else {
            panic!("expected DataAgedOut");
        };
        assert!(oldest_available_seq > 0);

        // Everything from the oldest available seq onward must read back
        // contiguous, with no dupes/gaps.
        let result = recorder
            .read_since(oldest_available_seq, usize::MAX)
            .unwrap();
        for (i, r) in result.records.iter().enumerate() {
            assert_eq!(r.seq(), oldest_available_seq + i as u64);
        }
        assert_eq!(result.records.first().unwrap().seq(), oldest_available_seq);
        assert_eq!(result.records.last().unwrap().seq(), 199);
    }

    #[test]
    fn read_since_never_returns_empty_for_an_aged_out_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", tiny_config());
        for i in 0..200 {
            recorder.append_rx(format!("x{i}").as_bytes()).unwrap();
        }
        match recorder.read_since(0, 4096) {
            Err(ReadSinceError::DataAgedOut {
                oldest_available_seq,
            }) => assert!(oldest_available_seq > 0),
            other => panic!("expected DataAgedOut for an evicted cursor, got {other:?}"),
        }
    }

    #[test]
    fn read_since_with_small_max_bytes_still_makes_progress_via_next_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        for i in 0..30 {
            recorder
                .append_rx(format!("chunk-{i:03}").as_bytes())
                .unwrap();
        }
        let mut cursor = 0u64;
        let mut collected = Vec::new();
        loop {
            let page = recorder.read_since(cursor, 40).unwrap();
            if page.records.is_empty() {
                break;
            }
            collected.extend(page.records.into_iter().map(|r| r.seq()));
            assert!(
                page.next_cursor > cursor,
                "must always make forward progress"
            );
            cursor = page.next_cursor;
        }
        assert_eq!(collected, (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn read_since_across_a_segment_boundary_is_correct_no_dupes_no_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", tiny_rotation_config());
        for i in 0..60 {
            recorder
                .append_rx(format!("boundary-{i:03}").as_bytes())
                .unwrap();
        }
        let segments_dir = tmp.path().join("devices/dev/segments");
        assert!(
            fs::read_dir(&segments_dir).unwrap().count() >= 2,
            "test needs a real segment boundary to cross"
        );
        let result = recorder.read_since(0, usize::MAX).unwrap();
        let seqs: Vec<u64> = result.records.iter().map(|r| r.seq()).collect();
        assert_eq!(seqs, (0..60).collect::<Vec<_>>());
    }

    #[test]
    fn recovery_truncates_a_torn_trailing_line_and_logs_discarded_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let recorder = open(tmp.path(), "dev", RecorderConfig::default());
            for i in 0..5 {
                recorder.append_rx(format!("good-{i}").as_bytes()).unwrap();
            }
        } // drop: clean shutdown, file ends with a valid line

        let segment_path = tmp.path().join("devices/dev/segments/000000000000.jsonl");
        let good_len = fs::metadata(&segment_path).unwrap().len();
        // Simulate a crash mid-write: append a garbage tail with no
        // newline, as a torn write would leave behind.
        let garbage = b"{\"seq\":5,\"t_mono\":1.0,\"t_wall\":\"bad";
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&segment_path).unwrap();
            f.write_all(garbage).unwrap();
        }
        let corrupted_len = fs::metadata(&segment_path).unwrap().len();
        assert_eq!(corrupted_len, good_len + garbage.len() as u64);

        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        let recovered_len = fs::metadata(&segment_path).unwrap().len();
        // Truncated back to (at least) the pre-corruption length, then
        // grew again by exactly one appended `recovery` event.
        assert!(recovered_len > good_len, "recovery event must be appended");

        let result = recorder.read_since(0, usize::MAX).unwrap();
        assert_eq!(result.records.len(), 6, "5 good rx + 1 recovery event");
        let last = result.records.last().unwrap();
        assert_eq!(last.seq(), 5, "recovery event continues seq with no gap");
        match last {
            Record::Event { event, extra, .. } => {
                assert_eq!(event, "recovery");
                let discarded = extra.get("discarded_bytes").and_then(|v| v.as_u64());
                assert_eq!(discarded, Some(garbage.len() as u64));
            }
            other => panic!("expected an Event record, got {other:?}"),
        }
    }

    #[test]
    fn fresh_device_has_no_recovery_event_and_starts_at_seq_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        let r = recorder.append_rx(b"hello").unwrap();
        assert_eq!(r.seq(), 0);
    }

    #[test]
    fn t_wall_matches_rfc3339_with_millis_and_numeric_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        let r = recorder.append_rx(b"x").unwrap();
        let t_wall = r.t_wall();
        // e.g. 2026-07-27T10:34:12.443+08:00 or ...+00:00
        assert!(
            chrono::DateTime::parse_from_rfc3339(t_wall).is_ok(),
            "t_wall {t_wall:?} must be valid RFC 3339"
        );
        assert!(
            t_wall.contains('.'),
            "t_wall {t_wall:?} must carry millisecond precision"
        );
        assert!(
            !t_wall.ends_with('Z'),
            "t_wall {t_wall:?} must carry a numeric UTC offset (e.g. +08:00 or \
             +00:00), not the 'Z' shorthand"
        );
    }

    #[test]
    fn device_id_path_traversal_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Recorder::open(tmp.path(), "../escape", RecorderConfig::default()).is_err());
        assert!(Recorder::open(tmp.path(), "a/b", RecorderConfig::default()).is_err());
        assert!(Recorder::open(tmp.path(), "", RecorderConfig::default()).is_err());
    }

    #[test]
    fn a_second_recorder_over_the_same_device_is_rejected_while_the_first_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let first = open(tmp.path(), "dev", RecorderConfig::default());

        // Two writers on the same device would each allocate their own
        // `next_seq` and interleave appends into the same file — the
        // exclusive lock must prevent this outright rather than let it
        // silently corrupt the stream.
        let second = Recorder::open(tmp.path(), "dev", RecorderConfig::default());
        assert!(
            second.is_err(),
            "a second Recorder over the same open device must be rejected"
        );

        drop(first);
        // Once released, a fresh Recorder must be able to take the lock.
        let third = Recorder::open(tmp.path(), "dev", RecorderConfig::default());
        assert!(
            third.is_ok(),
            "the lock must be released when the first Recorder drops"
        );
    }

    #[test]
    fn reopening_a_clean_recorder_continues_seq_with_no_recovery_event() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let recorder = open(tmp.path(), "dev", RecorderConfig::default());
            for i in 0..10 {
                recorder.append_rx(format!("l{i}").as_bytes()).unwrap();
            }
        }
        let recorder = open(tmp.path(), "dev", RecorderConfig::default());
        let r = recorder.append_rx(b"eleventh").unwrap();
        assert_eq!(r.seq(), 10, "no recovery event should be inserted, no gap");
    }
}
