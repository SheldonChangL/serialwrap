//! Query layer: line assembly, cursors, filters, out-of-band event
//! tracking, and `wait_for` over one device's recorded stream (`TASKS.md`
//! T1.4, issue #6). See the [Client protocol
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
//! for the authoritative `tail`/`read_since`/`wait_for`/`subscribe`/
//! `query_events` semantics this module implements.
//!
//! # Why line assembly lives here, not in the recorder
//!
//! [`crate::recorder::Recorder`] stores raw `rx` chunks exactly as read
//! from the device — no line assembly (see that module's docs: "a half-line
//! held in memory waiting for its newline is exactly the half-line a crash
//! would lose"). This module is what turns that chunk stream into
//! [`AssembledLine`]s, entirely derived from what's already durably on
//! disk, so it can be rebuilt from scratch at any time and never itself
//! holds data the recorder didn't already commit.
//!
//! # Why one shared [`DeviceQueryState`] per device, not one per connection
//!
//! Every `tail`/`read_since`/`wait_for`/`subscribe`/`query_events` request
//! against a device reads from the *same* [`DeviceQueryState`], fed by
//! exactly one background consumer of [`Recorder::read_since`]
//! ([`DeviceQueryState::ingest`], driven by [`spawn_poller`]). This is the
//! whole mechanism behind "8 concurrent subscribers see identical
//! seq/bytes" (`TASKS.md` T1.4 acceptance criterion 1): there is only ever
//! one assembler, so there is nothing for two callers' views to disagree
//! about.
//!
//! # Half-lines never match
//!
//! An in-progress, not-yet-newline-terminated chunk of rx bytes is kept
//! separate from [`DeviceQueryState`]'s `lines` — it is never a candidate
//! for `wait_for`'s regex, never returned by `tail`/`read_since`, and never
//! subject to a `Filter`. A pattern that would match across a chunk
//! boundary (the wiki's own example: `Temp: 25` arrives, then `.7 C\n`
//! 200ms later) cannot fire until the newline actually arrives — see
//! `DeviceQueryState::ingest`'s per-byte scan below.
//!
//! # Out-of-band events are never filtered, only range-bounded
//!
//! A [`Filter`] narrows which *lines* a query returns; it never removes an
//! [`OobRecord`] (`event`/`gate` kind records — disconnects, config
//! changes, lease activity, gate decisions). Both kinds share the same
//! `[cursor, next_cursor)` range accounting (see
//! [`DeviceQueryState::read_since`]), so paging never skips or duplicates
//! either — the filter only decides what's *returned*, never what's
//! *scanned*.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use regex::Regex;
use wrap_proto::{Filter, Kind, Record};

use crate::recorder::{ReadSinceError, Recorder};

/// Opaque cursor into the record stream.
///
/// Currently just the underlying `seq`; kept as a distinct type so callers
/// don't assume it stays a bare integer once cross-segment queries land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor(pub u64);

/// How often [`spawn_poller`]'s background task re-checks the recorder for
/// new data. Small enough that it contributes negligible latency to
/// `wait_for`'s timeout precision (acceptance criterion: <=100ms error) and
/// to `subscribe`'s push cadence, while still being a small, fixed number
/// of wakeups/sec per device rather than a busy spin.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Upper bound on how many bytes of raw records [`DeviceQueryState::ingest`]
/// pulls from the recorder per poll tick. Generous relative to anything
/// this project's tests or a real serial device produce between 5ms polls;
/// exists only so one pathological poll tick can't try to load an entire
/// multi-GB device history into memory in one call.
const MAX_INGEST_BYTES: usize = 16 * 1024 * 1024;

/// One fully assembled line of device output — the unit `tail`,
/// `read_since`, `wait_for`, and `subscribe` all operate on.
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledLine {
    /// The exact original bytes, with only the terminating `\n` (and a
    /// preceding `\r`, if any — i.e. CRLF input) stripped. This is the
    /// authoritative source of truth for this line: recorded rx bytes are
    /// untrusted, not necessarily valid UTF-8 (see the wiki's Security
    /// model), and this project's core promise — "you record bytes, not
    /// characters" — has to hold at the query layer too, not just in
    /// `recorder.rs`. See [`Self::text`] for the lossy display-only
    /// derivative.
    pub raw: Vec<u8>,
    /// `raw` decoded via `String::from_utf8_lossy` — a convenience field
    /// for callers that only want to print or pattern-match text (e.g.
    /// `wait_for`'s regex, the CLI's default rendering) and don't care
    /// about byte-exactness. An invalid sequence becomes U+FFFD here, but
    /// unlike before this field existed alongside [`Self::raw`], that lossy
    /// conversion is no longer destructive: `raw` still holds the original
    /// bytes regardless of what `text` looks like.
    pub text: String,
    /// `seq` of the raw `rx` record whose bytes contained this line's
    /// terminating `\n`. Not unique across a batch — a single rx chunk can
    /// complete more than one line, in which case they share this `seq`
    /// (callers resume from `seq + 1`, the same convention
    /// `Recorder::read_since` itself uses for its own `next_cursor`).
    pub seq: u64,
    pub t_mono: f64,
    pub t_wall: String,
}

