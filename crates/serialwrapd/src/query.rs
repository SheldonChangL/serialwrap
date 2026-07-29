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
use serde::{Deserialize, Serialize};
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
    /// terminator — `\n` (or, since issue #52, a bare `\r` under CR-only
    /// assembly) — or, for a [`Self::capped`] line, the record whose bytes
    /// crossed [`MAX_PARTIAL_BYTES`]. Not unique across a batch — a single
    /// rx chunk can complete more than one line (including, for the byte
    /// that resolves [`LineTerminatorMode::Auto`] detection, every line
    /// retroactively completed from the probe accumulated before it — see
    /// [`Partial`]'s docs), in which case they share this `seq` (callers
    /// resume from `seq + 1`, the same convention `Recorder::read_since`
    /// itself uses for its own `next_cursor`).
    pub seq: u64,
    pub t_mono: f64,
    pub t_wall: String,
    /// `true` if this line was force-completed by [`MAX_PARTIAL_BYTES`]
    /// rather than an actual line terminator ever arriving (issue #52's
    /// partial-buffer-cap requirement) — `raw`/`text` still hold exactly
    /// whatever bytes had accumulated, just cut off rather than
    /// terminator-delimited. `false` for every ordinarily-terminated line,
    /// which is every line this project produced before this field existed.
    pub capped: bool,
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

/// How a device's rx stream splits into lines — issue #52: the original
/// implementation only ever recognized `\n` (stripping a preceding `\r` for
/// CRLF), so a device that terminates every line with a bare `\r` and never
/// sends `\n` at all (common in embedded/RTOS printf implementations, e.g.
/// the Realtek RTL8735B this issue was filed against) never completed a
/// single line: `wait_for` could never match, and the growing partial
/// buffer never had a bound (see [`MAX_PARTIAL_BYTES`]).
///
/// [`LineTerminatorMode::Auto`] (the default) is a per-device *persisted*
/// choice — see [`crate::device_profile::DeviceProfile::line_terminator`] —
/// so a device that's already been auto-detected once doesn't need to pay
/// the detection probe again after a reconnect, and a device whose stream
/// is genuinely ambiguous (see [`Partial`]'s docs) can be pinned explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LineTerminatorMode {
    /// Detect from the stream itself — see [`Partial`]'s docs for the exact
    /// heuristic. What every device starts as; this is also what makes
    /// existing LF/CRLF devices' behavior byte-for-byte unchanged by this
    /// fix, since the very first `\n` any device ever sends resolves
    /// [`Terminator::Lf`] immediately and permanently.
    #[default]
    Auto,
    /// Force `\n`-terminated (an immediately preceding `\r` is still
    /// stripped, so this also covers CRLF) — skips detection entirely.
    Lf,
    /// Force `\r`-terminated (an immediately following `\n` is swallowed
    /// too) — skips detection entirely. For a device already known to be
    /// CR-only (or whose stream is too ambiguous for [`Auto`] to resolve
    /// confidently — see [`Partial`]'s docs), pinning this avoids the
    /// detection probe altogether.
    Cr,
}

/// A concrete (already-decided, non-[`LineTerminatorMode::Auto`]) line
/// terminator convention, and the actual byte-splitting logic for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Terminator {
    /// `\n` ends a line; an immediately preceding `\r` is part of the
    /// terminator too (CRLF), not the line's content.
    Lf,
    /// `\r` ends a line; an immediately following `\n` is part of the
    /// terminator too (so an explicit `Cr` override still handles a
    /// genuine CRLF stream correctly, not just bare-CR).
    Cr,
    /// Either byte ends a line, and `\r\n` counts as one terminator. This is
    /// what [`LineTerminatorMode::Auto`] resolves to, and it exists because
    /// real firmware mixes conventions within a single stream: the device in
    /// issue #52 ends most lines with a bare `\r` but some — its periodic
    /// statistics lines — with a bare `\n`. Committing to one convention
    /// therefore cannot be right for the whole session no matter how well it
    /// is detected, which is why no amount of probe-scoring fixed this.
    ///
    /// Empty segments are dropped under this mode. A bare `\r` means "cursor
    /// to column 0", so `\r\r` is not two blank lines, and once both bytes
    /// are terminators there is no way to tell a `\n\n` blank line apart
    /// from that cursor movement. Blank lines carry essentially no signal in
    /// a serial log while the false ones are constant, so dropping them all
    /// is the better trade; an operator who needs exact blank-line fidelity
    /// pins [`LineTerminatorMode::Lf`].
    Any,
}

