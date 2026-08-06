/**
 * Pure data model for the live log view (`TASKS.md` T5.2, issue #19):
 * turning the daemon's presented-page JSON (`GET /api/devices/:id/tail`,
 * `WS /api/stream?device=...` pushes — both already run through
 * `serialwrapd::presentation::present`, see `crates/serialwrapd/src/web/api.rs`'s
 * module doc comment) into a flat, chronologically-ordered array of
 * display items, plus the client-side-only concerns the daemon
 * deliberately doesn't do: regex filtering (a display concern, never
 * narrows what's recorded) and gap-chip insertion between two items whose
 * timestamps are far apart.
 *
 * This module is intentionally framework-free (no Svelte imports) so it's
 * usable from a plain `<script>` in a Playwright `page.evaluate` for the
 * performance acceptance tests, and so the folding/binary-summary logic
 * itself is never reimplemented here — every `LineRender` shape below is a
 * direct decode of what the daemon already computed (see
 * `crates/serialwrapd/src/presentation.rs`'s `LineRender`/`PresentedLine`).
 *
 * # Why display timing is derived from `t_wall`, never `t_mono`
 *
 * The wire's `t_mono` is `CLOCK_MONOTONIC` seconds — a clock with an
 * arbitrary, per-process epoch, useful for interval math *within* one
 * daemon run but meaningless compared against anything else. A folded
 * `PresentedLine` (`presentation::PresentedLine::Fold`) carries no
 * `t_mono` at all (only `first_t_wall`/`last_t_wall` — see
 * `presentation.rs`'s `line_to_json`), so there is no monotonic value to
 * decode for it in the first place. An earlier version of this module
 * tried to backfill one via `Date.parse(first_t_wall)` and mix the result
 * with real lines' `t_mono` values for gap-chip math — those are two
 * unrelated clocks, and the mix produced nonsense multi-year "gaps" in
 * manual testing. Every item here instead gets its display time from
 * `t_wall` (RFC 3339, parsed to epoch milliseconds) uniformly, including
 * ordinary un-folded lines — one clock domain, no special case, no
 * seam for a fold to fall through.
 */

import { hasAnsi, parseAnsi, type AnsiSpan } from "./ansi";

/** Wire shape of one `presentation::PresentedLine` (see `line_to_json`). */
export interface PresentedLineJson {
  seq?: number;
  t_mono?: number;
  t_wall?: string;
  text?: string;
  binary?: boolean;
  raw_hex?: string;
  binary_summary?: { length: number; hex_preview: string };
  folded?: boolean;
  count?: number;
  first_seq?: number;
  last_seq?: number;
  first_t_wall?: string;
  last_t_wall?: string;
}

/** Wire shape of one `presentation::OobRecord` (see `event_to_json`). */
export interface OobRecordJson {
  seq: number;
  t_mono: number;
  t_wall: string;
  kind: "event" | "gate" | "tx";
  event?: string;
  [extra: string]: unknown;
}

/** Wire shape of `presentation::PresentedPage` (see `page_to_json`). */
export interface PresentedPageJson {
  lines: PresentedLineJson[];
  events: OobRecordJson[];
  cursor: number;
  truncated: boolean;
}

/** How a data line's bytes render — mirrors `presentation::LineRender`
 * exactly, decoded rather than recomputed: `binary: true` with a
 * `binary_summary` means the daemon already decided this is a compact hex
 * chip; `binary: true` with `raw_hex` (no `binary_summary`) means the
 * daemon kept the full text *and* the full hex (the low-ratio-invalid-utf8
 * case) — see `presentation.rs`'s module docs for why both are `binary`.
 *
 * One client-side addition on top of the wire shape: ANSI escape
 * sequences are parsed out here, once, at decode time. `text` is always
 * the *stripped* string — so the filter box, fold display, and highlight
 * matching all see what the human sees, never `[1;34m` noise — and
 * `spans` carries the styled runs for display when the line actually had
 * any SGR color in it (`null` for the overwhelmingly common plain line,
 * which skips the parse entirely — see `ansi.ts`). */
export type LineRender =
  | { kind: "text"; text: string; spans: AnsiSpan[] | null; rawHex: string | null }
  | { kind: "binary_summary"; length: number; hexPreview: string };

function decodeRender(line: PresentedLineJson): LineRender {
  if (line.binary_summary) {
    return { kind: "binary_summary", length: line.binary_summary.length, hexPreview: line.binary_summary.hex_preview };
  }
  const raw = line.text ?? "";
  const spans = hasAnsi(raw) ? parseAnsi(raw) : null;
  return {
    kind: "text",
    text: spans ? spans.map((s) => s.text).join("") : raw,
    spans,
    rawHex: line.binary ? (line.raw_hex ?? null) : null,
  };
}

