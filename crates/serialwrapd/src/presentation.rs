//! Context-protection presentation layer (`TASKS.md` T3.2, issue #13):
//! duplicate-line folding, oversized/binary-heavy line summarization, and an
//! overall result-size cap with a correctness-preserving continuation
//! cursor. See the [Event stream and storage
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Event-stream-and-storage)'s
//! query layer section for the authoritative shape this implements: "For
//! agent clients, results are capped (8KB default) with a continuation
//! cursor," "a region whose proportion of invalid UTF-8 crosses a threshold
//! is presented as a length plus a hex preview of the first 64 bytes rather
//! than as replacement characters," and "three or more consecutive
//! identical lines collapse to one line plus a count and a time range."
//!
//! # Why this is a layer *on top of* the query layer, not inside it
//!
//! [`present`] never touches [`crate::query::DeviceQueryState`]'s own
//! cursor arithmetic. It takes the lines/events an already-correct
//! `tail`/`read_since` call produced (bounded however that call was bounded,
//! or not at all) plus that call's own already-correct cursor, and only
//! ever *narrows what's returned* the same way a [`wrap_proto::Filter`]
//! narrows lines without touching what's scanned (see `query.rs`'s module
//! docs: "out-of-band events are never filtered, only range-bounded" is the
//! same discipline this module extends to folding/summarizing/truncating).
//! The wiki states the resulting invariant directly: "Truncation and
//! repeated-line collapsing operate on the returned view, never on cursor
//! arithmetic. Reading a stream in chunks with cursors always yields
//! exactly the same records as reading it whole, regardless of how the view
//! was compressed for presentation."
//!
//! # Why this is reusable by both the MCP bridge and the future GUI (T5.2)
//!
//! Every type and function here is a pure transform over
//! [`crate::query::AssembledLine`]/[`crate::query::OobRecord`] slices plus a
//! plain `u64` cursor — no dependency on `DeviceQueryState`, a live
//! `Recorder`, or any wire/transport type. A GUI backend embedded in the
//! daemon (linking this crate directly) calls [`present`] against real
//! `AssembledLine`s straight out of `DeviceQueryState::tail`/`read_since`;
//! the MCP bridge (`crates/serialwrap/src/mcp`), running as a *separate*
//! process that only ever sees the daemon's wire JSON, reconstructs the same
//! `AssembledLine`/`OobRecord` values from that JSON (the wire already
//! carries every field losslessly — see `crate::protocol::session::line_json`'s
//! `raw_b64` rule) and calls the exact same [`present`] function. Either way
//! the folding/summarizing/truncating logic — and its cursor-correctness
//! guarantee — is written and tested exactly once, here.
//! [`DeviceQueryState::tail_presented`]/[`DeviceQueryState::read_since_presented`]
//! (in `query.rs`) are the convenience entry points for the "linked
//! in-process" case.
//!
//! # Cursor correctness: how folding and truncation compose safely
//!
//! Three invariants, each load-bearing:
//!
//! 1. **A fold block never straddles an out-of-band event's `seq`.**
//!    [`group_lines`] refuses to merge two adjacent identical-content lines
//!    into the same run if any event's `seq` falls in between them. Without
//!    this, a fold block's `[first_seq, last_seq]` span could numerically
//!    contain an event that the fold itself doesn't carry — and if a later
//!    size cap ever had to cut the page *between* that event and the fold
//!    block, no single cursor value could correctly describe "delivered
//!    the fold's lines but not the event in the middle" or vice versa
//!    without either skipping or re-delivering something. Breaking the run
//!    at every such boundary keeps every view item's seq range disjoint
//!    from every other's, which is what makes step 2 below sound.
//! 2. **Items are truncated in ascending start-seq order, one at a time**,
//!    exactly mirroring [`crate::query::DeviceQueryState::read_since`]'s own
//!    `max_bytes` loop (same "always include at least one item, for forward
//!    progress" guarantee — see that function's docs). Because item ranges
//!    are disjoint (invariant 1) and sorted, "included a contiguous prefix,
//!    stopped before some item" is always well-defined: nothing with a
//!    smaller `seq` than the stopping point was ever skipped.
//! 3. **The continuation cursor is one past the last included item's
//!    highest covered `seq`** — never the underlying `tail`/`read_since`
//!    call's own cursor once truncation actually happened. A caller who
//!    passes this cursor to the next `read_since`-shaped call resumes
//!    exactly where the presented view stopped, so pagination through
//!    [`present`] can never duplicate or drop a record even though what's
//!    "one record" from the cursor's perspective (one raw line) and from
//!    the view's perspective (a folded block of many, or a compact binary
//!    summary) differ.
//!
//! # Known limitation
//!
//! A single line whose *own* rendered JSON already exceeds
//! [`PresentationLimits::max_result_bytes`] (e.g. a very long line of valid
//! UTF-8 text with no invalid bytes and no duplicates to fold) is still
//! returned whole on its own page — the forward-progress guarantee (2
//! above) takes priority over the nominal cap for that one page. This
//! mirrors an accepted tradeoff `read_since` already makes for the same
//! reason.