/// One out-of-band occurrence: an `event` or `gate` kind record. Always
/// included in a query's range regardless of any [`Filter`] — see the
/// module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct OobRecord {
    pub seq: u64,
    pub t_mono: f64,
    pub t_wall: String,
    pub kind: Kind,
    /// The `event` name (e.g. `"disconnect"`, `"config_change"`) for
    /// `Kind::Event`; `None` for `Kind::Gate` (whose own fields land in
    /// `extra` instead).
    pub name: Option<String>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// In-progress, not-yet-newline-terminated tail of rx bytes. Deliberately
/// never exposed outside this module: it is not a candidate for
/// `wait_for`, not returned by `tail`/`read_since`, and not subject to a
/// `Filter` — see the module docs.
#[derive(Debug, Default)]
struct Partial {
    buf: Vec<u8>,
}

/// Result of a `tail`/`read_since`-shaped query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryPage {
    pub lines: Vec<AssembledLine>,
    pub events: Vec<OobRecord>,
    /// Pass this back as the next call's cursor to continue exactly where
    /// this one left off (same contract as `Recorder::read_since`).
    pub cursor: u64,
}

/// A `subscribe` task's position within [`DeviceQueryState`]'s internal
/// `lines`/`events` vectors — an opaque `(line_index, event_index)` pair
/// passed back into [`DeviceQueryState::drain_since`] to resume exactly
/// where the previous call left off.
pub type DrainCursor = (usize, usize);

/// Result of [`DeviceQueryState::drain_since`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DrainResult {
    pub lines: Vec<AssembledLine>,
    pub events: Vec<OobRecord>,
    pub next: DrainCursor,
}

/// Failure modes for a query against [`DeviceQueryState`].
#[derive(Debug, Clone, PartialEq)]
pub enum QueryError {
    /// `cursor` is older than the oldest record this state has retained.
    /// Carries the oldest sequence number still available, same contract
    /// as [`ReadSinceError::DataAgedOut`].
    DataAgedOut { oldest_available_seq: u64 },
    /// The `Filter`'s (or `wait_for`'s) regex failed to compile. Carries
    /// the underlying parser message.
    InvalidPattern(String),
}

/// Outcome of a [`DeviceQueryState::wait_for`] call — always a structured
/// result, never a bare timeout/hang (`TASKS.md` T1.4).
#[derive(Debug, Clone, PartialEq)]
pub enum WaitForOutcome {
    Matched {
        line: String,
        seq: u64,
        elapsed_ms: f64,
    },
    TimedOut {
        elapsed_ms: f64,
        timeout_s: f64,
    },
}

struct CompiledFilter {
    re: Regex,
    exclude: bool,
}

fn compile_filter(filter: Option<&Filter>) -> Result<Option<CompiledFilter>, QueryError> {
    match filter {
        None => Ok(None),
        Some(f) => {
            let re =
                Regex::new(&f.pattern).map_err(|e| QueryError::InvalidPattern(e.to_string()))?;
            Ok(Some(CompiledFilter {
                re,
                exclude: f.exclude,
            }))
        }
    }
}

fn line_passes(line: &AssembledLine, filter: Option<&CompiledFilter>) -> bool {
    match filter {
        None => true,
        Some(f) => f.re.is_match(&line.text) != f.exclude,
    }
}

/// One item of the merged lines+events stream, used internally by
/// [`DeviceQueryState::read_since`] to keep both under one cursor/range —
/// see the module docs on why filtering must never change the scanned
/// range, only what's returned from it.
enum StreamItem<'a> {
    Line(&'a AssembledLine),
    Event(&'a OobRecord),
}

impl StreamItem<'_> {
    fn seq(&self) -> u64 {
        match self {
            StreamItem::Line(l) => l.seq,
            StreamItem::Event(e) => e.seq,
        }
    }

    /// Rough serialized-size estimate for `max_bytes` bounding. Doesn't need
    /// to be exact — it exists so a bounded page makes forward progress at
    /// a sane granularity, the same role `Recorder::read_since`'s own
    /// `max_bytes` plays over raw JSONL bytes.
    fn approx_size(&self) -> usize {
        match self {
            StreamItem::Line(l) => l.text.len() + 48,
            StreamItem::Event(e) => e.extra.len() * 24 + 64,
        }
    }
}