/** Every kind of row the live log view can render, plus the client-only
 * synthetic `gap` row. `id` is a locally-assigned, monotonically
 * increasing identifier (not the daemon's `seq`, which folds/gaps don't
 * have one-to-one) used to key expand/collapse state. `tMs` is display
 * time only — see this module's doc comment on why it's derived from
 * `t_wall`, never the wire's `t_mono`. */
export type LogItem =
  | {
      kind: "line";
      id: number;
      seq: number;
      lastSeq: number;
      tMs: number;
      tWall: string;
      lastTWall: string;
      render: LineRender;
      folded: boolean;
      count: number;
    }
  | {
      kind: "tx";
      id: number;
      seq: number;
      tMs: number;
      tWall: string;
      client: string;
      clientType: string;
      gate: string;
      text: string;
      rawHex: string;
    }
  | {
      kind: "event";
      id: number;
      seq: number;
      tMs: number;
      tWall: string;
      name: string;
      extra: Record<string, unknown>;
    }
  | {
      kind: "gate";
      id: number;
      seq: number;
      tMs: number;
      tWall: string;
      action: string;
      reason: string;
      requestSeq: number;
    }
  | {
      kind: "gap";
      id: number;
      afterSeq: number;
      tMs: number;
      deltaS: number;
    };

/** A gap of at least this many seconds between two consecutive items'
 * display time gets a `+N.Ns` chip inserted between them — the UX-design
 * wiki's "a long pause is a finding even when reading absolute times."
 * Not specified as an exact number by the wiki/issue; 1s is this
 * implementation's chosen default (a typical firmware boot log's own
 * line-to-line spacing is well under 1s, so this rarely fires on normal
 * chatty output but reliably catches a real stall). */
export const DEFAULT_GAP_THRESHOLD_S = 1.0;

function parseTWallMs(tWall: string): number {
  const parsed = Date.parse(tWall);
  return Number.isNaN(parsed) ? 0 : parsed;
}

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join(" ");
}

/** Decode a base64 TX payload into displayable text (control characters
 * shown as their Unicode "control picture" glyphs, e.g. `\n` -> `␊`, the
 * mockup's own convention: "`status␊`") plus its hex form — mirrors the
 * approval card's "bytes appear in both forms" principle from the
 * UX-design wiki, applied to the always-visible TX row instead of a
 * one-off dialog. */
function decodeTxPayload(dataB64: string): { text: string; rawHex: string } {
  let bytes: Uint8Array;
  try {
    bytes = b64ToBytes(dataB64);
  } catch {
    return { text: "(invalid base64)", rawHex: "" };
  }
  const text = Array.from(bytes)
    .map((b) => {
      if (b < 0x20) return String.fromCodePoint(0x2400 + b); // control pictures block
      if (b === 0x7f) return "␡";
      return String.fromCharCode(b);
    })
    .join("");
  return { text, rawHex: bytesToHex(bytes) };
}

let nextItemId = 1;

/** Turn one `OobRecordJson` into its `LogItem` — `tx`/`gate` get their own
 * variant; anything else (`config_change`, `lease_start`, `disconnect`,
 * future event names T5.3+ introduce) becomes a generic `event` row, so
 * this view never needs updating just because a later task adds a new
 * event name — see this module's doc comment on forward compatibility. */
function eventToItem(e: OobRecordJson): LogItem {
  const id = nextItemId++;
  const tMs = parseTWallMs(e.t_wall);
  if (e.kind === "tx") {
    const { text, rawHex } = decodeTxPayload(String(e.data_b64 ?? ""));
    return {
      kind: "tx",
      id,
      seq: e.seq,
      tMs,
      tWall: e.t_wall,
      client: String(e.client ?? "unknown"),
      clientType: String(e.client_type ?? "unknown"),
      gate: String(e.gate ?? ""),
      text,
      rawHex,
    };
  }
  if (e.kind === "gate") {
    return {
      kind: "gate",
      id,
      seq: e.seq,
      tMs,
      tWall: e.t_wall,
      action: String(e.action ?? ""),
      reason: String(e.reason ?? ""),
      requestSeq: Number(e.request_seq ?? 0),
    };
  }
  const extra: Record<string, unknown> = { ...e };
  delete extra.kind;
  delete extra.seq;
  delete extra.t_mono;
  delete extra.t_wall;
  delete extra.event;
  return {
    kind: "event",
    id,
    seq: e.seq,
    tMs,
    tWall: e.t_wall,
    name: e.event ?? "unknown",
    extra,
  };
}