use crate::query::{AssembledLine, OobRecord};

/// Tunable thresholds for [`present`]. Defaults follow the wiki's query
/// layer section: 8KB result cap, a 64-byte hex preview, 3+ consecutive
/// identical lines fold. The binary-ratio threshold itself isn't pinned to
/// an exact number by the wiki (it only says "crosses a threshold") — 30%
/// is this implementation's reasoned default: enough invalid-UTF-8 that the
/// lossy `text` rendering would be mostly replacement characters (i.e.
/// genuinely more noise than signal), while a mostly-valid line with only a
/// handful of corrupted bytes still renders as text, preserving whatever
/// signal it has. All four fields are meant to be relaxed by a caller (e.g.
/// an agent that explicitly wants more/rawer data for one call) — see
/// `crates/serialwrap/src/mcp/tools.rs`'s `tail`/`read_since` tool
/// arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentationLimits {
    /// Soft cap, in approximate serialized JSON bytes, on one presented
    /// page's total size. "Soft" because of the forward-progress guarantee
    /// — see the module docs' Known limitation.
    pub max_result_bytes: usize,
    /// A line whose invalid-UTF-8 byte proportion is strictly greater than
    /// this (0.0..=1.0) is summarized as [`LineRender::BinarySummary`]
    /// instead of shown as lossy text.
    pub binary_ratio_threshold: f64,
    /// How many leading bytes of a summarized binary line's hex preview to
    /// show.
    pub hex_preview_bytes: usize,
    /// Minimum run length of consecutive, identically-rendered lines that
    /// triggers folding into one [`PresentedLine::Fold`] entry. The wiki:
    /// "three or more consecutive identical lines."
    pub fold_min_run: usize,
}

impl Default for PresentationLimits {
    fn default() -> Self {
        Self {
            max_result_bytes: 8 * 1024,
            binary_ratio_threshold: 0.3,
            hex_preview_bytes: 64,
            fold_min_run: 3,
        }
    }
}

/// How one line's content is rendered, independent of whether it ends up as
/// a standalone [`PresentedLine::Single`] or folded into a
/// [`PresentedLine::Fold`].
#[derive(Debug, Clone, PartialEq)]
pub enum LineRender {
    /// Valid UTF-8, or invalid UTF-8 whose proportion stayed at/under
    /// [`PresentationLimits::binary_ratio_threshold`]. `raw_hex` mirrors the
    /// pre-T3.2 wire rule exactly (present iff the raw bytes aren't valid
    /// UTF-8, regardless of ratio) — a caller who never crosses the ratio
    /// threshold sees byte-for-byte the same shape T3.1 already shipped.
    Text {
        text: String,
        raw_hex: Option<String>,
    },
    /// Invalid-UTF-8 proportion crossed the threshold: a compact summary
    /// instead of the (potentially huge, and mostly noise) full text/hex.
    BinarySummary { length: usize, hex_preview: String },
}

/// One item of a [`PresentedPage`]'s `lines`.
#[derive(Debug, Clone, PartialEq)]
pub enum PresentedLine {
    Single {
        seq: u64,
        t_mono: f64,
        t_wall: String,
        render: LineRender,
    },
    /// `fold_min_run` or more consecutive lines that rendered identically,
    /// collapsed into one entry — see the wiki quote in the module docs.
    Fold {
        render: LineRender,
        count: usize,
        first_seq: u64,
        last_seq: u64,
        first_t_wall: String,
        last_t_wall: String,
    },
}

impl PresentedLine {
    /// Lowest raw `seq` this entry covers.
    pub fn first_seq(&self) -> u64 {
        match self {
            PresentedLine::Single { seq, .. } => *seq,
            PresentedLine::Fold { first_seq, .. } => *first_seq,
        }
    }

    /// Highest raw `seq` this entry covers.
    pub fn last_seq(&self) -> u64 {
        match self {
            PresentedLine::Single { seq, .. } => *seq,
            PresentedLine::Fold { last_seq, .. } => *last_seq,
        }
    }
}

/// Result of [`present`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PresentedPage {
    pub lines: Vec<PresentedLine>,
    pub events: Vec<OobRecord>,
    /// Resume point for the next `read_since`-shaped call. Equal to the
    /// `full_cursor` [`present`] was given whenever nothing had to be
    /// truncated for size; otherwise recomputed from the last item actually
    /// included — see the module docs' cursor-correctness section.
    pub cursor: u64,
    /// Whether [`PresentationLimits::max_result_bytes`] forced this page to
    /// stop short of everything `lines`/`events` were given.
    pub truncated: bool,
}