/// Shared, continuously-updated query state for one device — assembled
/// lines, out-of-band records, and the in-progress partial line — fed by a
/// single background consumer of [`Recorder::read_since`]. See the module
/// docs for why there is exactly one of these per device, not one per
/// connection.
///
/// # Known limitation: unbounded in-memory growth
///
/// `lines`/`events` only ever grow — nothing here mirrors the recorder's
/// own ring eviction (`Recorder`'s on-disk segments are bounded by
/// `RecorderConfig::ring_bytes`; this in-memory cache is not). A daemon
/// left running for a very long time against a chatty device will grow
/// this cache indefinitely. Acceptable for this task's scope (protocol
/// correctness, not production memory bounding) but worth trimming to a
/// bounded suffix window (or dropping the cache and re-deriving from disk
/// on `DataAgedOut`) before this ships for real long-lived daemons.
pub struct DeviceQueryState {
    lines: Mutex<Vec<AssembledLine>>,
    events: Mutex<Vec<OobRecord>>,
    partial: Mutex<Partial>,
    /// Lowest seq still represented here — the floor below which
    /// `read_since` must report [`QueryError::DataAgedOut`] rather than
    /// silently returning nothing (which would be indistinguishable from
    /// "no new data yet", same reasoning as the recorder's own contract).
    oldest_seq: Mutex<Option<u64>>,
    /// Where [`Self::ingest`] left off reading from the `Recorder`.
    recorder_cursor: Mutex<u64>,
    /// Woken on every successful `ingest` that adds at least one line or
    /// event — what `wait_for` and `subscribe` block on instead of
    /// spin-polling their own copy of the state.
    notify: tokio::sync::Notify,
}

