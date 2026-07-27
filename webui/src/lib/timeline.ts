/**
 * Pure data model for the timeline (`TASKS.md` T5.3, issue #20): event
 * markers (reset/lease/tx/gate), lease colour bands, and the seq/time range
 * a drag-select produces — derived entirely from `LiveLogBuffer.items`
 * (`liveLog.ts`), the same merged, chronologically-ordered array the log
 * view itself renders. No new data source: the timeline is a different
 * *view* over data the live log already has, not a second fetch/subscribe
 * path — same reasoning `liveLog.ts`'s own module doc comment gives for why
 * `pageToItems` is "a legitimate, thin recombination" rather than a second
 * implementation of anything the daemon already decided.
 *
 * Framework-free for the same reason `liveLog.ts` is: usable from a plain
 * script/Playwright `page.evaluate`, and so the marker/band logic is never
 * duplicated between a Svelte-reactive form and a plain-data form.
 */
import type { LogItem } from "./liveLog";

export type TimelineMarkerKind = "reset" | "lease" | "tx" | "gate";

export interface TimelineMarker {
  /** The underlying `LogItem.id` — what a click hands back to the log view
   * so it can find and highlight the exact same item (`liveLog.ts`'s ids
   * are stable per ingested record, never reused). */
  itemId: number;
  kind: TimelineMarkerKind;
  seq: number;
  tMs: number;
  label: string;
}

export interface LeaseBand {
  token: string;
  command: string;
  startSeq: number;
  startTMs: number;
  /** `null` while the lease is still open (no matching `lease_end` seen
   * yet in the currently-buffered window). */
  endSeq: number | null;
  endTMs: number | null;
}

/** A `control_line_change`/`dtr_pulse` event counts as a "reset" marker —
 * both are exactly the operations `crates/serialwrapd/src/device_profile.rs`
 * documents as physically resetting most boards when they touch DTR (see
 * that module's "Event naming" section, and the Security-model wiki's
 * config-changes policy table: "Toggle DTR/RTS, dtr_pulse: gated ... Gated
 * — physically resets most boards"). `control_line_change` events that only
 * ever touch RTS are excluded — RTS alone does not reset the boards this
 * project's own docs call out (Arduino/ESP8266/32 reset on DTR).
 */
function isResetEvent(item: LogItem): item is Extract<LogItem, { kind: "event" }> {
  if (item.kind !== "event") return false;
  if (item.name === "dtr_pulse") return true;
  if (item.name === "control_line_change" && item.extra.line === "dtr") return true;
  return false;
}

export function buildTimelineMarkers(items: LogItem[]): TimelineMarker[] {
  const markers: TimelineMarker[] = [];
  for (const item of items) {
    if (item.kind === "tx") {
      markers.push({
        itemId: item.id,
        kind: "tx",
        seq: item.seq,
        tMs: item.tMs,
        label: `TX · ${item.client}`,
      });
    } else if (item.kind === "gate") {
      markers.push({
        itemId: item.id,
        kind: "gate",
        seq: item.seq,
        tMs: item.tMs,
        label: `gate: ${item.action}`,
      });
    } else if (item.kind === "event" && item.name === "lease_start") {
      markers.push({
        itemId: item.id,
        kind: "lease",
        seq: item.seq,
        tMs: item.tMs,
        label: `lease: ${String(item.extra.command ?? "")}`,
      });
    } else if (isResetEvent(item)) {
      markers.push({
        itemId: item.id,
        kind: "reset",
        seq: item.seq,
        tMs: item.tMs,
        label: item.name === "dtr_pulse" ? "reset (dtr_pulse)" : "reset (dtr)",
      });
    }
  }
  return markers;
}

/** Pair up `lease_start`/`lease_end` events (matched by their shared
 * `token` — see `crates/serialwrapd/src/port.rs`'s `append_lease_start_event`/
 * `append_lease_end_event`) into bands the timeline paints as a coloured
 * range, mirroring the UX-design wiki's main-log-view mockup ("▌ 10:32:01 –
 * 10:32:47 esptool held the port (flashing)"). A `lease_start` with no
 * matching `lease_end` yet (in the currently-buffered window) is still
 * rendered, open-ended — the lease is presumably still held. */
export function buildLeaseBands(items: LogItem[]): LeaseBand[] {
  const open = new Map<string, LeaseBand>();
  const bands: LeaseBand[] = [];
  for (const item of items) {
    if (item.kind !== "event") continue;
    if (item.name === "lease_start") {
      const token = String(item.extra.token ?? "");
      if (!token) continue;
      const band: LeaseBand = {
        token,
        command: String(item.extra.command ?? ""),
        startSeq: item.seq,
        startTMs: item.tMs,
        endSeq: null,
        endTMs: null,
      };
      open.set(token, band);
      bands.push(band);
    } else if (item.name === "lease_end") {
      const token = String(item.extra.token ?? "");
      const band = open.get(token);
      if (band) {
        band.endSeq = item.seq;
        band.endTMs = item.tMs;
        open.delete(token);
      }
    }
  }
  return bands;
}

/** `[start, end]` display-time domain (ms) the timeline maps positions
 * against — the span of every non-gap item currently buffered. `null` when
 * there's nothing to show yet. Gap items are skipped only for *domain*
 * purposes (their `tMs` is still a real display time, but a domain
 * shouldn't be forced wider than the actual data just because a gap chip
 * sits between two points already inside it — using the real items' own
 * range is equivalent and simpler). */
export function timelineDomain(items: LogItem[]): [number, number] | null {
  let start: number | null = null;
  let end: number | null = null;
  for (const item of items) {
    if (item.kind === "gap") continue;
    if (start === null || item.tMs < start) start = item.tMs;
    if (end === null || item.tMs > end) end = item.tMs;
  }
  if (start === null || end === null) return null;
  if (start === end) return [start, start + 1]; // avoid a zero-width domain
  return [start, end];
}

/** A drag-selected [start, end] range on the timeline, resolved back to the
 * nearest real items in `items` — this is the interface T5.5's export UI is
 * expected to consume (`crates/wrap-proto/src/request.rs`'s `ExportBound`
 * is `Seq(u64) | Wall(String)`; this carries both forms of each edge so
 * T5.5 can pick whichever `export_range` wants without this module needing
 * to know export's own wire shape). */
export interface TimelineSelection {
  fromSeq: number;
  toSeq: number;
  fromWall: string;
  toWall: string;
}

/** Find the item in `items` (skipping gaps) whose display time is closest
 * to `targetMs` — used to resolve a drag position (a fraction of the
 * domain) back to a concrete seq/wall-time pair. */
export function nearestItem(items: LogItem[], targetMs: number): LogItem | null {
  let best: LogItem | null = null;
  let bestDelta = Infinity;
  for (const item of items) {
    if (item.kind === "gap") continue;
    const delta = Math.abs(item.tMs - targetMs);
    if (delta < bestDelta) {
      bestDelta = delta;
      best = item;
    }
  }
  return best;
}

export function itemSeq(item: LogItem): number {
  return item.kind === "gap" ? item.afterSeq : item.seq;
}

export function itemWall(item: LogItem): string {
  return item.kind === "gap" ? "" : item.kind === "line" ? item.tWall : item.tWall;
}