/// Proportion (0.0..=1.0) of `bytes` that lies within an invalid UTF-8
/// sequence. Walks the same valid-prefix/invalid-span decomposition
/// `std::str::from_utf8`'s `Utf8Error` already exposes, so a byte is only
/// ever counted once regardless of how many consecutive invalid bytes it's
/// part of.
fn invalid_utf8_ratio(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut invalid = 0usize;
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(_) => break,
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                rest = &rest[valid_up_to..];
                // `error_len() == None` means "the tail of `rest` looks like
                // the start of a valid sequence but was cut off" (i.e. every
                // remaining byte is part of the same trailing invalid
                // span) — treat the whole remainder as invalid.
                let bad_len = e.error_len().unwrap_or(rest.len()).max(1).min(rest.len());
                invalid += bad_len;
                rest = &rest[bad_len..];
            }
        }
    }
    invalid as f64 / bytes.len() as f64
}

fn hex_of(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_line(line: &AssembledLine, limits: &PresentationLimits) -> LineRender {
    if std::str::from_utf8(&line.raw).is_ok() {
        return LineRender::Text {
            text: line.text.clone(),
            raw_hex: None,
        };
    }
    let ratio = invalid_utf8_ratio(&line.raw);
    if ratio > limits.binary_ratio_threshold {
        LineRender::BinarySummary {
            length: line.raw.len(),
            hex_preview: hex_of(&line.raw[..line.raw.len().min(limits.hex_preview_bytes)]),
        }
    } else {
        LineRender::Text {
            text: line.text.clone(),
            raw_hex: Some(hex_of(&line.raw)),
        }
    }
}

/// `true` if any event's `seq` falls in the closed range `[lo, hi]`.
/// `event_seqs` must be sorted ascending (true by construction: events are
/// only ever appended to `DeviceQueryState::events` in increasing `seq`
/// order — see `query.rs`'s module docs).
fn any_event_seq_in_range(event_seqs: &[u64], lo: u64, hi: u64) -> bool {
    let start = event_seqs.partition_point(|&s| s < lo);
    start < event_seqs.len() && event_seqs[start] <= hi
}

/// A maximal run of adjacent, identically-rendered lines — `[start, end)`
/// indices into the `lines` slice [`present`] was given.
struct Group {
    render: LineRender,
    start: usize,
    end: usize,
}

/// Group `lines` into maximal adjacent runs of identical [`LineRender`],
/// never merging across a `seq` an out-of-band event occupies — see the
/// module docs' cursor-correctness invariant 1.
fn group_lines(
    lines: &[AssembledLine],
    event_seqs: &[u64],
    limits: &PresentationLimits,
) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let render = render_line(&lines[i], limits);
        let mut j = i + 1;
        while j < lines.len() {
            let next = render_line(&lines[j], limits);
            if next != render || any_event_seq_in_range(event_seqs, lines[i].seq, lines[j].seq) {
                break;
            }
            j += 1;
        }
        groups.push(Group {
            render,
            start: i,
            end: j,
        });
        i = j;
    }
    groups
}

fn expand_group(
    g: &Group,
    lines: &[AssembledLine],
    limits: &PresentationLimits,
) -> Vec<PresentedLine> {
    let run_len = g.end - g.start;
    if run_len >= limits.fold_min_run {
        vec![PresentedLine::Fold {
            render: g.render.clone(),
            count: run_len,
            first_seq: lines[g.start].seq,
            last_seq: lines[g.end - 1].seq,
            first_t_wall: lines[g.start].t_wall.clone(),
            last_t_wall: lines[g.end - 1].t_wall.clone(),
        }]
    } else {
        (g.start..g.end)
            .map(|idx| PresentedLine::Single {
                seq: lines[idx].seq,
                t_mono: lines[idx].t_mono,
                t_wall: lines[idx].t_wall.clone(),
                render: g.render.clone(),
            })
            .collect()
    }
}

enum ViewItem {
    Line(PresentedLine),
    Event(OobRecord),
}

impl ViewItem {
    fn start_seq(&self) -> u64 {
        match self {
            ViewItem::Line(l) => l.first_seq(),
            ViewItem::Event(e) => e.seq,
        }
    }

    fn end_seq(&self) -> u64 {
        match self {
            ViewItem::Line(l) => l.last_seq(),
            ViewItem::Event(e) => e.seq,
        }
    }

    fn json_size(&self) -> usize {
        // +1 for the array separator (comma/whitespace) — not exact, but a
        // stable, cheap, slightly-conservative per-item estimate; the real
        // cap enforcement this feeds is a soft one regardless (see the
        // module docs' Known limitation).
        match self {
            ViewItem::Line(l) => line_to_json(l).to_string().len() + 1,
            ViewItem::Event(e) => event_to_json(e).to_string().len() + 1,
        }
    }
}