impl Default for DeviceQueryState {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceQueryState {
    pub fn new() -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            partial: Mutex::new(Partial::default()),
            oldest_seq: Mutex::new(None),
            recorder_cursor: Mutex::new(0),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Number of complete lines assembled so far. Used by `wait_for` to
    /// snapshot "now" before deciding what counts as a new match — see
    /// [`Self::wait_for`]'s docs.
    pub fn line_count(&self) -> usize {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Number of out-of-band records assembled so far. Used by `subscribe`
    /// to snapshot "now" alongside [`Self::line_count`] before starting its
    /// push loop — see [`Self::drain_since`].
    pub fn event_count(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Pull everything new from `recorder` since the last call and fold it
    /// into `lines`/`events`/`partial`. Safe to call repeatedly (e.g. from
    /// [`spawn_poller`]'s loop, or directly from a test after a known
    /// append); never blocks and never panics on an I/O error — logs and
    /// leaves state unchanged instead, matching every other best-effort
    /// stance in this crate (`recorder.rs`, `port.rs`).
    pub fn ingest(&self, recorder: &Recorder) {
        let cursor = *self
            .recorder_cursor
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let page = match recorder.read_since(cursor, MAX_INGEST_BYTES) {
            Ok(page) => page,
            Err(ReadSinceError::DataAgedOut {
                oldest_available_seq,
            }) => {
                // Our own cursor fell behind the recorder's ring eviction
                // (e.g. this state was just created against a device whose
                // early history is already gone). Resync to the new floor
                // and retry once from there.
                match recorder.read_since(oldest_available_seq, MAX_INGEST_BYTES) {
                    Ok(page) => {
                        let mut oldest = self.oldest_seq.lock().unwrap_or_else(|e| e.into_inner());
                        *oldest = Some(
                            oldest.map_or(oldest_available_seq, |o| o.max(oldest_available_seq)),
                        );
                        page
                    }
                    Err(e) => {
                        eprintln!("serialwrapd: query: ingest retry after DataAgedOut failed: {e}");
                        return;
                    }
                }
            }
            Err(ReadSinceError::Io(e)) => {
                eprintln!("serialwrapd: query: ingest read_since failed: {e}");
                return;
            }
        };

        if page.records.is_empty() {
            *self
                .recorder_cursor
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = page.next_cursor;
            return;
        }

        {
            let mut oldest = self.oldest_seq.lock().unwrap_or_else(|e| e.into_inner());
            if oldest.is_none() {
                oldest.replace(page.records[0].seq());
            }
        }

        let mut added = false;
        {
            let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            let mut partial = self.partial.lock().unwrap_or_else(|e| e.into_inner());

            for record in &page.records {
                match record {
                    Record::Rx {
                        seq,
                        t_mono,
                        t_wall,
                        data_b64,
                    } => {
                        let Ok(bytes) = BASE64.decode(data_b64) else {
                            // The recorder only ever writes what it itself
                            // base64-encoded (see `Recorder::append_rx`);
                            // this should be unreachable for real data, and
                            // silently skipping (rather than panicking) is
                            // consistent with `read_since`'s own defensive
                            // stance on unparseable stored bytes.
                            continue;
                        };
                        for &b in &bytes {
                            if b == b'\n' {
                                let mut raw = std::mem::take(&mut partial.buf);
                                if raw.last() == Some(&b'\r') {
                                    raw.pop();
                                }
                                let text = String::from_utf8_lossy(&raw).into_owned();
                                lines.push(AssembledLine {
                                    raw,
                                    text,
                                    seq: *seq,
                                    t_mono: *t_mono,
                                    t_wall: t_wall.clone(),
                                });
                                added = true;
                            } else {
                                partial.buf.push(b);
                            }
                        }
                    }
                    Record::Event {
                        seq,
                        t_mono,
                        t_wall,
                        event,
                        extra,
                    } => {
                        events.push(OobRecord {
                            seq: *seq,
                            t_mono: *t_mono,
                            t_wall: t_wall.clone(),
                            kind: Kind::Event,
                            name: Some(event.clone()),
                            extra: extra.clone(),
                        });
                        added = true;
                    }
                    Record::Gate {
                        seq,
                        t_mono,
                        t_wall,
                        action,
                        reason,
                        request_seq,
                    } => {
                        let mut extra = serde_json::Map::new();
                        extra.insert("action".to_string(), action.clone().into());
                        extra.insert("reason".to_string(), reason.clone().into());
                        extra.insert("request_seq".to_string(), (*request_seq).into());
                        events.push(OobRecord {
                            seq: *seq,
                            t_mono: *t_mono,
                            t_wall: t_wall.clone(),
                            kind: Kind::Gate,
                            name: None,
                            extra,
                        });
                        added = true;
                    }
                    Record::Tx { .. } => {
                        // Not surfaced by tail/read_since/query_events at
                        // this stage — the write path itself is deferred
                        // (T4.x's write gate); revisit what audit view a
                        // `tx` record needs once `write` actually lands.
                    }
                }
            }
        }

        *self
            .recorder_cursor
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = page.next_cursor;
        if added {
            self.notify.notify_waiters();
        }
    }

    /// Last `n` lines (after `filter`), plus every out-of-band event known
    /// so far, plus a cursor a caller can `read_since` from to continue
    /// live. See the module docs for why events are never filtered.
    pub fn tail(&self, n: usize, filter: Option<&Filter>) -> Result<QueryPage, QueryError> {
        let compiled = compile_filter(filter)?;
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());

        let filtered: Vec<AssembledLine> = lines
            .iter()
            .filter(|l| line_passes(l, compiled.as_ref()))
            .cloned()
            .collect();
        let start = filtered.len().saturating_sub(n);
        let picked = filtered[start..].to_vec();

        let newest_line = lines.last().map(|l| l.seq);
        let newest_event = events.last().map(|e| e.seq);
        let cursor = newest_line
            .into_iter()
            .chain(newest_event)
            .max()
            .map_or(0, |s| s + 1);

        Ok(QueryPage {
            lines: picked,
            events: events.clone(),
            cursor,
        })
    }

    /// Records with `seq >= cursor` (lines, filtered; events, never
    /// filtered), bounded by `max_bytes` of the *merged* lines+events
    /// stream (see [`StreamItem`]), plus the next cursor. Never returns an
    /// empty page for a cursor ring eviction has already passed —
    /// [`QueryError::DataAgedOut`] instead, same contract as
    /// [`Recorder::read_since`].
    pub fn read_since(
        &self,
        cursor: u64,
        max_bytes: Option<usize>,
        filter: Option<&Filter>,
    ) -> Result<QueryPage, QueryError> {
        let compiled = compile_filter(filter)?;

        if let Some(oldest) = *self.oldest_seq.lock().unwrap_or_else(|e| e.into_inner()) {
            if cursor < oldest {
                return Err(QueryError::DataAgedOut {
                    oldest_available_seq: oldest,
                });
            }
        }

        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());

        let mut merged: Vec<StreamItem> = Vec::new();
        merged.extend(
            lines
                .iter()
                .filter(|l| l.seq >= cursor)
                .map(StreamItem::Line),
        );
        merged.extend(
            events
                .iter()
                .filter(|e| e.seq >= cursor)
                .map(StreamItem::Event),
        );
        merged.sort_by_key(|item| item.seq());

        let mut out_lines = Vec::new();
        let mut out_events = Vec::new();
        let mut next_cursor = cursor;
        let mut bytes_used = 0usize;

        for item in &merged {
            if let Some(cap) = max_bytes {
                let would_exceed = bytes_used + item.approx_size() > cap;
                let already_has_something = !out_lines.is_empty() || !out_events.is_empty();
                if already_has_something && would_exceed {
                    break;
                }
            }
            bytes_used += item.approx_size();
            next_cursor = item.seq() + 1;
            match item {
                StreamItem::Line(l) => {
                    if line_passes(l, compiled.as_ref()) {
                        out_lines.push((*l).clone());
                    }
                }
                StreamItem::Event(e) => out_events.push((*e).clone()),
            }
        }

        Ok(QueryPage {
            lines: out_lines,
            events: out_events,
            cursor: next_cursor,
        })
    }

