<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import { LiveLogBuffer, type LogItem, type TimestampMode } from "./liveLog";
  import { LogStream, fetchDeviceConfig, type DeviceConfig, type LogStreamState } from "./logStream";
  import LogRow from "./LogRow.svelte";
  import Timeline from "./Timeline.svelte";
  import PortSettingsPopover from "./PortSettingsPopover.svelte";
  import type { TimelineSelection } from "./timeline";

  interface Props {
    deviceId: string;
    /** T5.5 (issue #22): forwarded a copy of every timeline drag-selection
     * this view produces, so a sibling `ExportDialog` (owned by `App.svelte`,
     * not this component) can offer it as an export source. This component
     * still keeps its own `timelineSelection` copy too (for the inline
     * "selected seq…" info line below the timeline) — the two are
     * deliberately kept in sync rather than one replacing the other, since
     * this component has no reason to depend on `App.svelte`'s state and
     * vice versa. */
    onTimelineSelect?: (selection: TimelineSelection | null) => void;
    /** T5.5 (issue #22): called once on mount with this component's own
     * `jumpToSeq` function, so `App.svelte` can route an audit panel's
     * "jump to log" click into the exact same jump/highlight behavior the
     * timeline's own click-to-jump already uses — an imperative handle
     * rather than a reactive prop, since "jump to this seq" is a one-shot
     * command, not a value this component's rendering depends on. */
    registerJumpToSeq?: (fn: (seq: number) => void) => void;
  }
  const { deviceId, onTimelineSelect, registerJumpToSeq }: Props = $props();

  /** Fixed row height, in px — see `liveLog.ts`'s module doc comment and
   * this project's virtual-scroll design note: every row (data, event,
   * fold, binary chip, gap) renders at exactly this height with
   * horizontal (never vertical) overflow, which is what makes fixed-size
   * virtualization correct here instead of needing measured/variable row
   * heights. */
  const ROW_HEIGHT = 22;
  const OVERSCAN = 10;

  const buffer = new LiveLogBuffer();
  let version = $state(0);

  let containerEl: HTMLDivElement | undefined = $state();
  let scrollTop = $state(0);
  let viewportHeight = $state(0);

  let following = $state(true);
  let pendingCount = $state(0);

  let filterText = $state("");
  let filterError = $state<string | null>(null);
  let timestampMode = $state<TimestampMode>("absolute");
  let hexMode = $state(false);
  let expandedIds = $state<Set<number>>(new Set());

  let streamState = $state<LogStreamState>("connecting");
  let streamErrorDetail = $state<string | null>(null);
  let deviceConfig = $state<DeviceConfig | null>(null);
  let recordingSinceLabel = $state<string | null>(null);

  // ---- T5.3 (issue #20): timeline jump/highlight + settings popover ----
  let popoverOpen = $state(false);
  let configChipEl: HTMLButtonElement | undefined = $state();
  let highlightedItemId = $state<number | null>(null);
  let highlightTimer: ReturnType<typeof setTimeout> | undefined;
  let timelineSelection = $state<TimelineSelection | null>(null);

  function refreshConfig(): void {
    fetchDeviceConfig(deviceId)
      .then((c) => {
        deviceConfig = c;
      })
      .catch(() => {
        deviceConfig = null;
      });
  }

  /** Item range (inclusive) a `LogItem` covers, in the underlying record
   * `seq` space — a `line` may be a fold spanning several records
   * (`seq`..`lastSeq`); everything else is a single record. `gap` has no
   * real seq of its own (`afterSeq` is a display anchor, not a record). */
  function itemSeqRange(item: LogItem): [number, number] | null {
    if (item.kind === "gap") return null;
    if (item.kind === "line") return [item.seq, item.lastSeq];
    return [item.seq, item.seq];
  }

  /** Jump the log view to whichever buffered item covers `seq`, pausing
   * follow mode, centering it in the viewport, and flashing a highlight —
   * the timeline's click-to-jump contract (T5.3 acceptance criterion 1).
   * Clears an active filter first: a filtered-out target would otherwise
   * silently fail to be found in `buffer.filtered` (the only array actually
   * rendered), and jumping to hidden content is a worse outcome than
   * dropping the filter. */
  function jumpToSeq(seq: number): void {
    const target = buffer.items.find((it) => {
      const range = itemSeqRange(it);
      return range !== null && seq >= range[0] && seq <= range[1];
    });
    if (!target) return;

    if (filterText.length > 0) {
      filterText = "";
      applyFilter();
    }

    following = false;
    void tick().then(() => {
      const idx = buffer.filtered.findIndex((it) => it.id === target.id);
      if (idx === -1 || !containerEl) return;
      const targetTop = idx * ROW_HEIGHT;
      containerEl.scrollTop = Math.max(0, targetTop - viewportHeight / 2 + ROW_HEIGHT / 2);
      scrollTop = containerEl.scrollTop;
      version++;
    });

    highlightedItemId = target.id;
    if (highlightTimer) clearTimeout(highlightTimer);
    highlightTimer = setTimeout(() => {
      highlightedItemId = null;
    }, 3_000);
  }

  function togglePopover(): void {
    popoverOpen = !popoverOpen;
  }

  let pendingPages: Parameters<typeof buffer.ingest>[0][] = [];
  let rafScheduled = false;

  // Reading `version` inside a `$derived.by` callback is how these blocks
  // declare "recompute when the buffer mutates" — `buffer.items`/`filtered`
  // are plain arrays, not Svelte-reactive state (see `liveLog.ts`'s module
  // doc comment on why), so `version` is the explicit dependency that
  // stands in for them. A function call (rather than a bare `version;`
  // statement) keeps this lint-clean under `no-unused-expressions`.
  function trackVersion(): void {
    void version;
  }

  const totalItems = $derived.by(() => {
    trackVersion();
    return buffer.filtered.length;
  });
  const totalHeight = $derived(totalItems * ROW_HEIGHT);
  const startIndex = $derived.by(() => {
    trackVersion();
    return Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  });
  const visibleCount = $derived(Math.ceil(viewportHeight / ROW_HEIGHT) + OVERSCAN * 2);
  const endIndex = $derived(Math.min(totalItems, startIndex + visibleCount));

  interface Row {
    item: LogItem;
    top: number;
    timestamp: string;
  }

  const rows = $derived.by((): Row[] => {
    trackVersion();
    const out: Row[] = [];
    for (let i = startIndex; i < endIndex; i++) {
      const item = buffer.filtered[i];
      if (!item) continue;
      const prev = i > 0 ? buffer.filtered[i - 1] : undefined;
      const timestamp = buffer.formatTimestamp(item, timestampMode, prev ? prev.tMs : null);
      out.push({ item, top: i * ROW_HEIGHT, timestamp });
    }
    return out;
  });

  function isExpanded(id: number): boolean {
    const toggled = expandedIds.has(id);
    return toggled ? !hexMode : hexMode;
  }

  function toggleExpand(id: number): void {
    const next = new Set(expandedIds);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    expandedIds = next;
  }

  function scrollToBottom(): void {
    if (!containerEl) return;
    containerEl.scrollTop = totalHeight;
  }

  function flushPending(): void {
    if (!rafScheduled) return; // already run by the other scheduled trigger — see onPage
    rafScheduled = false;
    const pages = pendingPages;
    pendingPages = [];
    const beforeLen = buffer.filtered.length;
    for (const page of pages) buffer.ingest(page);
    version++;
    const added = buffer.filtered.length - beforeLen;
    if (recordingSinceLabel === null && buffer.items.length > 0) {
      recordingSinceLabel = buffer.items[0].kind === "gap" ? null : buffer.items[0].tWall;
    }
    if (following) {
      pendingCount = 0;
      void tick().then(scrollToBottom);
    } else {
      pendingCount += added;
    }
  }

  function onPage(page: Parameters<typeof buffer.ingest>[0]): void {
    // T5.3 (issue #20) broadcast acceptance criterion: a `config_change`
    // event (from *any* client — this tab's own popover, another open tab,
    // or an agent) must refresh this tab's status-bar config chip too, not
    // just the tab that made the change. `config_change` already flows
    // through this same per-device push stream (see `device_profile.rs`'s
    // event-naming docs) — no new subscription needed, just react to it.
    if (page.events.some((e) => e.event === "config_change")) {
      refreshConfig();
    }
    pendingPages.push(page);
    if (!rafScheduled) {
      rafScheduled = true;
      requestAnimationFrame(flushPending);
      // Fallback for backgrounded/hidden tabs: browsers can suspend
      // `requestAnimationFrame` callbacks indefinitely for a hidden
      // document, which would otherwise strand ingested data in
      // `pendingPages` forever. A bounded 50ms timer guarantees a flush
      // regardless of page-visibility throttling, while rAF stays the
      // fast path whenever the tab is actually being painted.
      // `flushPending`'s own `rafScheduled` check keeps whichever of the
      // two fires second a no-op.
      setTimeout(flushPending, 50);
    }
  }

  const BOTTOM_EPS = ROW_HEIGHT / 2;

  function onScroll(): void {
    if (!containerEl) return;
    scrollTop = containerEl.scrollTop;
    viewportHeight = containerEl.clientHeight;
    const atBottom = scrollTop + viewportHeight >= totalHeight - BOTTOM_EPS;
    if (atBottom) {
      if (!following) {
        following = true;
        pendingCount = 0;
      }
    } else if (following) {
      following = false;
    }
  }

  function resumeFollowing(): void {
    following = true;
    pendingCount = 0;
    void tick().then(scrollToBottom);
  }

  /** Wall-clock time `buffer.setFilter` itself took on the last call, in
   * ms — surfaced via a hidden `data-testid="filter-elapsed-ms"` element
   * (see the template) purely so the Playwright E2E performance
   * acceptance test (`webui/e2e/live-log.spec.ts`, "10万行 regex filter
   * <=100ms") can read a precise, production-code-path measurement
   * instead of inferring timing from DOM mutation observers. */
  let lastFilterElapsedMs = $state(0);

  function applyFilter(): void {
    const t0 = performance.now();
    try {
      buffer.setFilter(filterText.length > 0 ? filterText : null);
      filterError = null;
    } catch (e) {
      filterError = e instanceof Error ? e.message : String(e);
    }
    lastFilterElapsedMs = performance.now() - t0;
    version++;
  }

  /** Total buffered items (pre-filter), surfaced via
   * `data-testid="buffered-count"` for the same E2E performance tests —
   * they need to know when a bulk injection has actually finished
   * arriving through the real ingest pipeline before measuring anything
   * downstream of it. */
  const bufferedCount = $derived.by((): number => {
    trackVersion();
    return buffer.items.length;
  });

  let stream: LogStream | undefined;

  onMount(() => {
    viewportHeight = containerEl?.clientHeight ?? 0;
    stream = new LogStream(deviceId, {
      onPage,
      onState: (state, detail) => {
        streamState = state;
        streamErrorDetail = detail ?? null;
      },
    });
    void stream.start();
    refreshConfig();
    registerJumpToSeq?.(jumpToSeq);
  });

  onDestroy(() => {
    stream?.stop();
    if (highlightTimer) clearTimeout(highlightTimer);
  });

  function cycleTimestampMode(): void {
    const order: TimestampMode[] = ["absolute", "relative", "delta"];
    const idx = order.indexOf(timestampMode);
    timestampMode = order[(idx + 1) % order.length];
    version++;
  }

  const errorCountsLabel = $derived.by((): string => {
    const ec = deviceConfig?.error_counts;
    if (!ec || ec.status === "unavailable") return "framing unavailable · overrun unavailable";
    return `framing ${ec.framing} · overrun ${ec.overrun}`;
  });

  function itemSeqForDisplay(item: LogItem): number {
    return item.kind === "gap" ? item.afterSeq : item.kind === "line" ? item.lastSeq : item.seq;
  }

  const lastOffsetLabel = $derived.by((): string => {
    trackVersion();
    if (buffer.items.length === 0) return "0";
    return itemSeqForDisplay(buffer.items[buffer.items.length - 1]).toLocaleString();
  });

  const configLabel = $derived.by((): string => {
    const c = deviceConfig?.config as { baud?: number; data_bits?: string; parity?: string; stop_bits?: string } | undefined;
    if (!c) return "…";
    const bits = c.data_bits === "eight" ? "8" : c.data_bits === "seven" ? "7" : (c.data_bits ?? "?");
    const parity = c.parity === "none" ? "N" : c.parity === "even" ? "E" : c.parity === "odd" ? "O" : "?";
    const stop = c.stop_bits === "one" ? "1" : c.stop_bits === "two" ? "2" : (c.stop_bits ?? "?");
    return `${c.baud ?? "?"} · ${bits}${parity}${stop}`;
  });
