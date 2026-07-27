<script lang="ts">
  import type { LogItem } from "./liveLog";
  import {
    buildLeaseBands,
    buildTimelineMarkers,
    itemSeq,
    itemWall,
    nearestItem,
    timelineDomain,
    type TimelineMarker,
    type TimelineSelection,
  } from "./timeline";

  interface Props {
    items: LogItem[];
    /** `LiveLog.svelte`'s own `version` counter, bumped after every buffer
     * mutation. `items` (`buffer.items`) is a plain, unproxied, in-place-
     * mutated array (see `liveLog.ts`'s module doc comment) — its
     * *reference* never changes, only its contents, so a `$derived` here
     * that only reads `items` would never see Svelte schedule a
     * recomputation after the initial one. Reading `version` too (even
     * though nothing below uses its value) is what makes "recompute
     * whenever the buffer mutates" explicit — the same `trackVersion()`
     * convention `LiveLog.svelte` already uses internally, extended across
     * this component boundary. */
    version: number;
    onJump: (seq: number) => void;
    onRangeSelect?: (selection: TimelineSelection | null) => void;
  }
  const { items, version, onJump, onRangeSelect }: Props = $props();

  let trackEl: HTMLDivElement | undefined = $state();

  function trackVersion(): void {
    void version;
  }

  const domain = $derived.by(() => {
    trackVersion();
    return timelineDomain(items);
  });
  const markers = $derived.by(() => {
    trackVersion();
    return buildTimelineMarkers(items);
  });
  const bands = $derived.by(() => {
    trackVersion();
    return buildLeaseBands(items);
  });

  function pct(tMs: number): number {
    const d = domain;
    if (!d) return 0;
    const [start, end] = d;
    return ((tMs - start) / (end - start)) * 100;
  }

  function formatClock(tMs: number): string {
    const d = new Date(tMs);
    if (Number.isNaN(d.getTime())) return "";
    return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  }

  // ---- Drag-select ----
  // Supplies T5.5's export range picker (see `timeline.ts`'s
  // `TimelineSelection` doc comment for the exact interface). This
  // component only produces the selection and displays it; nothing
  // downstream of `onRangeSelect` exists yet (T5.5 is a later task) — the
  // acceptance criterion this satisfies (T5.3 #6) is "拖曳框選區間可用",
  // i.e. the interaction itself works and yields a usable range.
  let dragging = $state(false);
  let dragStartFrac = $state(0);
  let dragEndFrac = $state(0);

  function fracAtClientX(clientX: number): number {
    if (!trackEl) return 0;
    const rect = trackEl.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
  }

  function handlePointerDown(e: PointerEvent): void {
    if (!domain) return;
    // A pointerdown that started on a marker button is a click-to-jump, not
    // a drag-select — letting it also start a (zero-distance, harmless)
    // drag would be inert on its own, but capturing the pointer here (see
    // below) would incorrectly redirect the marker's own subsequent `click`
    // handling.
    if (e.target instanceof Element && e.target.closest(".marker")) return;
    dragging = true;
    const frac = fracAtClientX(e.clientX);
    dragStartFrac = frac;
    dragEndFrac = frac;
    trackEl?.setPointerCapture(e.pointerId);
  }

  function handlePointerMove(e: PointerEvent): void {
    if (!dragging) return;
    dragEndFrac = fracAtClientX(e.clientX);
  }

  function handlePointerUp(): void {
    if (!dragging) return;
    dragging = false;
    const d = domain;
    if (!d || !onRangeSelect) return;
    const [start, end] = d;
    const loFrac = Math.min(dragStartFrac, dragEndFrac);
    const hiFrac = Math.max(dragStartFrac, dragEndFrac);
    // Ignore a click-without-drag (near-zero width) — a real range
    // selection, not every single click on the track.
    if (hiFrac - loFrac < 0.01) {
      onRangeSelect(null);
      return;
    }
    const loMs = start + loFrac * (end - start);
    const hiMs = start + hiFrac * (end - start);
    const fromItem = nearestItem(items, loMs);
    const toItem = nearestItem(items, hiMs);
    if (!fromItem || !toItem) {
      onRangeSelect(null);
      return;
    }
    onRangeSelect({
      fromSeq: itemSeq(fromItem),
      toSeq: itemSeq(toItem),
      fromWall: itemWall(fromItem),
      toWall: itemWall(toItem),
    });
  }

  const selectionStyle = $derived.by((): string | null => {
    if (!dragging) return null;
    const lo = Math.min(dragStartFrac, dragEndFrac) * 100;
    const hi = Math.max(dragStartFrac, dragEndFrac) * 100;
    return `left: ${lo}%; width: ${hi - lo}%;`;
  });

  function markerTitle(m: TimelineMarker): string {
    return `${m.label} @ seq ${m.seq}`;
  }