    /// `event`/`gate` records in `[since_seq, until_seq]`, optionally
    /// narrowed to `kinds` (matched against the `Kind` discriminant —
    /// `"event"`/`"gate"` — or, for `Kind::Event`, the specific event name
    /// too, e.g. `"disconnect"`). Empty `kinds` means "all". This is the
    /// audit view — see the wiki's Error handling / Security-model pages.
    pub fn query_events(
        &self,
        kinds: &[String],
        since_seq: Option<u64>,
        until_seq: Option<u64>,
    ) -> Vec<OobRecord> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events
            .iter()
            .filter(|e| since_seq.is_none_or(|s| e.seq >= s))
            .filter(|e| until_seq.is_none_or(|u| e.seq <= u))
            .filter(|e| {
                if kinds.is_empty() {
                    return true;
                }
                kinds
                    .iter()
                    .any(|k| k == kind_str(e.kind) || e.name.as_deref() == Some(k.as_str()))
            })
            .cloned()
            .collect()
    }

    /// Scan currently-assembled lines starting at index `from` (inclusive)
    /// for the first one matching `re`. Returns the match plus the index
    /// just past it, so a caller can resume scanning without re-checking
    /// already-seen lines.
    fn find_match_from(&self, re: &Regex, from: usize) -> (Option<(String, u64)>, usize) {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let mut idx = from;
        while idx < lines.len() {
            let line = &lines[idx];
            idx += 1;
            if re.is_match(&line.text) {
                return (Some((line.text.clone(), line.seq)), idx);
            }
        }
        (None, idx)
    }