function lineToItem(line: PresentedLineJson): LogItem {
  const id = nextItemId++;
  const render = decodeRender(line);
  if (line.folded) {
    const firstTWall = line.first_t_wall ?? "";
    return {
      kind: "line",
      id,
      seq: line.first_seq ?? 0,
      lastSeq: line.last_seq ?? 0,
      tMs: parseTWallMs(firstTWall),
      tWall: firstTWall,
      lastTWall: line.last_t_wall ?? "",
      render,
      folded: true,
      count: line.count ?? 1,
    };
  }
  const tWall = line.t_wall ?? "";
  return {
    kind: "line",
    id,
    seq: line.seq ?? 0,
    lastSeq: line.seq ?? 0,
    tMs: parseTWallMs(tWall),
    tWall,
    lastTWall: tWall,
    render,
    folded: false,
    count: 1,
  };
}

function itemStartSeq(item: LogItem): number {
  return item.kind === "gap" ? item.afterSeq : item.seq;
}

/** Interleave a page's separately-sorted `lines`/`events` arrays back into
 * one chronological sequence by start `seq` — both arrays are individually
 * ascending already (the daemon only ever appends in increasing `seq`
 * order), so this is a stable two-pointer merge, not a resort. The daemon
 * sends them as two arrays because that's `presentation::PresentedPage`'s
 * shape (lines and events are structurally different); reconstructing
 * display order from them is a legitimate, thin recombination — not a
 * reimplementation of the folding/summarization decisions themselves,
 * which are already baked into each element by the time it gets here. */
export function pageToItems(page: PresentedPageJson): LogItem[] {
  const lineItems = page.lines.map(lineToItem);
  const eventItems = page.events.map(eventToItem);
  const merged: LogItem[] = [];
  let i = 0;
  let j = 0;
  while (i < lineItems.length && j < eventItems.length) {
    if (itemStartSeq(lineItems[i]) <= itemStartSeq(eventItems[j])) {
      merged.push(lineItems[i++]);
    } else {
      merged.push(eventItems[j++]);
    }
  }
  while (i < lineItems.length) merged.push(lineItems[i++]);
  while (j < eventItems.length) merged.push(eventItems[j++]);
  return merged;
}

/** Text a regex filter matches against for one item. Out-of-band items
 * (`tx`/`event`/`gate`/`gap`) have no text field at all here on purpose —
 * see `LiveLogBuffer.setFilter`'s doc comment for why they never
 * participate in "does this item match" at all, matching the wiki's
 * "filters narrow which log lines are interesting; they never suppress
 * the fact that the stream was interrupted." */
function filterableText(item: LogItem): string | null {
  if (item.kind !== "line") return null;
  return item.render.kind === "text" ? item.render.text : null;
}

/** Maximum buffered items (post-merge, pre-filter) the live log view keeps
 * in memory — this is the *data* bound; DOM node count is bounded
 * separately (and far more tightly) by virtual scrolling regardless of
 * this cap. 200k comfortably covers the 100k-line filter-performance
 * acceptance criterion with headroom, while still being small enough that
 * a multi-hour session doesn't grow this without limit — mirrors the
 * daemon's own `DeviceQueryState` "known limitation: unbounded in-memory
 * growth" note (`crates/serialwrapd/src/query.rs`) by at least bounding
 * *this* side of the pipe, even though that one isn't in this task's
 * scope to fix. */
export const MAX_BUFFERED_ITEMS = 200_000;

/** How many items to evict at once when the cap is exceeded — evicting in
 * batches (rather than one at a time) amortizes the cost of the splice. */
const EVICT_BATCH = 20_000;

export type TimestampMode = "absolute" | "relative" | "delta";