</script>

<div class="timeline" data-testid="timeline">
  <!-- Drag-select is a pointer-only power-user gesture (T5.5's future range
       picker will have its own accessible controls); the individual event
       markers below are real `<button>`s and remain independently
       focusable/clickable. -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="track"
    data-testid="timeline-track"
    bind:this={trackEl}
    onpointerdown={handlePointerDown}
    onpointermove={handlePointerMove}
    onpointerup={handlePointerUp}
  >
    {#each bands as band (band.token)}
      <div
        class="lease-band"
        data-testid="timeline-lease-band"
        style="left: {pct(band.startTMs)}%; width: {Math.max(0.5, pct(band.endTMs ?? domain?.[1] ?? band.startTMs) - pct(band.startTMs))}%;"
        title="lease: {band.command}"
      ></div>
    {/each}

    {#if selectionStyle}
      <div class="selection-overlay" data-testid="timeline-selection-overlay" style={selectionStyle}></div>
    {/if}

    {#each markers as marker (marker.itemId)}
      <button
        type="button"
        class="marker"
        data-testid="timeline-marker"
        data-marker-kind={marker.kind}
        data-marker-seq={marker.seq}
        style="left: {pct(marker.tMs)}%;"
        title={markerTitle(marker)}
        onclick={(e) => {
          e.stopPropagation();
          onJump(marker.seq);
        }}
      >
      </button>
    {/each}
  </div>

  {#if domain}
    <div class="ruler">
      <span>{formatClock(domain[0])}</span>
      <span>{formatClock(domain[1])} &middot; now</span>
    </div>
  {/if}
</div>

<style>
  .timeline {
    padding: 0.35rem 0.75rem;
    border-bottom: 1px solid var(--border);
  }

  .track {
    position: relative;
    height: 1.1rem;
    background: var(--surface-raised);
    border-radius: 0.25rem;
    touch-action: none;
    cursor: crosshair;
  }

  .lease-band {
    position: absolute;
    top: 0;
    bottom: 0;
    background: rgba(210, 153, 34, 0.35);
    border-radius: 0.15rem;
  }

  .selection-overlay {
    position: absolute;
    top: 0;
    bottom: 0;
    background: rgba(88, 166, 255, 0.35);
    border: 1px solid #58a6ff;
    pointer-events: none;
  }

  .marker {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 3px;
    padding: 0;
    border: none;
    cursor: pointer;
    transform: translateX(-1.5px);
  }
  .marker[data-marker-kind="reset"] {
    background: #f85149;
  }
  .marker[data-marker-kind="lease"] {
    background: #d29922;
  }
  .marker[data-marker-kind="tx"] {
    background: #d29922;
  }
  .marker[data-marker-kind="gate"] {
    background: #f85149;
  }

  .ruler {
    display: flex;
    justify-content: space-between;
    color: var(--text-dim);
    font-size: 0.7rem;
    margin-top: 0.15rem;
  }
</style>