impl Terminator {
    /// Split every complete line out of `buf`, in order, returning
    /// `(completed_lines, remainder)` — `remainder` is the new in-progress
    /// tail (possibly empty), exactly mirroring the old bare `if b ==
    /// b'\n'` loop's semantics but generalized to either terminator byte.
    ///
    /// A line's content never includes its own terminator bytes. For
    /// [`Terminator::Cr`], two consecutive `\r`s with nothing between them
    /// produce one empty line — the same thing a real terminal does with
    /// back-to-back carriage returns, and exactly what issue #52's own
    /// fixture data contains (`\r\r[Driver]: ...`).
    pub(crate) fn split(self, buf: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let mut lines = Vec::new();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < buf.len() {
            let is_terminator = match self {
                Terminator::Lf => buf[i] == b'\n',
                Terminator::Cr => buf[i] == b'\r',
                Terminator::Any => buf[i] == b'\n' || buf[i] == b'\r',
            };
            if !is_terminator {
                i += 1;
                continue;
            }
            let mut end = i;
            if self == Terminator::Lf && end > start && buf[end - 1] == b'\r' {
                end -= 1; // CRLF: the `\r` belongs to the terminator, not the content.
            }
            // A run of carriage returns is not a run of blank lines — see the
            // `Cr`/`Any` variant docs. `Lf` keeps its empty lines: there, a
            // blank line genuinely is one.
            let drops_empty = matches!(self, Terminator::Cr | Terminator::Any);
            if !(drops_empty && end == start) {
                lines.push(buf[start..end].to_vec());
            }
            let terminator_was_cr = buf[i] == b'\r';
            i += 1;
            if matches!(self, Terminator::Cr | Terminator::Any)
                && terminator_was_cr
                && i < buf.len()
                && buf[i] == b'\n'
            {
                i += 1; // CRLF: swallow the paired `\n` as part of one terminator.
            }
            start = i;
        }
        (lines, buf[start..].to_vec())
    }
}

/// Hard cap on an in-progress (not-yet-terminated) line's byte length,
/// independent of [`AUTO_DETECT_PROBE_CAP`] and regardless of
/// detected/configured mode. Crossing it force-completes whatever's
/// accumulated as one line — marked [`AssembledLine::capped`] — instead of
/// growing without bound (issue #52's third, lowest-severity consequence: a
/// CR-only device that, pre-fix, never produced a terminator at all could
/// grow `partial.buf` forever).
const MAX_PARTIAL_BYTES: usize = 64 * 1024;

/// In-progress, not-yet-terminated tail of rx bytes, plus this device's
/// line-terminator auto-detection state. Deliberately never exposed outside
/// this module: an in-progress line is not a candidate for `wait_for`, not
/// returned by `tail`/`read_since`, and not subject to a `Filter` — see the
/// module docs.
///
/// # Auto-detection strategy
///
/// While [`Self::resolved`] is `None` (only possible under
/// [`LineTerminatorMode::Auto`]), every incoming byte is inspected without
/// yet being treated as a terminator:
///
/// - The first `\n` byte ever seen resolves [`Terminator::Lf`] immediately
///   and permanently. This is deliberately asymmetric with the `\r` rule
///   below: seeing an actual `\n` is unambiguous proof this device uses
///   `\n`, and [`Terminator::Lf`]'s own splitting already tolerates an
///   optional preceding `\r` (i.e. it handles CRLF too), so there is no
///   ambiguity left to resolve once `\n` shows up even once.
/// - [`CR_AUTO_DETECT_THRESHOLD`] bare `\r` bytes with no `\n` seen yet
///   resolves [`Terminator::Cr`].
/// - Failing both, [`AUTO_DETECT_PROBE_CAP`] undecided bytes forces a
///   decision (`Cr` if any `\r` was seen, else the legacy `Lf` default).
///
/// Once resolved, the *entire* probe accumulated so far (not just the
/// triggering byte) is re-split under the now-known terminator — see
/// [`Terminator::split`] — so nothing before the decision point is lost or
/// misassembled, and detection never has to "look ahead" beyond bytes
/// already received.
///
/// # Genuinely mixed/ambiguous streams
///
/// A device is expected to use exactly one convention for its entire
/// session (real firmware does); this module does not attempt to
/// re-detect mid-stream once [`Self::resolved`] is set; [`Auto`]
/// [`LineTerminatorMode::Auto`] resolves once, then stays resolved for the
/// lifetime of this [`Partial`] (i.e. for as long as the owning
/// [`DeviceQueryState`] lives — across reconnects, since it's cached per
/// device, see `protocol::registry::QueryRegistry`). A device that
/// legitimately switches convention mid-session, or whose first bytes are
/// ambiguous enough to trip the wrong branch above (e.g. exactly one stray
/// leading `\r` followed by silence past [`AUTO_DETECT_PROBE_CAP`] before
/// any real newline), is exactly the case
/// [`LineTerminatorMode::Lf`]/[`LineTerminatorMode::Cr`] exists for: an
/// explicit per-device override in
/// [`crate::device_profile::DeviceProfile::line_terminator`] skips
/// detection entirely.
#[derive(Debug)]
struct Partial {
    buf: Vec<u8>,
    /// `None` only while the configured [`LineTerminatorMode`] was `Auto`
    /// and no decision has been forced yet — see the struct docs. Set
    /// immediately at construction for an explicit `Lf`/`Cr` override
    /// (detection never runs at all in that case).
    resolved: Terminator,
    /// Only meaningful once `resolved == Some(Terminator::Cr)`: set right
    /// after a `\r` completes a line, so that *if* the very next byte
    /// turns out to be `\n`, it's swallowed as the second half of a CRLF
    /// pair instead of starting a new (empty) line. Needed because bytes
    /// arrive one at a time across separate [`Self::push`] calls — whether
    /// a `\r` is paired with a following `\n` can't always be decided
    /// within the same call that saw the `\r`.
    swallow_next_lf: bool,
}