</script>

<section class="live-log" data-testid="live-log" data-device={deviceId}>
  <div class="status-bar" data-testid="status-bar">
    <span
      class="dot"
      class:connected={streamState === "open"}
      data-testid="connection-dot"
      data-state={streamState}
    ></span>
    <span class="device-id">{deviceId}</span>
    <span class="config-chip-wrap">
      <button
        type="button"
        class="config-chip"
        data-testid="config-chip"
        bind:this={configChipEl}
        onclick={togglePopover}
      >
        {configLabel}
      </button>
      <PortSettingsPopover
        {deviceId}
        open={popoverOpen}
        onClose={() => (popoverOpen = false)}
        onApplied={refreshConfig}
        anchorEl={configChipEl}
      />
    </span>
    {#if streamState === "error" && streamErrorDetail}
      <span class="stream-error" data-testid="stream-error">{streamErrorDetail}</span>
    {/if}
  </div>

  <Timeline
    items={buffer.items}
    {version}
    onJump={jumpToSeq}
    onRangeSelect={(selection) => {
      timelineSelection = selection;
      onTimelineSelect?.(selection);
    }}
  />
  {#if timelineSelection}
    <div class="timeline-selection-info" data-testid="timeline-selection">
      selected seq {timelineSelection.fromSeq}–{timelineSelection.toSeq}
    </div>
  {/if}

  <div class="controls">
    <input
      type="text"
      class:invalid={filterError !== null}
      placeholder="regex filter"
      data-testid="filter-input"
      bind:value={filterText}
      oninput={applyFilter}
    />
    <button type="button" data-testid="timestamp-mode-toggle" onclick={cycleTimestampMode}>
      &Delta; {timestampMode}
    </button>
    <label class="hex-toggle">
      <input type="checkbox" data-testid="hex-toggle" bind:checked={hexMode} />
      HEX
    </label>
    {#if !following}
      <span class="paused-indicator" data-testid="paused-indicator">&#9208; paused</span>
    {/if}
  </div>
  {#if filterError}
    <div class="filter-error" data-testid="filter-error">{filterError}</div>
  {/if}

  <div
    class="viewport"
    data-testid="log-viewport"
    data-following={following}
    bind:this={containerEl}
    onscroll={onScroll}
  >
    <div class="spacer" style="height: {totalHeight}px;">
      {#each rows as row (row.item.id)}
        <div class="positioned" style="transform: translateY({row.top}px);">
          <LogRow
            item={row.item}
            timestamp={row.timestamp}
            expanded={isExpanded(row.item.id)}
            onToggleExpand={toggleExpand}
            {deviceId}
            highlighted={row.item.id === highlightedItemId}
            onReverted={refreshConfig}
          />
        </div>
      {/each}
    </div>
  </div>

  {#if pendingCount > 0}
    <button
      type="button"
      class="resume-pill"
      data-testid="resume-following-pill"
      onclick={resumeFollowing}
    >
      &#8964; {pendingCount.toLocaleString()} new lines &mdash; resume following
    </button>
  {/if}

  <div class="footer-status" data-testid="error-counts">
    {errorCountsLabel}
    <span class="sep">&middot;</span>
    offset {lastOffsetLabel}
    {#if recordingSinceLabel}
      <span class="sep">&middot;</span>
      recording since {new Date(recordingSinceLabel).toLocaleTimeString()}
    {/if}
  </div>

  <!-- E2E performance-test hooks only — never shown, no user-facing
       purpose. See `lastFilterElapsedMs`/`bufferedCount`'s doc comments. -->
  <span data-testid="buffered-count" style="display: none;">{bufferedCount}</span>
  <span data-testid="filter-elapsed-ms" style="display: none;">{lastFilterElapsedMs}</span>
</section>

<style>
  .live-log {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--surface);
    overflow: hidden;
  }

  .status-bar {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid var(--border);
    font-family: var(--font-mono);
    font-size: 0.8125rem;
  }

  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--dot-closed);
    flex: none;
  }
  .dot.connected {
    background: var(--dot-open);
  }

  .config-chip-wrap {
    position: relative;
    display: inline-flex;
  }

  .config-chip {
    font: inherit;
    border: 1px solid var(--border);
    border-radius: 0.35rem;
    padding: 0.05rem 0.4rem;
    color: var(--text-dim);
    background: transparent;
    cursor: pointer;
  }

  .timeline-selection-info {
    padding: 0.15rem 0.75rem;
    font-size: 0.7rem;
    color: var(--text-dim);
    border-bottom: 1px solid var(--border);
  }

  .stream-error {
    color: var(--dot-closed);
    margin-left: auto;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.4rem 0.75rem;
    border-bottom: 1px solid var(--border);
    font-size: 0.8125rem;
  }

  .controls input[type="text"] {
    flex: 1;
    font: inherit;
    font-family: var(--font-mono);
    padding: 0.25rem 0.5rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
  }

  .controls input.invalid {
    border-color: var(--dot-closed);
  }

  .controls button {
    font: inherit;
    padding: 0.25rem 0.6rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
    white-space: nowrap;
  }

  .hex-toggle {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    color: var(--text-dim);
  }

  .paused-indicator {
    margin-left: auto;
    color: var(--dot-stale);
  }

  .filter-error {
    padding: 0.2rem 0.75rem;
    color: var(--dot-closed);
    font-size: 0.75rem;
  }

  .viewport {
    position: relative;
    height: 24rem;
    overflow-y: auto;
    overflow-x: hidden;
  }

  .spacer {
    position: relative;
    width: 100%;
  }

  .positioned {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    height: 22px;
  }

  .resume-pill {
    align-self: center;
    margin: -1.6rem auto 0.5rem;
    font: inherit;
    font-size: 0.75rem;
    padding: 0.25rem 0.75rem;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
    z-index: 1;
  }

  .footer-status {
    padding: 0.35rem 0.75rem;
    border-top: 1px solid var(--border);
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--text-dim);
  }

  .sep {
    margin: 0 0.3rem;
  }
</style>