function formatClockTime(tWall: string): string {
  const d = new Date(tWall);
  if (Number.isNaN(d.getTime())) return tWall;
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

function formatOffset(seconds: number): string {
  const sign = seconds < 0 ? "-" : "+";
  return `${sign}${Math.abs(seconds).toFixed(3)}s`;
}

/** Owns the merged item buffer, the filtered view over it, and the
 * client-only display concerns (gap chips, timestamp formatting,
 * expand/collapse state). Deliberately not a Svelte store/rune itself —
 * `LiveLog.svelte` holds a `version` counter it bumps after mutating
 * calls, so large arrays here stay plain (unproxied) for ingest
 * throughput, and Svelte only ever re-derives the small virtualized
 * slice. */
export class LiveLogBuffer {
  /** Every merged item, unfiltered, oldest first. */
  items: LogItem[] = [];
  /** Items passing the current filter (or all of them, if no filter is
   * set) plus every out-of-band item — see `filterableText`. Maintained
   * incrementally on `ingest` (append-only) and rebuilt from scratch only
   * when the filter itself changes, so a filter *change* costs one pass
   * over `items` (the acceptance criterion this bounds) while ordinary
   * high-rate ingest never re-scans the whole buffer. */
  filtered: LogItem[] = [];

  private filterRegex: RegExp | null = null;
  private lastNonGapTMs: number | null = null;
  private gapThresholdS = DEFAULT_GAP_THRESHOLD_S;

  /** Longest data line seen so far, in characters.
   *
   * Rows never wrap (fixed-height virtual scrolling depends on that), so the
   * log pane scrolls horizontally as one surface and needs to know how wide
   * that surface is. Absolutely-positioned virtual rows contribute nothing
   * to their parent's intrinsic width, so `max-content` can't do this for
   * us; tracking the longest line during ingest and multiplying by the
   * monospace `ch` unit can, at O(1) per line.
   *
   * Only ever grows — eviction doesn't recompute it. An over-wide scroll
   * range after the longest line ages out is invisible in practice, and
   * rescanning the buffer to reclaim a few hundred pixels would trade a real
   * cost for a cosmetic one. */
  maxChars = 0;

  /** First real (non-gap) item's display time, for `"relative"` timestamp
   * mode — `null` until at least one item has been ingested. */
  sessionStartMs: number | null = null;

  private pushRaw(item: LogItem): void {
    if (item.kind === "line") {
      const chars =
        item.render.kind === "text" ? item.render.text.length : item.render.hexPreview.length;
      if (chars > this.maxChars) this.maxChars = chars;
    }
    if (item.kind !== "gap") {
      if (this.sessionStartMs === null) this.sessionStartMs = item.tMs;
      if (this.lastNonGapTMs !== null) {
        const deltaS = (item.tMs - this.lastNonGapTMs) / 1000;
        if (deltaS > this.gapThresholdS) {
          const gap: LogItem = {
            kind: "gap",
            id: nextItemId++,
            afterSeq: this.items.length > 0 ? itemStartSeq(this.items[this.items.length - 1]) : 0,
            tMs: item.tMs,
            deltaS,
          };
          this.items.push(gap);
          if (this.matchesFilter(gap)) this.filtered.push(gap);
        }
      }
      this.lastNonGapTMs = item.tMs;
    }
    this.items.push(item);
    if (this.matchesFilter(item)) this.filtered.push(item);
  }

  private matchesFilter(item: LogItem): boolean {
    if (!this.filterRegex) return true;
    const text = filterableText(item);
    if (text === null) return true; // never suppress non-data items or binary chips
    return this.filterRegex.test(text);
  }

  /** Merge one `tail`/push page in. */
  ingest(page: PresentedPageJson): void {
    for (const item of pageToItems(page)) {
      this.pushRaw(item);
    }
    this.evictIfNeeded();
  }

  private evictIfNeeded(): void {
    if (this.items.length <= MAX_BUFFERED_ITEMS) return;
    const cutIndex = this.items.length - MAX_BUFFERED_ITEMS + EVICT_BATCH;
    this.items.splice(0, cutIndex);
    // filtered is always <= items in length, so re-deriving it by simple
    // seq-threshold filter (rather than a full refilter) keeps eviction
    // cheap too.
    const floorSeq = this.items.length > 0 ? itemStartSeq(this.items[0]) : 0;
    this.filtered = this.filtered.filter((it) => {
      const s = it.kind === "gap" ? it.afterSeq : itemStartSeq(it);
      return s >= floorSeq;
    });
  }

  /** Change the active filter, recomputing `filtered` from scratch — the
   * one place a full `items` scan happens, and the operation the "10万行
   * regex filter <=100ms" acceptance criterion measures. `null`/empty
   * clears the filter. Throws if `pattern` isn't a valid regex; caller
   * (`LiveLog.svelte`) surfaces that as the filter box's error state
   * rather than silently keeping the old filter. */
  setFilter(pattern: string | null): void {
    this.filterRegex = pattern ? new RegExp(pattern) : null;
    this.filtered = this.items.filter((it) => this.matchesFilter(it));
  }

  /** Drop everything this tab is currently showing.
   *
   * A view-only operation: the daemon's recording is append-only and is not
   * touched, so the cleared lines remain in `serialwrap tail`, in `export`,
   * and in a reload of this page. That distinction is why the control is
   * labelled "Clear view" rather than "Clear" — in a tool whose promise is
   * that nothing is lost, a button that looks like it deletes the log had
   * better not be ambiguous. */
  clear(): void {
    this.items = [];
    this.filtered = [];
    this.lastNonGapTMs = null;
    this.sessionStartMs = null;
    this.maxChars = 0;
  }

  formatTimestamp(item: LogItem, mode: TimestampMode, previousTMs: number | null): string {
    if (mode === "absolute") {
      return formatClockTime(item.kind === "gap" ? "" : item.tWall);
    }
    if (mode === "relative") {
      const start = this.sessionStartMs ?? item.tMs;
      return formatOffset((item.tMs - start) / 1000);
    }
    // delta
    if (previousTMs === null) return formatOffset(0);
    return formatOffset((item.tMs - previousTMs) / 1000);
  }
}