    /// Block until a fully-assembled line matches `pattern`, or `timeout`
    /// elapses. Only ever considers lines assembled *from the moment this
    /// call started* onward — deliberately not the entire recorded
    /// history, so a stale match from long before the call (e.g. a
    /// substring that happened to appear in an old boot log) can never be
    /// mistaken for "it just happened". This is the same "wait for what
    /// happens next" semantics classic `expect`-style tooling uses.
    ///
    /// Never matches a half-line: matching only ever runs against
    /// [`Self::lines`], which [`Self::ingest`] only appends to once a
    /// terminating `\n` has actually arrived — see the module docs.
    pub async fn wait_for(
        &self,
        pattern: &str,
        timeout: Duration,
    ) -> Result<WaitForOutcome, QueryError> {
        let re = Regex::new(pattern).map_err(|e| QueryError::InvalidPattern(e.to_string()))?;
        let start = Instant::now();
        let deadline = start + timeout;
        let mut checked = self.line_count();

        loop {
            let (found, next_checked) = self.find_match_from(&re, checked);
            checked = next_checked;
            if let Some((line, seq)) = found {
                return Ok(WaitForOutcome::Matched {
                    line,
                    seq,
                    elapsed_ms: elapsed_ms(start),
                });
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitForOutcome::TimedOut {
                    elapsed_ms: elapsed_ms(start),
                    timeout_s: timeout.as_secs_f64(),
                });
            }
            let remaining = deadline - now;

            // Register as a waiter *before* checking the deadline/sleeping
            // — this is `tokio::sync::Notify`'s documented race-free
            // pattern: the `Notified` future snapshots "has a notification
            // happened since this call" at creation time, so a
            // `notify_waiters()` landing between here and the `select!`
            // below is never missed.
            let notified = self.notify.notified();
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return Ok(WaitForOutcome::TimedOut {
                        elapsed_ms: elapsed_ms(start),
                        timeout_s: timeout.as_secs_f64(),
                    });
                }
            }
        }
    }

    /// Resolve a `since_cursor` (`read_since`-compatible: "everything with
    /// `seq >= since_cursor`") into the [`DrainCursor`] position
    /// [`Self::drain_since`] should start from — what closes the
    /// tail-then-subscribe gap (`TASKS.md` issue #32): a client passes back
    /// the cursor an earlier `tail`/`read_since` call returned, and the
    /// first thing `subscribe` ever drains from the resulting position is
    /// exactly what a `read_since(since_cursor)` call would have returned
    /// at that same instant — no gap, no duplicate.
    ///
    /// Fails with [`QueryError::DataAgedOut`] under exactly the condition
    /// [`Self::read_since`] does: `since_cursor` older than the oldest
    /// record this state still retains. This can only happen right here,
    /// at subscribe start — once resolved, a `DrainCursor` stays valid for
    /// the lifetime of the subscription, because `lines`/`events` are
    /// append-only in memory (see the struct docs' "known limitation");
    /// nothing after this call can ever again invalidate an already-issued
    /// `DrainCursor`.
    pub fn cursor_from_seq(&self, since_cursor: u64) -> Result<DrainCursor, QueryError> {
        if let Some(oldest) = *self.oldest_seq.lock().unwrap_or_else(|e| e.into_inner()) {
            if since_cursor < oldest {
                return Err(QueryError::DataAgedOut {
                    oldest_available_seq: oldest,
                });
            }
        }
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        // `lines`/`events` are each sorted ascending by `seq` (ties allowed
        // — see `AssembledLine::seq`'s docs) purely by construction: only
        // ever appended to, in the order `ingest` processes an
        // already-ascending `Recorder::read_since` page. `partition_point`
        // is exactly the right tool for "first index at/after a threshold"
        // over such a sequence.
        let line_idx = lines.partition_point(|l| l.seq < since_cursor);
        let event_idx = events.partition_point(|e| e.seq < since_cursor);
        Ok((line_idx, event_idx))
    }

    /// Snapshot everything at/after `from` right now — used by a
    /// `subscribe` task's first poll and every subsequent wakeup. Returns
    /// the [`DrainResult`] to resume from next time.
    pub fn drain_since(
        &self,
        from: DrainCursor,
        filter: Option<&Filter>,
    ) -> Result<DrainResult, QueryError> {
        let compiled = compile_filter(filter)?;
        let (line_idx, event_idx) = from;
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());

        let new_lines: Vec<AssembledLine> = lines
            .get(line_idx..)
            .unwrap_or(&[])
            .iter()
            .filter(|l| line_passes(l, compiled.as_ref()))
            .cloned()
            .collect();
        let new_events: Vec<OobRecord> = events.get(event_idx..).unwrap_or(&[]).to_vec();

        Ok(DrainResult {
            lines: new_lines,
            events: new_events,
            next: (lines.len(), events.len()),
        })
    }

    /// A `Notified` future for `subscribe`/anything else that wants to wake
    /// up on the next `ingest` that adds data. See [`Self::wait_for`]'s
    /// docs for the race-free usage pattern this must follow.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Rx => "rx",
        Kind::Tx => "tx",
        Kind::Event => "event",
        Kind::Gate => "gate",
    }
}