/// Apply duplicate-line folding, binary summarization, and the overall size
/// cap to `lines`/`events` — the complete raw output of one `tail`/
/// `read_since`-shaped query, plus `full_cursor` (that same query's own
/// already-correct next cursor). See the module docs for the full
/// cursor-correctness argument.
pub fn present(
    lines: &[AssembledLine],
    events: &[OobRecord],
    full_cursor: u64,
    limits: &PresentationLimits,
) -> PresentedPage {
    let event_seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let groups = group_lines(lines, &event_seqs, limits);

    let mut items: Vec<ViewItem> = Vec::new();
    for g in &groups {
        for presented in expand_group(g, lines, limits) {
            items.push(ViewItem::Line(presented));
        }
    }
    for e in events {
        items.push(ViewItem::Event(e.clone()));
    }
    // Ranges are pairwise disjoint by construction (invariant 1 in the
    // module docs), so sorting by start == sorting by end; ascending
    // start-seq order is what makes the truncation loop below safe.
    items.sort_by_key(ViewItem::start_seq);

    let mut out_lines = Vec::new();
    let mut out_events = Vec::new();
    let mut bytes_used = 0usize;
    let mut truncated = false;
    let mut cursor = full_cursor;

    for item in items {
        let already_has_something = !out_lines.is_empty() || !out_events.is_empty();
        let size = item.json_size();
        if already_has_something && bytes_used + size > limits.max_result_bytes {
            truncated = true;
            break;
        }
        bytes_used += size;
        cursor = item.end_seq() + 1;
        match item {
            ViewItem::Line(l) => out_lines.push(l),
            ViewItem::Event(e) => out_events.push(e),
        }
    }

    if !truncated {
        cursor = full_cursor;
    }

    PresentedPage {
        lines: out_lines,
        events: out_events,
        cursor,
        truncated,
    }
}

fn render_into(obj: &mut serde_json::Map<String, serde_json::Value>, render: &LineRender) {
    match render {
        LineRender::Text { text, raw_hex } => {
            obj.insert("text".to_string(), text.clone().into());
            obj.insert("binary".to_string(), raw_hex.is_some().into());
            if let Some(hex) = raw_hex {
                obj.insert("raw_hex".to_string(), hex.clone().into());
            }
        }
        LineRender::BinarySummary {
            length,
            hex_preview,
        } => {
            obj.insert("binary".to_string(), true.into());
            obj.insert(
                "binary_summary".to_string(),
                serde_json::json!({"length": length, "hex_preview": hex_preview}),
            );
        }
    }
}

/// JSON shape for one [`PresentedLine`]. A [`PresentedLine::Single`] with a
/// [`LineRender::Text`] renders to *exactly* the pre-T3.2 wire shape
/// (`seq`/`t_mono`/`t_wall`/`text`/`binary`/`raw_hex`?) — see the module
/// docs — so existing consumers who never trigger folding or the ratio
/// threshold see no change at all. [`PresentedLine::Fold`] is the new shape,
/// tagged `"folded": true`.
pub fn line_to_json(line: &PresentedLine) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match line {
        PresentedLine::Single {
            seq,
            t_mono,
            t_wall,
            render,
        } => {
            obj.insert("seq".to_string(), (*seq).into());
            obj.insert("t_mono".to_string(), (*t_mono).into());
            obj.insert("t_wall".to_string(), t_wall.clone().into());
            render_into(&mut obj, render);
        }
        PresentedLine::Fold {
            render,
            count,
            first_seq,
            last_seq,
            first_t_wall,
            last_t_wall,
        } => {
            obj.insert("folded".to_string(), true.into());
            obj.insert("count".to_string(), (*count as u64).into());
            obj.insert("first_seq".to_string(), (*first_seq).into());
            obj.insert("last_seq".to_string(), (*last_seq).into());
            obj.insert("first_t_wall".to_string(), first_t_wall.clone().into());
            obj.insert("last_t_wall".to_string(), last_t_wall.clone().into());
            render_into(&mut obj, render);
        }
    }
    serde_json::Value::Object(obj)
}

/// JSON shape for one [`OobRecord`] — identical to
/// `crate::protocol::session`'s private `oob_json`, duplicated here (rather
/// than shared) so this module stays independent of the wire-protocol layer
/// entirely; both must stay in sync with the wiki's event-record shape.
pub fn event_to_json(e: &OobRecord) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("seq".to_string(), e.seq.into());
    obj.insert("t_mono".to_string(), e.t_mono.into());
    obj.insert("t_wall".to_string(), e.t_wall.clone().into());
    obj.insert(
        "kind".to_string(),
        serde_json::to_value(e.kind).unwrap_or(serde_json::Value::Null),
    );
    if let Some(name) = &e.name {
        obj.insert("event".to_string(), name.clone().into());
    }
    for (k, v) in &e.extra {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
    serde_json::Value::Object(obj)
}