impl Default for Partial {
    fn default() -> Self {
        Self::new(LineTerminatorMode::Auto)
    }
}

impl Partial {
    fn new(configured: LineTerminatorMode) -> Self {
        // `Auto` no longer probes: it resolves immediately to `Any`, which
        // treats both bytes as terminators. Detection was the wrong shape for
        // the problem — see [`Terminator::Any`] — because a device that mixes
        // conventions has no single right answer to detect.
        let resolved = match configured {
            LineTerminatorMode::Lf => Terminator::Lf,
            LineTerminatorMode::Cr => Terminator::Cr,
            LineTerminatorMode::Auto => Terminator::Any,
        };
        Self {
            buf: Vec::new(),
            resolved,
            swallow_next_lf: false,
        }
    }

    /// Feed one incoming byte. Returns every line completed as a result —
    /// usually 0 or 1, but an auto-detection decision landing on this byte
    /// can retroactively complete several at once (see the struct docs).
    /// Each returned line is `(raw_bytes, capped)`; `capped` is `true` only
    /// for a line forced out by [`MAX_PARTIAL_BYTES`] rather than an actual
    /// terminator.
    ///
    /// # Why this never rescans the whole buffer
    ///
    /// Once [`Self::resolved`] is known, appending one byte can complete at
    /// most one line, and deciding whether it does never requires looking
    /// at anything before it — so the per-byte fast path below only ever
    /// inspects the single incoming byte, never re-splits everything
    /// accumulated so far. That matters for more than tidiness: an earlier
    /// version of this function called [`Terminator::split`] (a full
    /// `O(buf.len())` scan) on *every* byte, which is quadratic in the
    /// length of the longest unterminated stretch — exactly the shape a
    /// [`MAX_PARTIAL_BYTES`]-sized run of data with no terminator has, and
    /// exactly what this project's own partial-cap tests construct.
    /// [`Terminator::split`] is still used, but only once: the single
    /// one-time pass over whatever's accumulated in the probe the instant
    /// `Auto` detection resolves (bounded by [`AUTO_DETECT_PROBE_CAP`], so
    /// still cheap).
    fn push(&mut self, b: u8) -> Vec<(Vec<u8>, bool)> {
        let mut out = Vec::new();

        let term = self.resolved;
        let cr_terminates = matches!(term, Terminator::Cr | Terminator::Any);
        if std::mem::take(&mut self.swallow_next_lf) && cr_terminates && b == b'\n' {
            // Swallowed: the second half of a CRLF pair, where the `\r`
            // already terminated its line on a previous call.
        } else {
            self.push_resolved_byte(term, b, &mut out);
        }

        if self.buf.len() > MAX_PARTIAL_BYTES {
            out.push((std::mem::take(&mut self.buf), true));
            self.swallow_next_lf = false;
        }

        out
    }