/// Run [`DeviceQueryState::ingest`] against `recorder` every `interval`
/// until the returned handle is dropped/aborted. This is the production
/// wiring's data source for a device's query state (see
/// `protocol::backend::LiveBackend`); tests that want zero-latency
/// propagation instead call `ingest` directly after a known `append_rx`.
pub fn spawn_poller(
    recorder: std::sync::Arc<Recorder>,
    state: std::sync::Arc<DeviceQueryState>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            state.ingest(&recorder);
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::RecorderConfig;
    use serde_json::Map;

    fn recorder(tmp: &std::path::Path) -> Recorder {
        Recorder::open(tmp, "dev", RecorderConfig::default()).expect("open recorder")
    }

    #[test]
    fn assembles_only_complete_lines_and_holds_back_the_partial_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();

        recorder.append_rx(b"Temp: 25").unwrap();
        state.ingest(&recorder);
        assert_eq!(state.line_count(), 0, "half a line must not assemble yet");

        recorder.append_rx(b".7 C\n").unwrap();
        state.ingest(&recorder);
        assert_eq!(state.line_count(), 1);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 1);
        assert_eq!(page.lines[0].text, "Temp: 25.7 C");
    }

    #[test]
    fn crlf_input_strips_the_trailing_cr() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"hello\r\n").unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines[0].text, "hello");
    }

    #[test]
    fn one_chunk_can_complete_more_than_one_line() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"a\nb\nc\n").unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn invalid_utf8_becomes_replacement_characters_not_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        let mut bytes = vec![0xFFu8, 0xFE, b'x'];
        bytes.push(b'\n');
        recorder.append_rx(&bytes).unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 1);
        assert!(page.lines[0].text.ends_with('x'));
    }

    // ---- Issue #32 acceptance criterion 1: byte-exact round trip ----

    #[test]
    fn invalid_utf8_line_keeps_its_exact_original_bytes_in_raw() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        // Deliberately invalid UTF-8 (a lone continuation byte, then a
        // truncated 2-byte sequence) mixed in with ordinary text — this is
        // exactly the shape `String::from_utf8_lossy` cannot round-trip
        // (it would fold both into a single U+FFFD each, destroying the
        // original byte count and values).
        let original: Vec<u8> = {
            let mut v = b"before-".to_vec();
            v.extend_from_slice(&[0xFF, 0xFE, 0x80]);
            v.extend_from_slice(b"-after");
            v
        };
        let mut with_newline = original.clone();
        with_newline.push(b'\n');
        recorder.append_rx(&with_newline).unwrap();
        state.ingest(&recorder);

        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 1);
        let line = &page.lines[0];

        assert_eq!(
            line.raw, original,
            "raw must be byte-for-byte identical to what was written, newline stripped"
        );
        assert!(
            std::str::from_utf8(&line.raw).is_err(),
            "test fixture must actually be invalid UTF-8, or this test proves nothing"
        );
        assert!(
            line.text.contains('\u{FFFD}'),
            "text is still the lossy display form: {:?}",
            line.text
        );

        let mut original_hasher = Sha256::new();
        original_hasher.update(&original);
        let mut raw_hasher = Sha256::new();
        raw_hasher.update(&line.raw);
        assert_eq!(
            format!("{:x}", original_hasher.finalize()),
            format!("{:x}", raw_hasher.finalize()),
            "sha256(original) must match sha256(raw) — the whole point of carrying raw bytes"
        );
    }

    // ---- Issue #32 acceptance criterion 2: subscribe(since_cursor) has no gap ----

    #[test]
    fn cursor_from_seq_resolves_to_the_same_position_read_since_would_start_from() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        for i in 0..10 {
            recorder
                .append_rx(format!("line-{i}\n").as_bytes())
                .unwrap();
        }
        state.ingest(&recorder);

        // tail-style: everything is currently at seq 0..=9, so "the cursor
        // to continue from" is 10 (same `+1` convention `tail`/`read_since`
        // already use).
        let since_cursor = 10;
        recorder.append_rx(b"line-10\n").unwrap();
        state.ingest(&recorder);

        let (line_idx, event_idx) = state.cursor_from_seq(since_cursor).unwrap();
        let drained = state.drain_since((line_idx, event_idx), None).unwrap();
        let read_since_page = state.read_since(since_cursor, None, None).unwrap();

        assert_eq!(event_idx, 0, "no events were ever appended in this test");
        assert_eq!(
            drained.lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
            read_since_page
                .lines
                .iter()
                .map(|l| l.seq)
                .collect::<Vec<_>>(),
            "subscribe's drain_since(cursor_from_seq(N)) must agree with read_since(N)"
        );
        assert_eq!(drained.lines.len(), 1, "expected only the seq=10 line");
        assert_eq!(
            drained.lines[0].seq, 10,
            "first line must be N+1 = 10, not a repeat of 0..9"
        );
        assert_eq!(drained.lines[0].text, "line-10");
    }

    #[test]
    fn cursor_from_seq_reports_data_aged_out_below_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"line one\n").unwrap();
        state.ingest(&recorder);
        // Same simulated-eviction setup `read_since_reports_data_aged_out_below_the_floor`
        // uses: pretend seq 0..5 have already been evicted.
        *state.oldest_seq.lock().unwrap() = Some(5);
        let err = state.cursor_from_seq(0).unwrap_err();
        assert_eq!(
            err,
            QueryError::DataAgedOut {
                oldest_available_seq: 5
            }
        );
    }

    #[test]
    fn filter_excludes_lines_but_never_events() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"keep me\n").unwrap();
        recorder.append_rx(b"drop me\n").unwrap();
        recorder.append_event("disconnect", Map::new()).unwrap();
        state.ingest(&recorder);

        let filter = Filter {
            pattern: ".*".to_string(),
            exclude: true, // exclude everything
        };
        let page = state.read_since(0, None, Some(&filter)).unwrap();
        assert!(
            page.lines.is_empty(),
            "an exclude-all filter must drop every line"
        );
        assert_eq!(
            page.events.len(),
            1,
            "out-of-band events must survive a filter that excludes all log lines"
        );
        assert_eq!(page.events[0].name.as_deref(), Some("disconnect"));
    }

    #[test]
    fn read_since_reports_data_aged_out_below_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"line one\n").unwrap();
        state.ingest(&recorder);
        // Simulate ring eviction having already happened before this state
        // ever saw seq 0: manually raise the floor to pretend seq 0..5 are
        // gone, the way `ingest`'s DataAgedOut branch would set it.
        *state.oldest_seq.lock().unwrap() = Some(5);
        let err = state.read_since(0, None, None).unwrap_err();
        assert_eq!(
            err,
            QueryError::DataAgedOut {
                oldest_available_seq: 5
            }
        );
    }

    #[test]
    fn read_since_cursor_always_advances_and_never_drops_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        for i in 0..20 {
            recorder
                .append_rx(format!("line-{i}\n").as_bytes())
                .unwrap();
        }
        state.ingest(&recorder);

        let mut cursor = 0u64;
        let mut collected = Vec::new();
        loop {
            let page = state.read_since(cursor, Some(40), None).unwrap();
            if page.lines.is_empty() && page.events.is_empty() {
                break;
            }
            collected.extend(page.lines.iter().map(|l| l.text.clone()));
            assert!(page.cursor > cursor, "cursor must always advance");
            cursor = page.cursor;
        }
        let expected: Vec<String> = (0..20).map(|i| format!("line-{i}")).collect();
        assert_eq!(collected, expected);
    }

    #[tokio::test]
    async fn wait_for_matches_a_line_that_completes_after_the_call_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = std::sync::Arc::new(recorder(tmp.path()));
        let state = std::sync::Arc::new(DeviceQueryState::new());

        let waiter = {
            let state = std::sync::Arc::clone(&state);
            tokio::spawn(async move { state.wait_for("boot ok", Duration::from_secs(2)).await })
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        recorder.append_rx(b"boot ok\n").unwrap();
        state.ingest(&recorder);

        let outcome = waiter.await.unwrap().unwrap();
        match outcome {
            WaitForOutcome::Matched { line, seq, .. } => {
                assert_eq!(line, "boot ok");
                assert_eq!(seq, 0);
            }
            other => panic!("expected Matched, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_for_times_out_with_a_structured_result_not_a_hang() {
        let state = DeviceQueryState::new();
        let outcome = state
            .wait_for("never happens", Duration::from_millis(80))
            .await
            .unwrap();
        match outcome {
            WaitForOutcome::TimedOut { timeout_s, .. } => {
                assert!((timeout_s - 0.08).abs() < 1e-9);
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[test]
    fn invalid_regex_pattern_is_a_structured_error_not_a_panic() {
        let state = DeviceQueryState::new();
        let filter = Filter {
            pattern: "(unclosed".to_string(),
            exclude: false,
        };
        let err = state.read_since(0, None, Some(&filter)).unwrap_err();
        assert!(matches!(err, QueryError::InvalidPattern(_)));
    }

    #[test]
    fn query_events_filters_by_kind_and_seq_range() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_event("connect", Map::new()).unwrap();
        recorder.append_event("disconnect", Map::new()).unwrap();
        recorder.append_gate("deny", "danger:erase", 0).unwrap();
        state.ingest(&recorder);

        let all = state.query_events(&[], None, None);
        assert_eq!(all.len(), 3);

        let only_disconnect = state.query_events(&["disconnect".to_string()], None, None);
        assert_eq!(only_disconnect.len(), 1);
        assert_eq!(only_disconnect[0].name.as_deref(), Some("disconnect"));

        let only_gate = state.query_events(&["gate".to_string()], None, None);
        assert_eq!(only_gate.len(), 1);
        assert_eq!(only_gate[0].kind, Kind::Gate);

        let ranged = state.query_events(&[], Some(1), Some(1));
        assert_eq!(ranged.len(), 1);
        assert_eq!(ranged[0].name.as_deref(), Some("disconnect"));
    }
}