/// Full JSON shape for a [`PresentedPage`] — what a caller that wants the
/// default wire-shaped rendering (e.g. the MCP bridge) hands back as a tool
/// result body, merged with whatever else that tool call also needs (its
/// own `device`, etc).
pub fn page_to_json(page: &PresentedPage) -> serde_json::Value {
    serde_json::json!({
        "lines": page.lines.iter().map(line_to_json).collect::<Vec<_>>(),
        "events": page.events.iter().map(event_to_json).collect::<Vec<_>>(),
        "cursor": page.cursor,
        "truncated": page.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(seq: u64, raw: &[u8]) -> AssembledLine {
        AssembledLine {
            raw: raw.to_vec(),
            text: String::from_utf8_lossy(raw).into_owned(),
            seq,
            t_mono: seq as f64,
            t_wall: format!("t{seq}"),
            capped: false,
        }
    }

    fn event(seq: u64) -> OobRecord {
        OobRecord {
            seq,
            t_mono: seq as f64,
            t_wall: format!("t{seq}"),
            kind: wrap_proto::Kind::Event,
            name: Some("disconnect".to_string()),
            extra: serde_json::Map::new(),
        }
    }

    // ---- invalid_utf8_ratio ----

    #[test]
    fn ratio_is_zero_for_empty_and_fully_valid_input() {
        assert_eq!(invalid_utf8_ratio(b""), 0.0);
        assert_eq!(invalid_utf8_ratio(b"hello world"), 0.0);
    }

    #[test]
    fn ratio_is_one_for_fully_invalid_input() {
        assert_eq!(invalid_utf8_ratio(&[0xFF, 0xFE, 0xFD]), 1.0);
    }

    #[test]
    fn ratio_counts_only_the_invalid_bytes() {
        // 2 invalid bytes out of 6 total.
        let bytes = [b'a', b'b', 0xFF, 0xFE, b'c', b'd'];
        assert!((invalid_utf8_ratio(&bytes) - 2.0 / 6.0).abs() < 1e-9);
    }

    // ---- render_line / binary threshold ----

    #[test]
    fn valid_utf8_renders_as_plain_text_never_binary() {
        let limits = PresentationLimits::default();
        let rendered = render_line(&line(0, b"hello"), &limits);
        assert_eq!(
            rendered,
            LineRender::Text {
                text: "hello".to_string(),
                raw_hex: None
            }
        );
    }

    // ---- Issue #52 acceptance criterion: clean ASCII with a CR terminator
    // is never presented as binary ----

    /// A correctly CR-terminated line has no embedded `\r` at all (the CR
    /// was the terminator, stripped by `query::Terminator::split` before it
    /// ever reaches `AssembledLine::raw`) — so once `serialwrapd::query`
    /// correctly recognizes CR as a line terminator (this same issue #52),
    /// this content is *always* valid UTF-8, and `render_line` never even
    /// reaches the invalid-UTF-8 ratio/threshold logic below, regardless of
    /// what that threshold is set to. This test pins that down with the
    /// issue's own real device content (a 35-byte line from the RTL8735B
    /// capture, minus the leading `\r` the query layer now strips as the
    /// terminator).
    #[test]
    fn issue_52_a_correctly_assembled_cr_terminated_line_renders_as_plain_text() {
        let limits = PresentationLimits::default();
        let raw = b"osd_update_custom_init Jun  3 2026";
        assert!(
            std::str::from_utf8(raw).is_ok(),
            "sanity: this fixture must actually be valid UTF-8"
        );
        let rendered = render_line(&line(0, raw), &limits);
        assert_eq!(
            rendered,
            LineRender::Text {
                text: "osd_update_custom_init Jun  3 2026".to_string(),
                raw_hex: None,
            },
            "clean ASCII (even from a device whose line-ending convention was CR) must never \
             render as BinarySummary"
        );
    }

    /// Diagnosis note for issue #52's "35 bytes, 1 control char (2.9%),
    /// crosses a 30% threshold?" report: this ratio-based threshold isn't
    /// actually what misclassified the user's log as binary. A bare `\r`
    /// (0x0D) is a *valid* single-byte UTF-8 codepoint, so `render_line`'s
    /// `std::str::from_utf8(&line.raw).is_ok()` check is `true` for content
    /// like this even with an embedded `\r` still in it (e.g. before this
    /// issue's line-assembly fix, when the query layer had no CR-as-
    /// terminator recognition at all) — `invalid_utf8_ratio`'s 30%
    /// threshold is never even evaluated. The actual "binary" hex-dump the
    /// user saw came from a *different*, independent, zero-tolerance
    /// mechanism: `serialwrap`'s CLI-only `cli::render::is_terminal_safe`,
    /// which flags *any* control byte other than tab, regardless of ratio —
    /// see that function's module for why it's out of this fix's scope.
    /// This test just pins down the presentation-layer half of that
    /// diagnosis: a single embedded control byte alone never crosses this
    /// module's ratio threshold merely by being a control byte, only by
    /// being part of *invalid* UTF-8.
    #[test]
    fn a_single_embedded_cr_byte_alone_never_crosses_the_ratio_threshold() {
        let limits = PresentationLimits::default();
        let mut raw = b"\r".to_vec();
        raw.extend_from_slice(b"osd_update_custom_init Jun  3 2026");
        assert!(
            std::str::from_utf8(&raw).is_ok(),
            "a bare \\r is valid single-byte UTF-8 — this must hold for the diagnosis to apply"
        );
        assert_eq!(
            invalid_utf8_ratio(&raw),
            0.0,
            "a control byte that's still valid UTF-8 contributes nothing to the invalid-UTF-8 \
             ratio — confirms this module's ratio threshold was never the mechanism that \
             misclassified issue #52's log lines"
        );
        assert_eq!(
            render_line(&line(0, &raw), &limits),
            LineRender::Text {
                text: String::from_utf8_lossy(&raw).into_owned(),
                raw_hex: None,
            }
        );
    }

    #[test]
    fn low_ratio_invalid_utf8_keeps_full_raw_hex_matching_pre_t3_2_shape() {
        // Mirrors the T3.1 acceptance test's payload: "prefix-" + 3 invalid
        // bytes + "-suffix" = 18 bytes, ratio ~17%, below the 30% default
        // threshold. Must still be flagged binary with the FULL exact hex,
        // not summarized — existing consumers rely on this.
        let mut raw = b"prefix-".to_vec();
        raw.extend_from_slice(&[0xFF, 0xFE, 0x80]);
        raw.extend_from_slice(b"-suffix");
        let limits = PresentationLimits::default();
        let rendered = render_line(&line(0, &raw), &limits);
        match rendered {
            LineRender::Text { raw_hex, .. } => {
                let hex = raw_hex.expect("low-ratio invalid utf8 must still carry raw_hex");
                assert_eq!(hex, hex_of(&raw));
            }
            other => panic!("expected Text{{raw_hex: Some}}, got {other:?}"),
        }
    }

    #[test]
    fn high_ratio_invalid_utf8_becomes_a_compact_binary_summary() {
        // 256-byte line cycling every byte value except the 0x0A line
        // terminator -- overwhelmingly invalid UTF-8 (lone continuation/
        // leading bytes with no valid multi-byte partners).
        let raw: Vec<u8> = (0u16..256).filter(|&b| b != 10).map(|b| b as u8).collect();
        let limits = PresentationLimits::default();
        assert!(
            invalid_utf8_ratio(&raw) > limits.binary_ratio_threshold,
            "test fixture must actually cross the threshold"
        );
        let rendered = render_line(&line(0, &raw), &limits);
        match rendered {
            LineRender::BinarySummary {
                length,
                hex_preview,
            } => {
                assert_eq!(length, raw.len());
                assert_eq!(hex_preview, hex_of(&raw[..64]));
            }
            other => panic!("expected BinarySummary, got {other:?}"),
        }
    }

    // ---- folding ----

    #[test]
    fn two_identical_lines_do_not_fold() {
        let lines = vec![line(0, b"same"), line(1, b"same")];
        let page = present(&lines, &[], 2, &PresentationLimits::default());
        assert_eq!(page.lines.len(), 2, "{:?}", page.lines);
        assert!(page
            .lines
            .iter()
            .all(|l| matches!(l, PresentedLine::Single { .. })));
    }

    #[test]
    fn three_identical_lines_fold_into_one_entry_with_correct_count_and_range() {
        let lines = vec![line(0, b"same"), line(1, b"same"), line(2, b"same")];
        let page = present(&lines, &[], 3, &PresentationLimits::default());
        assert_eq!(page.lines.len(), 1, "{:?}", page.lines);
        match &page.lines[0] {
            PresentedLine::Fold {
                count,
                first_seq,
                last_seq,
                ..
            } => {
                assert_eq!(*count, 3);
                assert_eq!(*first_seq, 0);
                assert_eq!(*last_seq, 2);
            }
            other => panic!("expected Fold, got {other:?}"),
        }
    }

    #[test]
    fn a_hundred_thousand_identical_lines_fold_with_the_exact_count() {
        let lines: Vec<AssembledLine> = (0..100_000u64).map(|i| line(i, b"read timeout")).collect();
        let page = present(&lines, &[], 100_000, &PresentationLimits::default());
        assert_eq!(page.lines.len(), 1, "{:?}", page.lines.len());
        match &page.lines[0] {
            PresentedLine::Fold { count, .. } => assert_eq!(*count, 100_000),
            other => panic!("expected Fold, got {other:?}"),
        }
    }

    #[test]
    fn a_dissimilar_line_breaks_the_run() {
        let lines = vec![
            line(0, b"same"),
            line(1, b"same"),
            line(2, b"same"),
            line(3, b"different"),
            line(4, b"same"),
        ];
        let page = present(&lines, &[], 5, &PresentationLimits::default());
        // fold(0..=2) + single(3) + single(4)
        assert_eq!(page.lines.len(), 3, "{:?}", page.lines);
        assert!(matches!(
            page.lines[0],
            PresentedLine::Fold { count: 3, .. }
        ));
        assert!(matches!(page.lines[1], PresentedLine::Single { .. }));
        assert!(matches!(page.lines[2], PresentedLine::Single { .. }));
    }

    #[test]
    fn folding_never_merges_across_an_interleaved_event() {
        // Five identical lines with an event sitting at seq 2, strictly
        // between two of them -- must not produce one Fold spanning
        // [0,4], since that range would silently swallow the event's seq.
        let lines = vec![
            line(0, b"same"),
            line(1, b"same"),
            line(3, b"same"),
            line(4, b"same"),
            line(5, b"same"),
        ];
        let events = vec![event(2)];
        let page = present(&lines, &events, 6, &PresentationLimits::default());
        for l in &page.lines {
            if let PresentedLine::Fold {
                first_seq,
                last_seq,
                ..
            } = l
            {
                assert!(
                    !(*first_seq <= 2 && 2 <= *last_seq),
                    "a fold must never straddle the event's seq: {first_seq}..={last_seq}"
                );
            }
        }
        assert_eq!(page.events.len(), 1);
    }

    // ---- size cap / truncation / cursor ----

    #[test]
    fn no_truncation_needed_passes_the_original_cursor_through_unchanged() {
        let lines = vec![line(0, b"a"), line(1, b"b")];
        let page = present(&lines, &[], 2, &PresentationLimits::default());
        assert!(!page.truncated);
        assert_eq!(page.cursor, 2);
    }

    #[test]
    fn a_fold_block_that_does_not_fit_is_excluded_entirely_not_split() {
        // 1000 identical lines (fold to one block), then one more, distinct
        // line after it — the fold block alone already exhausts the tiny
        // budget, so the trailing line must be excluded whole, never a
        // partial fold.
        let mut lines: Vec<AssembledLine> = (0..1000u64)
            .map(|i| line(i, b"this is a moderately long repeated line of text"))
            .collect();
        lines.push(line(1000, b"trailing distinct line"));
        let tight_limits = PresentationLimits {
            max_result_bytes: 100, // smaller than even one fold entry's JSON
            ..PresentationLimits::default()
        };
        let page = present(&lines, &[], 1001, &tight_limits);
        assert!(page.truncated);
        // Forward-progress guarantee: the one fold block is still included
        // whole (never partially), and nothing past it leaks in.
        assert_eq!(page.lines.len(), 1);
        match &page.lines[0] {
            PresentedLine::Fold {
                count,
                first_seq,
                last_seq,
                ..
            } => {
                assert_eq!(*count, 1000);
                assert_eq!(*first_seq, 0);
                assert_eq!(*last_seq, 999);
            }
            other => panic!("expected Fold, got {other:?}"),
        }
        assert_eq!(
            page.cursor, 1000,
            "cursor must point past the whole included fold, not into the excluded trailing line"
        );
    }

    #[test]
    fn truncation_stops_before_an_item_that_would_exceed_the_cap_and_cursor_matches() {
        // Distinct (non-folding) lines, tight enough that only the first
        // couple fit.
        let lines: Vec<AssembledLine> = (0..50u64)
            .map(|i| {
                line(
                    i,
                    format!("line number {i} with some padding text").as_bytes(),
                )
            })
            .collect();
        let tight_limits = PresentationLimits {
            max_result_bytes: 200,
            ..PresentationLimits::default()
        };
        let page = present(&lines, &[], 50, &tight_limits);
        assert!(page.truncated);
        assert!(page.lines.len() < 50, "{}", page.lines.len());
        let last_included_seq = page.lines.last().unwrap().last_seq();
        assert_eq!(page.cursor, last_included_seq + 1);
        // Nothing beyond the cursor was silently included.
        assert!(page.lines.iter().all(|l| l.last_seq() < page.cursor));
    }

    // ---- cursor equivalence: paginated reads must match one whole read ----

    /// Expand a [`PresentedPage`]'s lines into a `(seq, rendering)` trace —
    /// one entry per underlying raw seq a line/fold covers, with the fold's
    /// shared rendering repeated for every seq in its range. Two different
    /// paginations of the same underlying stream must produce identical
    /// traces regardless of exactly where fold boundaries happened to fall,
    /// because content is homogeneous within a fold by construction.
    fn expand_trace(lines: &[PresentedLine]) -> Vec<(u64, LineRender)> {
        let mut trace = Vec::new();
        for l in lines {
            match l {
                PresentedLine::Single { seq, render, .. } => trace.push((*seq, render.clone())),
                PresentedLine::Fold {
                    render,
                    first_seq,
                    last_seq,
                    ..
                } => {
                    for seq in *first_seq..=*last_seq {
                        trace.push((seq, render.clone()));
                    }
                }
            }
        }
        trace
    }

    #[test]
    fn paginated_reads_via_cursor_are_fully_equivalent_to_one_whole_read() {
        // A deliberately awkward mix: singles, a 3-run, a 10-run interrupted
        // partway through by an out-of-band event, a binary line, and
        // another dup run. `seq` is one single monotonically increasing
        // counter shared by *both* lines and the event — exactly like the
        // real system's single per-device sequence space, where a line's
        // seq and an event's seq can never coincide.
        let mut lines = Vec::new();
        let mut events = Vec::new();
        let mut seq = 0u64;
        let push_line = |raw: &[u8], seq: &mut u64, lines: &mut Vec<AssembledLine>| {
            lines.push(line(*seq, raw));
            *seq += 1;
        };
        push_line(b"alpha", &mut seq, &mut lines);
        push_line(b"beta", &mut seq, &mut lines);
        push_line(b"dup", &mut seq, &mut lines);
        push_line(b"dup", &mut seq, &mut lines);
        push_line(b"dup", &mut seq, &mut lines);
        push_line(b"gamma", &mut seq, &mut lines);
        // First 2 of the would-be 10-run...
        push_line(b"noisy repeat", &mut seq, &mut lines);
        push_line(b"noisy repeat", &mut seq, &mut lines);
        // ...an event lands here, consuming its own seq slot...
        events.push(event(seq));
        seq += 1;
        // ...then the remaining 8 of the run.
        for _ in 0..8 {
            push_line(b"noisy repeat", &mut seq, &mut lines);
        }
        push_line(&[0xFFu8, 0xFE, 0xFD, 0xFC, 0x01], &mut seq, &mut lines); // high-ratio binary
        push_line(b"delta", &mut seq, &mut lines);
        push_line(b"dup2", &mut seq, &mut lines);
        push_line(b"dup2", &mut seq, &mut lines);
        push_line(b"dup2", &mut seq, &mut lines);
        push_line(b"dup2", &mut seq, &mut lines);
        push_line(b"epsilon", &mut seq, &mut lines);

        let tip_cursor = seq;
        let limits = PresentationLimits::default();

        // (a) one whole, unbounded read.
        let whole = present(&lines, &events, tip_cursor, &limits);
        assert!(!whole.truncated);
        let whole_trace = expand_trace(&whole.lines);

        // (b) many small paginated reads, following the returned cursor —
        // exactly what an MCP/GUI client does against read_since.
        let tiny_limits = PresentationLimits {
            max_result_bytes: 40, // forces many pages
            ..PresentationLimits::default()
        };
        let mut cursor = 0u64;
        let mut paginated_line_trace = Vec::new();
        let mut paginated_events = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 10_000, "must terminate; possible infinite loop");
            let remaining_lines: Vec<AssembledLine> =
                lines.iter().filter(|l| l.seq >= cursor).cloned().collect();
            let remaining_events: Vec<OobRecord> =
                events.iter().filter(|e| e.seq >= cursor).cloned().collect();
            if remaining_lines.is_empty() && remaining_events.is_empty() {
                break;
            }
            let page = present(
                &remaining_lines,
                &remaining_events,
                tip_cursor,
                &tiny_limits,
            );
            assert!(
                page.cursor > cursor || (page.lines.is_empty() && page.events.is_empty()),
                "cursor must always advance to make progress"
            );
            paginated_line_trace.extend(expand_trace(&page.lines));
            paginated_events.extend(page.events.iter().cloned());
            cursor = page.cursor;
        }

        assert_eq!(
            paginated_line_trace, whole_trace,
            "paginated per-seq content trace must exactly match the whole read"
        );
        assert_eq!(
            paginated_events, whole.events,
            "paginated events must exactly match the whole read, no dup/no gap"
        );

        // And, at the raw-seq level: the set of seqs covered is identical
        // and each covered exactly once (no dup, no gap).
        let whole_seqs: Vec<u64> = whole_trace.iter().map(|(s, _)| *s).collect();
        let paginated_seqs: Vec<u64> = paginated_line_trace.iter().map(|(s, _)| *s).collect();
        assert_eq!(whole_seqs, paginated_seqs);
    }

    // ---- JSON shape backward-compatibility ----

    #[test]
    fn single_text_line_json_shape_matches_pre_t3_2_wire_fields() {
        let l = PresentedLine::Single {
            seq: 5,
            t_mono: 1.5,
            t_wall: "t5".to_string(),
            render: LineRender::Text {
                text: "hello".to_string(),
                raw_hex: None,
            },
        };
        let json = line_to_json(&l);
        assert_eq!(json["seq"], 5);
        assert_eq!(json["t_mono"], 1.5);
        assert_eq!(json["t_wall"], "t5");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["binary"], false);
        assert!(json.get("raw_hex").is_none());
        assert!(json.get("folded").is_none());
    }

    #[test]
    fn fold_json_shape_carries_count_and_range() {
        let l = PresentedLine::Fold {
            render: LineRender::Text {
                text: "dup".to_string(),
                raw_hex: None,
            },
            count: 4,
            first_seq: 10,
            last_seq: 13,
            first_t_wall: "t10".to_string(),
            last_t_wall: "t13".to_string(),
        };
        let json = line_to_json(&l);
        assert_eq!(json["folded"], true);
        assert_eq!(json["count"], 4);
        assert_eq!(json["first_seq"], 10);
        assert_eq!(json["last_seq"], 13);
        assert_eq!(json["text"], "dup");
    }

    #[test]
    fn binary_summary_json_omits_text_and_carries_length_and_preview() {
        let l = PresentedLine::Single {
            seq: 0,
            t_mono: 0.0,
            t_wall: "t0".to_string(),
            render: LineRender::BinarySummary {
                length: 999,
                hex_preview: "ff fe".to_string(),
            },
        };
        let json = line_to_json(&l);
        assert_eq!(json["binary"], true);
        assert_eq!(json["binary_summary"]["length"], 999);
        assert_eq!(json["binary_summary"]["hex_preview"], "ff fe");
        assert!(json.get("text").is_none());
    }
}