    /// Append `b` under an already-[`Self::resolved`] `term`, completing a
    /// line into `out` if `b` is that terminator — the `O(1)`-per-byte
    /// counterpart to [`Terminator::split`], covering the exact same rules
    /// (CRLF-stripping for `Lf`, paired-`\n`-swallowing for `Cr` via
    /// [`Self::swallow_next_lf`]) at single-byte granularity instead of a
    /// full-buffer scan. See [`Self::push`]'s docs for why this exists.
    fn push_resolved_byte(&mut self, term: Terminator, b: u8, out: &mut Vec<(Vec<u8>, bool)>) {
        let is_terminator = match term {
            Terminator::Lf => b == b'\n',
            Terminator::Cr => b == b'\r',
            Terminator::Any => b == b'\n' || b == b'\r',
        };
        if !is_terminator {
            self.buf.push(b);
            return;
        }
        let mut line = std::mem::take(&mut self.buf);
        if term == Terminator::Lf && line.last() == Some(&b'\r') {
            line.pop(); // CRLF: the `\r` belongs to the terminator, not the content.
        }
        // See `Terminator::split`: a stretch bounded by carriage returns is
        // cursor movement, not a blank line. `Lf` keeps its blank lines.
        let drops_empty = matches!(term, Terminator::Cr | Terminator::Any);
        if !(drops_empty && line.is_empty()) {
            out.push((line, false));
        }
        // Only a `\r` can be the first half of a CRLF pair whose `\n` might
        // arrive on a later call.
        if drops_empty && b == b'\r' {
            self.swallow_next_lf = true;
        }
    }
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
        /// `String::from_utf8_lossy` of the matched line's bytes — kept for
        /// convenience/back-compat, but see [`Self::Matched::raw`] for the
        /// byte-exact source of truth.
        line: String,
        /// The matched line's exact original bytes ([`AssembledLine::raw`]).
        ///
        /// Added alongside the T3.1 MCP bridge work (issue #13): `tail`/
        /// `read_since` already carry a line's real bytes via
        /// `AssembledLine::raw` (fixed for issue #32), but `wait_for` only
        /// ever matched against — and returned — the lossy `text`,
        /// discarding byte-exactness for the one line that satisfied the
        /// caller's pattern. Same bug, same fix, one milestone later.
        raw: Vec<u8>,
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
        Self::with_line_terminator(LineTerminatorMode::Auto)
    }

    /// Like [`Self::new`], but pinning the line-terminator convention
    /// instead of auto-detecting it — the per-device override path (issue
    /// #52): a caller that already knows a device's convention (e.g. from
    /// [`crate::device_profile::DeviceProfile::line_terminator`]) can skip
    /// [`Partial`]'s detection probe entirely.
    pub fn with_line_terminator(mode: LineTerminatorMode) -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            partial: Mutex::new(Partial::new(mode)),
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
                            for (raw, capped) in partial.push(b) {
                                let text = String::from_utf8_lossy(&raw).into_owned();
                                lines.push(AssembledLine {
                                    raw,
                                    text,
                                    seq: *seq,
                                    t_mono: *t_mono,
                                    t_wall: t_wall.clone(),
                                    capped,
                                });
                                added = true;
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
                    Record::Tx {
                        seq,
                        t_mono,
                        t_wall,
                        client,
                        client_type,
                        gate,
                        data_b64,
                    } => {
                        // T2.1 (issue #8): now that `write` actually lands
                        // (see `protocol::session`'s `Request::Write`
                        // handler), a `tx` record needs exactly the same
                        // "never filtered, always in the tail/subscribe
                        // stream" treatment `Gate` already gets above —
                        // "TX 事件入流，所有 viewer 即時看到回顯" is a T2.1
                        // acceptance criterion, and there is no other path
                        // into `tail`/`read_since`/`subscribe` than this
                        // `events` vec. `client` already carries
                        // `"name:pid"` (see `protocol::session`'s
                        // `changed_by` convention), so the kernel-verified
                        // identity travels with every tx event without
                        // needing a new wire field.
                        let mut extra = serde_json::Map::new();
                        extra.insert("client".to_string(), client.clone().into());
                        extra.insert(
                            "client_type".to_string(),
                            serde_json::to_value(client_type).unwrap_or(serde_json::Value::Null),
                        );
                        extra.insert("gate".to_string(), gate.clone().into());
                        extra.insert("data_b64".to_string(), data_b64.clone().into());
                        events.push(OobRecord {
                            seq: *seq,
                            t_mono: *t_mono,
                            t_wall: t_wall.clone(),
                            kind: Kind::Tx,
                            name: None,
                            extra,
                        });
                        added = true;
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

    /// [`Self::tail`], with the context-protection presentation layer
    /// (`TASKS.md` T3.2, issue #13) applied on top: duplicate-line folding,
    /// binary-ratio summarization, and an overall size cap with a
    /// correctness-preserving continuation cursor. See
    /// [`crate::presentation`]'s module docs for why this composition is
    /// safe and how a GUI backend embedded in this daemon (T5.2) is meant
    /// to call this directly, the same way the MCP bridge calls
    /// [`crate::presentation::present`] itself after reconstructing lines/
    /// events from the wire.
    pub fn tail_presented(
        &self,
        n: usize,
        filter: Option<&Filter>,
        limits: &crate::presentation::PresentationLimits,
    ) -> Result<crate::presentation::PresentedPage, QueryError> {
        let page = self.tail(n, filter)?;
        Ok(crate::presentation::present(
            &page.lines,
            &page.events,
            page.cursor,
            limits,
        ))
    }

    /// [`Self::read_since`], with the context-protection presentation layer
    /// applied on top — see [`Self::tail_presented`]'s docs.
    pub fn read_since_presented(
        &self,
        cursor: u64,
        max_bytes: Option<usize>,
        filter: Option<&Filter>,
        limits: &crate::presentation::PresentationLimits,
    ) -> Result<crate::presentation::PresentedPage, QueryError> {
        let page = self.read_since(cursor, max_bytes, filter)?;
        Ok(crate::presentation::present(
            &page.lines,
            &page.events,
            page.cursor,
            limits,
        ))
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
    fn find_match_from(&self, re: &Regex, from: usize) -> (Option<(String, Vec<u8>, u64)>, usize) {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let mut idx = from;
        while idx < lines.len() {
            let line = &lines[idx];
            idx += 1;
            if re.is_match(&line.text) {
                return (Some((line.text.clone(), line.raw.clone(), line.seq)), idx);
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
            if let Some((line, raw, seq)) = found {
                return Ok(WaitForOutcome::Matched {
                    line,
                    raw,
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
    fn tx_record_is_surfaced_as_an_oob_event_with_identity_and_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder
            .append_tx(
                b"status\n",
                "agent-writer:4242",
                wrap_proto::ClientType::Human,
                "human_rw",
            )
            .unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.events.len(), 1, "{:?}", page.events);
        let ev = &page.events[0];
        assert_eq!(ev.kind, Kind::Tx);
        assert_eq!(
            ev.extra.get("client").and_then(|v| v.as_str()),
            Some("agent-writer:4242")
        );
        assert_eq!(
            ev.extra.get("gate").and_then(|v| v.as_str()),
            Some("human_rw")
        );
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

    // ---- Issue #52: CR-only line assembly ----

    /// The exact bytes from issue #52's real-device capture (Realtek
    /// RTL8735B / AmebaPro2): every line starts with a bare `\r`, no `\n`
    /// anywhere in the whole session. Fed as five separate `append_rx`
    /// calls, mirroring the five separate reads the issue's own report
    /// lists (so this also exercises detection/assembly carrying state
    /// across chunk boundaries, not just within one buffer).
    #[test]
    fn issue_52_cr_only_device_assembles_the_real_reported_bytes_into_correct_text_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();

        let chunks: &[&[u8]] = &[
            b"\rosd_update_custom_init Jun  3 2026",
            b"\rosd ch 0 e1 num 24 (0, 1, 2)",
            b"\rosd_render_task start",
            b"\r\r[Driver]: TSFValue = 31802015744465, tsf = 0, shift_set= 0x8000",
            b"\r",
        ];
        for chunk in chunks {
            recorder.append_rx(chunk).unwrap();
            state.ingest(&recorder);
        }

        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();
        // The bare `\r` opening the session and the back-to-back `\r\r`
        // before "[Driver]" each bound an empty segment, which under `Cr` is
        // cursor movement rather than a blank line — see
        // `Terminator::split`'s docs. What must survive is every real
        // message, in order, with no merging and nothing lost.
        assert_eq!(
            texts,
            vec![
                "osd_update_custom_init Jun  3 2026",
                "osd ch 0 e1 num 24 (0, 1, 2)",
                "osd_render_task start",
                "[Driver]: TSFValue = 31802015744465, tsf = 0, shift_set= 0x8000",
            ],
            "{texts:?}"
        );
        assert!(
            state.line_count() >= 4,
            "the device's real log lines must have actually assembled, not just accumulated \
             forever in the partial buffer (the pre-fix bug). Four, not six: the empty segments \
             bounded by consecutive CRs are cursor movement and are no longer counted as lines"
        );
        for line in &page.lines {
            assert!(
                std::str::from_utf8(&line.raw).is_ok(),
                "a correctly CR-terminated line must never contain an embedded \\r: {:?}",
                line.raw
            );
            assert!(
                !line.text.contains('\r'),
                "text must not contain \\r: {:?}",
                line.text
            );
            assert!(
                !line.capped,
                "none of this fixture should ever hit the partial cap"
            );
        }
    }

    /// Issue #52 end to end, from the user's real recording: a capture that
    /// opens with flash-mode garbage — in which `\n` occurs by chance at byte
    /// offset 1, verbatim below — and then settles into the device's actual
    /// CR-only logging. Detection is *expected* to guess wrong on the garbage
    /// and then recover, which is the behaviour no one-shot decision can
    /// provide, and why this can't be fixed by tuning a threshold.
    #[test]
    fn garbage_prefix_then_cr_only_log_recovers_and_yields_clean_text() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();

        recorder
            .append_rx(b"\x84\n\x8c\xc6baA\x0117\xa9-\xfa\x05\xbc\x92\xc0F\xbdB\xa1\xfb")
            .unwrap();
        state.ingest(&recorder);

        for chunk in [
            &b"\rinterface 0 is initialized"[..],
            &b"\rinterface 1 is initialized"[..],
            &b"\rcfg_size_lib = 120, cfg_size_user = 120"[..],
            &b"\r\rInitializing WIFI ...[Driver]: [HALMAC]"[..],
            &b"\r HALMAC_MAJOR_VER = 1"[..],
            &b"\rosd_render_task start"[..],
            &b"\r\r[Local] Socket closed"[..],
            &b"\rrtsp_cmd_options"[..],
        ] {
            recorder.append_rx(chunk).unwrap();
            state.ingest(&recorder);
        }

        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();

        for expected in [
            "interface 1 is initialized",
            "cfg_size_lib = 120, cfg_size_user = 120",
            "osd_render_task start",
            "[Local] Socket closed",
        ] {
            assert!(
                texts.contains(&expected),
                "missing {expected:?} among {texts:?}"
            );
        }
        // An embedded control byte is what made every renderer fall back to a
        // hex dump, so no assembled line may still carry one.
        assert!(
            !texts.iter().any(|t| t.contains('\r')),
            "no assembled line may still contain a CR: {texts:?}"
        );
    }
    /// A stream that mixes a leading bare `\r` with `\n`-terminated lines —
    /// which under `Any` is not ambiguous at all: both bytes terminate, so
    /// the leading `\r` bounds an empty stretch that gets dropped and each
    /// message comes out clean. Before `Any`, detection had to *choose*, and
    /// choosing `Lf` here left the `\r` embedded in the first line's content
    /// (which is what made every renderer hex-dump it).
    #[test]
    fn a_leading_cr_before_lf_terminated_lines_yields_clean_content_either_way() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();
        recorder.append_rx(b"\rboot ok\n").unwrap();
        state.ingest(&recorder);
        recorder.append_rx(b"second line\n").unwrap();
        state.ingest(&recorder);

        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["boot ok", "second line"], "{texts:?}");
    }

    #[test]
    fn explicit_cr_override_skips_detection_from_the_very_first_byte() {
        // Same one-stray-leading-\r-then-\n shape as the auto-detect test
        // above, but this time with an explicit `Cr` override — proving
        // the override actually takes effect (bypasses the LF-wins-on-\n
        // heuristic entirely) rather than being inert configuration.
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::with_line_terminator(LineTerminatorMode::Cr);
        recorder.append_rx(b"\rfirst\rsecond\r").unwrap();
        state.ingest(&recorder);

        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();
        // Leading `\r` bounds an empty segment, which under `Cr` is cursor
        // movement, not a blank line — the override is proven effective by
        // the content splitting on `\r` at all (an LF reading would leave
        // one unterminated blob).
        assert_eq!(texts, vec!["first", "second"], "{texts:?}");
    }

    #[test]
    fn explicit_cr_override_swallows_a_paired_lf_even_when_it_arrives_in_a_separate_chunk() {
        // Regression coverage for a bug caught during review: an earlier
        // version of `Partial::push` called `Terminator::split` on every
        // byte, which only ever paired a `\r` with an immediately
        // following `\n` if *both* bytes were already sitting in the same
        // buffer at once. Since that version flushed the buffer the
        // instant the `\r` itself arrived, a `\n` arriving in a
        // *subsequent* `append_rx`/`push` call was never recognized as
        // paired — it leaked through as the start of a new, spurious empty
        // line instead of being swallowed. `swallow_next_lf` exists
        // specifically to carry that pairing decision across calls.
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::with_line_terminator(LineTerminatorMode::Cr);

        recorder.append_rx(b"first\r").unwrap();
        state.ingest(&recorder); // the `\r` arrives alone...
        recorder.append_rx(b"\nsecond\r").unwrap();
        state.ingest(&recorder); // ...and its paired `\n` arrives separately.

        let page = state.read_since(0, None, None).unwrap();
        let texts: Vec<&str> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["first", "second"],
            "the \\n must be swallowed as part of the CRLF pair even though it arrived in a \
             separate chunk from its \\r — no spurious empty line: {texts:?}"
        );
    }

    #[test]
    fn explicit_lf_override_never_auto_detects_cr_even_with_many_bare_crs() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::with_line_terminator(LineTerminatorMode::Lf);
        // Plenty of bare `\r`s (well past the auto-detect threshold) but no
        // `\n` at all yet — under `Auto` this would resolve `Cr`; under an
        // explicit `Lf` override it must not.
        recorder.append_rx(b"\r\r\r\r\rstill going").unwrap();
        state.ingest(&recorder);
        assert_eq!(
            state.line_count(),
            0,
            "an explicit Lf override must keep waiting for \\n regardless of \\r content"
        );
        recorder.append_rx(b"\n").unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 1);
        assert_eq!(page.lines[0].text, "\r\r\r\r\rstill going");
    }

    #[tokio::test]
    async fn wait_for_matches_on_a_cr_only_device() {
        // Issue #52 acceptance criterion: `wait_for` — the MCP bridge's
        // core primitive — must actually match on a CR-only device, not
        // just `read_since`/`tail`.
        let tmp = tempfile::tempdir().unwrap();
        let recorder = std::sync::Arc::new(recorder(tmp.path()));
        let state = std::sync::Arc::new(DeviceQueryState::new());

        let waiter = {
            let state = std::sync::Arc::clone(&state);
            tokio::spawn(async move { state.wait_for("boot ok", Duration::from_secs(2)).await })
        };
        // Deterministically let `waiter` reach its "checked" snapshot before
        // the match actually arrives — same reasoning as this file's other
        // `wait_for` tests just above (no fixed sleep).
        tokio::task::yield_now().await;
        recorder.append_rx(b"\rboot ok\r").unwrap();
        state.ingest(&recorder);

        let outcome = waiter.await.unwrap().unwrap();
        match outcome {
            WaitForOutcome::Matched { line, .. } => assert_eq!(line, "boot ok"),
            other => panic!("expected Matched, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_for_never_matches_an_unterminated_half_line_on_a_cr_only_device() {
        // The flip side of the previous test, and issue #52's explicit
        // "half-line semantics preserved" acceptance criterion: a pattern
        // that would match the *content* must still not fire until the
        // terminating `\r` actually arrives, on a CR-only device exactly
        // as it already does for LF (`assembles_only_complete_lines_and_
        // holds_back_the_partial_tail` above).
        let tmp = tempfile::tempdir().unwrap();
        let recorder = std::sync::Arc::new(recorder(tmp.path()));
        let state = std::sync::Arc::new(DeviceQueryState::new());
        // Establish CR-only mode up front: "noise" gives one real completed
        // line, and the second bare CR bounds only an empty segment, which
        // under `Cr` is cursor movement rather than a blank line — so one
        // known-good line total, making the half-line appended below
        // unambiguous.
        recorder.append_rx(b"noise\r\r").unwrap();
        state.ingest(&recorder);
        assert_eq!(
            state.line_count(),
            1,
            "sanity check on the CR-mode-establishing prefix"
        );

        let waiter = {
            let state = std::sync::Arc::clone(&state);
            tokio::spawn(async move { state.wait_for("boot ok", Duration::from_millis(80)).await })
        };
        // Same deterministic "let the waiter reach its checked snapshot
        // first" pattern as `wait_for_matches_on_a_cr_only_device` above.
        tokio::task::yield_now().await;
        recorder.append_rx(b"boot ok").unwrap(); // deliberately no trailing \r
        state.ingest(&recorder);

        let outcome = waiter.await.unwrap().unwrap();
        assert_eq!(
            state.line_count(),
            1,
            "only the one earlier established line — never the half-line"
        );
        match outcome {
            WaitForOutcome::TimedOut { .. } => {}
            other => panic!("expected TimedOut (half-line must never match), got {other:?}"),
        }
    }

    // ---- Issue #52: unbounded partial-buffer growth ----

    #[test]
    fn partial_buffer_beyond_the_cap_is_forced_into_one_capped_line() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();

        // No terminator anywhere — under the pre-#52 behavior this would
        // grow `partial.buf` forever. Exactly `MAX_PARTIAL_BYTES + 1` bytes
        // guarantees the cap triggers on the very last byte pushed, with
        // nothing left over in the new (post-flush) partial buffer, so the
        // forced line's length is exactly this payload's — a payload any
        // larger would still be handled correctly (no data ever dropped),
        // just split across more than one capped line, one per
        // `MAX_PARTIAL_BYTES`-sized span.
        let payload = vec![b'x'; MAX_PARTIAL_BYTES + 1];
        recorder.append_rx(&payload).unwrap();
        state.ingest(&recorder);

        assert_eq!(
            state.line_count(),
            1,
            "crossing the cap must force exactly one line out, not zero (unbounded growth) or \
             more than one"
        );
        let page = state.read_since(0, None, None).unwrap();
        let forced = &page.lines[0];
        assert!(forced.capped, "a cap-forced line must be flagged capped");
        assert_eq!(
            forced.raw.len(),
            MAX_PARTIAL_BYTES + 1,
            "no bytes may be dropped when force-completing"
        );

        // Assembly must continue working correctly afterwards — the cap
        // must not leave the assembler in a broken state.
        recorder.append_rx(b"back to normal\n").unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 2);
        assert!(!page.lines[1].capped);
        assert_eq!(page.lines[1].text, "back to normal");
    }

    #[test]
    fn a_payload_many_times_the_cap_splits_into_several_capped_lines_with_no_data_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = recorder(tmp.path());
        let state = DeviceQueryState::new();

        // Each capped flush fires as soon as the buffer *exceeds*
        // `MAX_PARTIAL_BYTES`, i.e. at exactly `MAX_PARTIAL_BYTES + 1`
        // bytes (see `Partial::push`) — so three flushes consume
        // `3 * (MAX_PARTIAL_BYTES + 1)` bytes, leaving a clean 7-byte
        // remainder still pending from this payload.
        let per_flush = MAX_PARTIAL_BYTES + 1;
        let payload = vec![b'y'; per_flush * 3 + 7];
        recorder.append_rx(&payload).unwrap();
        state.ingest(&recorder);

        let page = state.read_since(0, None, None).unwrap();
        // Three full-cap-sized capped lines, plus the 7-byte remainder
        // still pending (not yet capped — it hasn't crossed the threshold
        // itself) — nothing beyond that, and nothing dropped.
        assert_eq!(
            page.lines.len(),
            3,
            "{:?}",
            page.lines.iter().map(|l| l.raw.len()).collect::<Vec<_>>()
        );
        assert!(page.lines.iter().all(|l| l.capped));
        let total: usize = page.lines.iter().map(|l| l.raw.len()).sum();
        assert_eq!(
            total,
            per_flush * 3,
            "every capped line together must account for every byte up to the still-pending \
             remainder — no silent drops"
        );

        // The 7-byte remainder is still sitting in the partial buffer,
        // exactly as an ordinary half-line would be — completing it
        // normally must produce clean content, not corruption.
        recorder.append_rx(b"\n").unwrap();
        state.ingest(&recorder);
        let page = state.read_since(0, None, None).unwrap();
        assert_eq!(page.lines.len(), 4);
        assert!(!page.lines[3].capped);
        assert_eq!(page.lines[3].raw.len(), 7);
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

        // Unlike the protocol-/MCP-level tests of this same "append after
        // wait_for starts" shape (which cross a socket or subprocess
        // boundary and so need to confirm the real event — see issue #39),
        // this unit test only needs `waiter` to have reached its own
        // "checked" snapshot, which is a plain in-process tokio task with
        // no I/O in front of it. `yield_now` deterministically gives the
        // (default current-thread) runtime a chance to run it to its first
        // suspension point before we append — no guessed duration needed.
        tokio::task::yield_now().await;
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

    // ---- Issue #13 (T3.2): wait_for carries the matched line's exact raw
    // bytes, not just the lossy text ----

    #[tokio::test]
    async fn wait_for_matched_line_carries_exact_raw_bytes_for_invalid_utf8() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let recorder = std::sync::Arc::new(recorder(tmp.path()));
        let state = std::sync::Arc::new(DeviceQueryState::new());

        // Same fixture shape as `invalid_utf8_line_keeps_its_exact_original_bytes_in_raw`
        // above (issue #32's own scenario), but through `wait_for` instead
        // of `read_since`.
        let mut original = b"status:".to_vec();
        original.extend_from_slice(&[0xFF, 0xFE, 0x80]);
        let mut with_newline = original.clone();
        with_newline.push(b'\n');

        let waiter = {
            let state = std::sync::Arc::clone(&state);
            tokio::spawn(async move { state.wait_for("^status:", Duration::from_secs(2)).await })
        };

        // See the identical comment in
        // `wait_for_matches_a_line_that_completes_after_the_call_starts`
        // above: `yield_now` deterministically lets `waiter` reach its
        // "checked" snapshot instead of guessing a duration.
        tokio::task::yield_now().await;
        recorder.append_rx(&with_newline).unwrap();
        state.ingest(&recorder);

        let outcome = waiter.await.unwrap().unwrap();
        match outcome {
            WaitForOutcome::Matched { raw, seq, .. } => {
                assert_eq!(seq, 0);
                assert!(
                    std::str::from_utf8(&raw).is_err(),
                    "test fixture must actually be invalid UTF-8, or this test proves nothing"
                );
                assert_eq!(
                    raw, original,
                    "raw must be byte-for-byte identical to what was written, newline stripped"
                );

                let mut original_hasher = Sha256::new();
                original_hasher.update(&original);
                let mut raw_hasher = Sha256::new();
                raw_hasher.update(&raw);
                assert_eq!(
                    format!("{:x}", original_hasher.finalize()),
                    format!("{:x}", raw_hasher.finalize()),
                    "sha256(original) must match sha256(raw)"
                );
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
