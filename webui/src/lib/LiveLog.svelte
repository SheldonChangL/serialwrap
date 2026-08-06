<script lang="ts">
  /**
   * The log pane — the thing this whole page exists to show.
   *
   * # What changed, and why
   *
   * This view used to be a 24rem box among five equally-weighted cards, with
   * each row clipping its own overflow behind a hidden scrollbar. Both are
   * fixed here, and both fixes come from the same premise: an operator
   * reading a serial port is doing one thing, so the log gets the screen.
   *
   * - **Height comes from the viewport, not a constant.** The pane flexes to
   *   fill whatever the window gives it, so a taller window means more log
   *   rather than more empty card.
   * - **Horizontal overflow is one surface.** Rows are `white-space: pre`
   *   and the *viewport* scrolls sideways under a single visible scrollbar,
   *   with the scrollable width derived from the longest buffered line
   *   (`LiveLogBuffer.maxChars` × the monospace `ch`). Previously each row
   *   scrolled alone with `scrollbar-width: none`, which meant a long line
   *   was cut off with no indication it had been and no practical way to
   *   read the rest.
   * - **The baud warning is on screen, not in a popover.** The daemon
   *   already computes "this looks undecodable, try 115200"
   *   (`compute_decode_health`), but it only ever appeared inside the port
   *   settings popover — i.e. only to someone who already suspected the
   *   baud rate. It now sits above the log, where the garbage is.
   *
   * Virtual scrolling still assumes every row is exactly `ROW_HEIGHT` tall,
   * which is what keeps 100k lines cheap and is why there is no wrap toggle:
   * wrapping makes row height content-dependent, and variable-height
   * virtualization is a different (and much larger) piece of machinery than
   * this view needs to earn its keep.
   */
  import { onDestroy, onMount, tick, type Snippet } from "svelte";
  import { LiveLogBuffer, type LogItem, type TimestampMode } from "./liveLog";
  import {
    LogStream,
    fetchDeviceConfig,
    setDeviceConfig,
    type DeviceConfig,
    type LogStreamState,
  } from "./logStream";
  import { stripAnsi } from "./ansi";
  import { harvest } from "./completion";
  import { formatConfig } from "./eventText";
  import LogRow from "./LogRow.svelte";
  import Timeline from "./Timeline.svelte";
  import PortSettingsPopover from "./PortSettingsPopover.svelte";
  import type { TimelineSelection } from "./timeline";

  interface Props {
    deviceId: string;
    /** Rendered at the start of this pane's header row — `App.svelte` puts
     * the device picker here rather than in a bar of its own, so "which port
     * am I looking at" and "how is it configured" read as one statement. */
    headerLead?: Snippet;
    /** T5.5 (issue #22): forwarded a copy of every timeline drag-selection
     * this view produces, so a sibling `ExportDialog` (owned by `App.svelte`,
     * not this component) can offer it as an export source. */
    onTimelineSelect?: (selection: TimelineSelection | null) => void;
    /** T5.5 (issue #22): called once on mount with this component's own
     * `jumpToSeq`, so an audit row's "jump to log" can reuse the timeline's
     * jump/highlight behavior — an imperative handle rather than a reactive
     * prop, since "jump to this seq" is a one-shot command. */
    registerJumpToSeq?: (fn: (seq: number) => void) => void;
    /** Same shape, for the keyboard shortcut that focuses the filter box. */
    registerFocusFilter?: (fn: () => void) => void;
    /** Same shape, for the keyboard shortcut that clears this tab's view. */
    registerClearView?: (fn: () => void) => void;
  }
  const {
    deviceId,
    headerLead,
    onTimelineSelect,
    registerJumpToSeq,
    registerFocusFilter,
    registerClearView,
  }: Props = $props();

  /** Fixed row height, in px — every row kind (data, event, fold, binary
   * chip, gap) renders at exactly this height with horizontal, never
   * vertical, overflow, which is what makes fixed-size virtualization
   * correct here instead of needing measured row heights. */
  const ROW_HEIGHT = 22;
  const OVERSCAN = 10;

  const buffer = new LiveLogBuffer();
  let version = $state(0);

  let containerEl: HTMLDivElement | undefined = $state();
  let filterEl: HTMLInputElement | undefined = $state();
  let scrollTop = $state(0);
  let viewportHeight = $state(0);

  let following = $state(true);
  let pendingCount = $state(0);

  let filterText = $state("");
  let filterError = $state<string | null>(null);
  /** `narrow` drops non-matching lines (the original behavior, and what the
   * export/filter contract elsewhere means by "filter"); `mark` keeps every
   * line and highlights the hits. Reading a log is usually the second one —
   * a matching line is rarely interesting without the lines around it — but
   * narrowing is what you want when you're counting occurrences, so both
   * stay. */
  let filterMode = $state<"narrow" | "mark">("narrow");
  let timestampMode = $state<TimestampMode>("absolute");
  let hexMode = $state(false);
  let expandedIds = $state<Set<number>>(new Set());

  let streamState = $state<LogStreamState>("connecting");
  let streamErrorDetail = $state<string | null>(null);
  let deviceConfig = $state<DeviceConfig | null>(null);
  let recordingSinceLabel = $state<string | null>(null);
  let applyingSuggestedBaud = $state(false);

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

  /** Item range (inclusive) a `LogItem` covers in record `seq` space — a
   * `line` may be a fold spanning several records; everything else is a
   * single record. `gap` has no real seq (`afterSeq` is a display anchor). */
  function itemSeqRange(item: LogItem): [number, number] | null {
    if (item.kind === "gap") return null;
    if (item.kind === "line") return [item.seq, item.lastSeq];
    return [item.seq, item.seq];
  }

  /** Jump to whichever buffered item covers `seq`, pausing follow, centering
   * it, and flashing a highlight (T5.3 acceptance criterion 1). Clears an
   * active narrowing filter first: a filtered-out target would silently fail
   * to be found in `buffer.filtered` (the only array rendered), and jumping
   * to hidden content is a worse outcome than dropping the filter. */
  function jumpToSeq(seq: number): void {
    const target = buffer.items.find((it) => {
      const range = itemSeqRange(it);
      return range !== null && seq >= range[0] && seq <= range[1];
    });
    if (!target) return;

    if (filterText.length > 0 && filterMode === "narrow") {
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

  let pendingPages: Parameters<typeof buffer.ingest>[0][] = [];
  let rafScheduled = false;

  // Reading `version` inside a `$derived.by` callback is how these blocks
  // declare "recompute when the buffer mutates" — `buffer.items`/`filtered`
  // are plain arrays, not Svelte-reactive state (see `liveLog.ts`), so
  // `version` stands in as the explicit dependency.
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

  /** Width the virtual row surface needs, in `ch`. The `+ 16` covers the
   * timestamp column and row padding, which live outside the tracked text
   * length; overshooting slightly is free (a little unused scroll range),
   * undershooting would clip, which is the bug this replaces. */
  const contentWidthCh = $derived.by((): number => {
    trackVersion();
    return buffer.maxChars + 16;
  });

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

  /** Compiled once per keystroke rather than per row. `null` unless the
   * filter box is in mark mode and holds a valid pattern. */
  const highlightRe = $derived.by((): RegExp | null => {
    if (filterMode !== "mark" || filterText.length === 0) return null;
    try {
      return new RegExp(filterText);
    } catch {
      return null;
    }
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
    // T5.3 broadcast acceptance criterion: a `config_change` from *any*
    // client (this tab's popover, another tab, an agent) must refresh this
    // tab's config chip too. It already flows through this same per-device
    // push stream — no new subscription needed, just react to it.
    if (page.events.some((e) => e.event === "config_change")) {
      refreshConfig();
    }
    // Feed the write bar's Tab completion (see `completion.ts`): absolute
    // paths the device prints are exactly the paths an operator will want
    // to type next. Binary lines are skipped — corrupted bytes that happen
    // to contain slashes are not paths.
    for (const line of page.lines) {
      if (line.text && !line.binary) harvest(deviceId, stripAnsi(line.text));
    }
    pendingPages.push(page);
    if (!rafScheduled) {
      rafScheduled = true;
      requestAnimationFrame(flushPending);
      // Fallback for backgrounded tabs: browsers can suspend rAF callbacks
      // indefinitely for a hidden document, stranding ingested data in
      // `pendingPages`. A bounded 50ms timer guarantees a flush regardless
      // of page-visibility throttling; `flushPending`'s own `rafScheduled`
      // check makes whichever fires second a no-op.
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

  /** Explicit pause, as distinct from the implicit one that happens when you
   * scroll up. Following was previously *only* controllable by scrolling,
   * which meant there was no way to hold the view still while output kept
   * arriving without also losing your place at the bottom. */
  function togglePause(): void {
    if (following) following = false;
    else resumeFollowing();
  }

  function clearView(): void {
    buffer.clear();
    expandedIds = new Set();
    recordingSinceLabel = null;
    pendingCount = 0;
    version++;
    following = true;
  }

  /** Wall-clock ms `buffer.setFilter` itself took on the last call —
   * surfaced via a hidden `data-testid="filter-elapsed-ms"` purely so the
   * Playwright performance test can read a production-code-path measurement
   * instead of inferring timing from DOM mutation observers. */
  let lastFilterElapsedMs = $state(0);

  function applyFilter(): void {
    const t0 = performance.now();
    try {
      // Mark mode never narrows: `filtered` stays the full buffer and the
      // pattern is handed to each row instead.
      const pattern = filterMode === "narrow" && filterText.length > 0 ? filterText : null;
      buffer.setFilter(pattern);
      // A bad pattern is still worth reporting in mark mode, even though it
      // can't break the view there — silently doing nothing while someone
      // types `[` and waits is its own bug.
      if (filterText.length > 0) new RegExp(filterText);
      filterError = null;
    } catch (e) {
      filterError = e instanceof Error ? e.message : String(e);
    }
    lastFilterElapsedMs = performance.now() - t0;
    version++;
  }

  function setFilterMode(mode: "narrow" | "mark"): void {
    filterMode = mode;
    applyFilter();
  }

  /** Total buffered items (pre-filter), surfaced via
   * `data-testid="buffered-count"` for the same E2E performance tests. */
  const bufferedCount = $derived.by((): number => {
    trackVersion();
    return buffer.items.length;
  });

  let stream: LogStream | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let clockTimer: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    clockTimer = setInterval(() => (nowMs = Date.now()), 1_000);
    viewportHeight = containerEl?.clientHeight ?? 0;
    // The pane's height now tracks the window's, so the virtualizer can't
    // measure once at mount and be done — a resized window would render too
    // few rows to fill the taller viewport.
    if (containerEl && typeof ResizeObserver !== "undefined") {
      resizeObserver = new ResizeObserver(() => {
        viewportHeight = containerEl?.clientHeight ?? 0;
        if (following) void tick().then(scrollToBottom);
      });
      resizeObserver.observe(containerEl);
    }
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
    registerFocusFilter?.(() => filterEl?.focus());
    registerClearView?.(clearView);
  });

  onDestroy(() => {
    stream?.stop();
    resizeObserver?.disconnect();
    if (clockTimer) clearInterval(clockTimer);
    if (highlightTimer) clearTimeout(highlightTimer);
  });

  const TIMESTAMP_LABELS: Record<TimestampMode, string> = {
    // No stray "Δ" on the absolute mode: the button says what you are
    // currently looking at, and absolute time is not a delta.
    absolute: "Clock",
    relative: "Since start",
    delta: "Δ from previous",
  };

  function cycleTimestampMode(): void {
    const order: TimestampMode[] = ["absolute", "relative", "delta"];
    timestampMode = order[(order.indexOf(timestampMode) + 1) % order.length];
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

  const configLabel = $derived(
    formatConfig(deviceConfig?.config as Parameters<typeof formatConfig>[0]),
  );

  /** Dismissing is per-view and deliberately not persisted: a board that
   * legitimately streams binary (a camera SoC's protocol frames, a firmware
   * pushing packed telemetry) will trip the undecodable-ratio threshold
   * forever, and nagging about it every render is worse than useless. It
   * comes back on reload, or when the device is reselected, because by then
   * the situation may genuinely be different. */
  let baudWarningDismissed = $state(false);

  /** Coarse clock driving the staleness gate below. One tick a second is
   * ample for a 60s threshold, and without it the banner would only ever
   * re-evaluate when something else happened to re-render this component —
   * which, on a device that has gone silent, is precisely never. */
  let nowMs = $state(Date.now());

  /** How stale the newest output can be before this view stops calling it
   * "recent".
   *
   * `compute_decode_health` samples the last `DECODE_HEALTH_WINDOW_LINES`
   * assembled lines with no bound on how old they are, so a device that
   * emitted a garbled burst and then went quiet keeps reporting the same
   * ratio over the same stale bytes indefinitely — the suggestion never
   * expires on its own. Warning about output that stopped arriving an hour
   * ago is worse than not warning: it sends someone changing baud rates
   * against a stream that isn't there (which is exactly what happens — every
   * candidate rate reports an identical ratio, because it is being computed
   * over the identical stale sample each time).
   *
   * Gating on this view's own last arrival is the honest fix available from
   * here. The underlying window is the daemon's to bound. */
  const DECODE_HEALTH_STALE_MS = 60_000;

  /** When the *device* last said something. Deliberately only `line` items:
   * a `connect`/`config_change` the daemon wrote a second ago is the broker
   * talking, not the port, and counting it would keep the staleness gate
   * below permanently open on a silent device — every daemon restart would
   * re-arm a warning about bytes that arrived hours earlier. */
  const lastArrivalMs = $derived.by((): number | null => {
    trackVersion();
    for (let i = buffer.items.length - 1; i >= 0; i--) {
      const item = buffer.items[i];
      if (item.kind === "line") return item.tMs;
    }
    return null;
  });

  const suggestedBaud = $derived.by((): number | null => {
    if (baudWarningDismissed) return null;
    const suggestion = deviceConfig?.decode_health?.suggested_baud ?? null;
    if (suggestion === null) return null;
    if (lastArrivalMs === null) return null;
    if (nowMs - lastArrivalMs > DECODE_HEALTH_STALE_MS) return null;
    return suggestion;
  });

  const undecodablePct = $derived(
    Math.round((deviceConfig?.decode_health?.undecodable_ratio ?? 0) * 100),
  );

  async function applySuggestedBaud(): Promise<void> {
    if (suggestedBaud === null || applyingSuggestedBaud) return;
    applyingSuggestedBaud = true;
    try {
      await setDeviceConfig(deviceId, { baud: suggestedBaud });
      refreshConfig();
    } catch {
      // The failure is already visible: the config chip doesn't change and
      // no `config_change` row appears. Surfacing a second error string in
      // a one-click banner would be noise.
    } finally {
      applyingSuggestedBaud = false;
    }
  }

  /** One consistent clock format for everything in this pane. The footer
   * previously used `toLocaleTimeString()` while every row used
   * `HH:MM:SS.mmm`, so the same instant read two different ways on one
   * screen. */
  function clockTime(tWall: string): string {
    const d = new Date(tWall);
    if (Number.isNaN(d.getTime())) return tWall;
    const p = (n: number, w = 2) => String(n).padStart(w, "0");
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  }
</script>

<section class="live-log" data-testid="live-log" data-device={deviceId}>
  <div class="status-bar" data-testid="status-bar">
    {#if headerLead}{@render headerLead()}{/if}
    <span
      class="dot"
      class:connected={streamState === "open"}
      data-testid="connection-dot"
      data-state={streamState}
      title="Log stream: {streamState}"
    ></span>
    <span class="config-chip-wrap">
      <button
        type="button"
        class="config-chip tnum"
        data-testid="config-chip"
        bind:this={configChipEl}
        title="Port settings"
        onclick={() => (popoverOpen = !popoverOpen)}
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

  {#if suggestedBaud !== null}
    <!-- The daemon already knew this; it just wasn't saying it anywhere the
         person staring at the garbage would look.

         Wording matters here. `suggest_alternate_baud` picks the first entry
         in a list of common rates that isn't the current one — its own doc
         comment says a wrong-baud mismatch corrupts bits during sampling, so
         the correct rate cannot be recovered from the already-corrupted
         bytes. The only measured claim is the undecodable ratio; the rate is
         a next-thing-to-try. Saying anything stronger ("these bytes fit 74880
         better") would send someone confidently to a rate nothing measured. -->
    <div class="baud-warning" role="status" data-testid="baud-warning">
      <span>
        {undecodablePct}% of recent output didn't decode as text — often a baud mismatch.
        <span class="qualifier">
          {suggestedBaud} is just the next common rate to try, not a reading off these bytes.
        </span>
      </span>
      <button
        type="button"
        data-testid="baud-warning-apply"
        disabled={applyingSuggestedBaud}
        onclick={applySuggestedBaud}
      >
        {applyingSuggestedBaud ? "Switching…" : `Try ${suggestedBaud}`}
      </button>
      <button
        type="button"
        class="dismiss"
        data-testid="baud-warning-dismiss"
        title="Some firmware legitimately sends binary — dismiss if that's this board"
        onclick={() => (baudWarningDismissed = true)}
      >
        Dismiss
      </button>
    </div>
  {/if}

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
      class="filter"
      class:invalid={filterError !== null}
      placeholder="Filter with a regex"
      aria-label="Filter log lines with a regular expression"
      data-testid="filter-input"
      bind:this={filterEl}
      bind:value={filterText}
      oninput={applyFilter}
    />
    <div class="segmented" role="group" aria-label="What the filter does">
      <button
        type="button"
        class:on={filterMode === "narrow"}
        aria-pressed={filterMode === "narrow"}
        data-testid="filter-mode-narrow"
        title="Show only matching lines"
        onclick={() => setFilterMode("narrow")}>Only matches</button
      >
      <button
        type="button"
        class:on={filterMode === "mark"}
        aria-pressed={filterMode === "mark"}
        data-testid="filter-mode-mark"
        title="Keep every line, highlight the matches"
        onclick={() => setFilterMode("mark")}>Highlight</button
      >
    </div>

    <button
      type="button"
      class="chip"
      data-testid="timestamp-mode-toggle"
      title="Change how timestamps are shown"
      onclick={cycleTimestampMode}
    >
      {TIMESTAMP_LABELS[timestampMode]}
    </button>

    <label class="chip toggle">
      <input type="checkbox" data-testid="hex-toggle" bind:checked={hexMode} />
      Hex
    </label>

    <button
      type="button"
      class="chip"
      class:active={!following}
      data-testid="pause-toggle"
      onclick={togglePause}
    >
      {following ? "Pause" : "Resume"}
    </button>

    <button
      type="button"
      class="chip"
      data-testid="clear-view"
      title="Clears this tab only — the daemon's recording is untouched"
      onclick={clearView}>Clear view</button
    >

    {#if !following}
      <span class="paused-indicator" data-testid="paused-indicator">Paused</span>
    {/if}
  </div>
  {#if filterError}
    <div class="filter-error" data-testid="filter-error">
      Not a valid regex — {filterError}
    </div>
  {/if}

  <div
    class="viewport"
    data-testid="log-viewport"
    data-following={following}
    bind:this={containerEl}
    onscroll={onScroll}
  >
    <div class="spacer" style="height: {totalHeight}px; min-width: {contentWidthCh}ch;">
      {#each rows as row (row.item.id)}
        <div class="positioned" style="transform: translateY({row.top}px);">
          <LogRow
            item={row.item}
            timestamp={row.timestamp}
            expanded={isExpanded(row.item.id)}
            onToggleExpand={toggleExpand}
            {deviceId}
            {highlightRe}
            highlighted={row.item.id === highlightedItemId}
            onReverted={refreshConfig}
          />
        </div>
      {/each}
      {#if totalItems === 0}
        <p class="empty">
          Nothing recorded yet on this port. Recording starts the moment the device enumerates —
          if this stays empty, the board may not be sending, or you may be on the wrong port.
        </p>
      {/if}
    </div>
  </div>

  {#if pendingCount > 0}
    <button
      type="button"
      class="resume-pill"
      data-testid="resume-following-pill"
      onclick={resumeFollowing}
    >
      {pendingCount.toLocaleString()} new {pendingCount === 1 ? "line" : "lines"} below — jump to
      live
    </button>
  {/if}

  <div class="footer-status" data-testid="error-counts">
    <span>{errorCountsLabel}</span>
    <span class="sep" aria-hidden="true">·</span>
    <span class="tnum">seq {lastOffsetLabel}</span>
    {#if recordingSinceLabel}
      <span class="sep" aria-hidden="true">·</span>
      <span class="tnum">recording since {clockTime(recordingSinceLabel)}</span>
    {/if}
  </div>

  <!-- E2E performance-test hooks only — never shown, no user-facing purpose.
       See `lastFilterElapsedMs`/`bufferedCount`'s doc comments. -->
  <span data-testid="buffered-count" style="display: none;">{bufferedCount}</span>
  <span data-testid="filter-elapsed-ms" style="display: none;">{lastFilterElapsedMs}</span>
</section>

<style>
  .live-log {
    display: flex;
    flex-direction: column;
    /* Anchors the floating "jump to live" pill over the log. */
    position: relative;
    /* Take everything the shell has left over, and allow shrinking below
     * content size so the inner viewport (not the page) does the scrolling. */
    flex: 1;
    min-height: 0;
    border-top: 1px solid var(--border);
    background: var(--surface);
  }

  .status-bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border);
    flex-wrap: wrap;
  }

  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--gate);
    flex: none;
  }
  .dot.connected {
    background: var(--ok);
  }

  .config-chip-wrap {
    position: relative;
    display: inline-flex;
  }

  .config-chip {
    font: inherit;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
    color: var(--text-dim);
    background: transparent;
    cursor: pointer;
  }
  .config-chip:hover {
    border-color: var(--border-strong);
    color: var(--text);
  }

  .baud-warning {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
    padding: var(--space-2) var(--space-3);
    background: var(--tx-bg);
    border-bottom: 1px solid var(--border);
    border-left: var(--gutter-w) solid var(--warn);
    font-size: var(--text-sm);
  }

  .baud-warning button {
    font: inherit;
    font-weight: 600;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--warn);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--warn);
    cursor: pointer;
    flex: none;
  }

  .baud-warning span {
    flex: 1;
    min-width: 12rem;
  }

  .baud-warning .qualifier {
    color: var(--text-dim);
    font-size: var(--text-xs);
  }

  .baud-warning .dismiss {
    font-weight: 400;
    border-color: var(--border);
    color: var(--text-dim);
  }

  .timeline-selection-info {
    padding: var(--space-1) var(--space-3);
    font-size: var(--text-xs);
    color: var(--text-dim);
    border-bottom: 1px solid var(--border);
  }

  .stream-error {
    color: var(--gate);
    margin-left: auto;
    font-size: var(--text-sm);
  }

  .controls {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border);
    flex-wrap: wrap;
  }

  .filter {
    flex: 1;
    min-width: 8rem;
    font: inherit;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    background: var(--surface-sunken);
    color: var(--text);
  }

  .filter.invalid {
    border-color: var(--gate);
  }

  .segmented {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    overflow: hidden;
    flex: none;
  }

  .segmented button {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border: none;
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
    white-space: nowrap;
  }

  .segmented button.on {
    background: var(--rx-bg);
    color: var(--rx);
    font-weight: 600;
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
    white-space: nowrap;
    flex: none;
  }

  .chip:hover {
    color: var(--text);
    border-color: var(--border-strong);
  }

  .chip.active {
    color: var(--warn);
    border-color: var(--warn);
  }

  .toggle {
    cursor: pointer;
  }

  .paused-indicator {
    margin-left: auto;
    color: var(--warn);
    font-size: var(--text-sm);
    font-weight: 600;
  }

  .filter-error {
    padding: var(--space-1) var(--space-3);
    color: var(--gate);
    background: var(--gate-bg);
    font-size: var(--text-sm);
  }

  .viewport {
    position: relative;
    flex: 1;
    /* Never let the chrome squeeze the log to nothing. On a short window
     * (a laptop with a split screen, a small browser pane) the header,
     * timeline, controls, footer, write bar and status bar together can
     * exceed the viewport height, and a plain `min-height: 0` resolves that
     * by shrinking the one element the page exists for down to zero pixels —
     * which is what happened. Roughly five rows is the floor; `.stage`'s
     * `overflow: hidden` (App.svelte) bounds what happens past it, so the
     * chrome below the log clips before the log itself disappears.
     *
     * Deliberately *not* solved by hiding chrome at a height breakpoint: the
     * first attempt dropped the timeline below 46rem, which is under a
     * standard 720px-tall browser window — removing a navigation control
     * from the most common laptop viewport there is. A floor on the log is
     * the fix; the chrome is not the problem. */
    min-height: 6.5rem;
    /* One scroll surface for the whole log, both axes — see the doc comment
     * on why per-row horizontal scrolling was worse than no scrolling. */
    overflow: auto;
    background: var(--surface-sunken);
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

  .empty {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    text-align: center;
    margin: 0;
    padding: var(--space-6);
    max-width: 34rem;
    margin-inline: auto;
    color: var(--text-dim);
    font-size: var(--text-sm);
    line-height: 1.6;
  }

  .resume-pill {
    position: absolute;
    left: 50%;
    bottom: 3.5rem;
    transform: translateX(-50%);
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-pill);
    border: 1px solid var(--rx);
    background: var(--surface-raised);
    color: var(--rx);
    cursor: pointer;
    z-index: 3;
    box-shadow: 0 4px 14px rgb(0 0 0 / 0.25);
  }

  .footer-status {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    flex-wrap: wrap;
    padding: var(--space-1) var(--space-3);
    border-top: 1px solid var(--border);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--text-faint);
  }

  .sep {
    color: var(--border-strong);
  }

  @media (max-width: 40rem) {
    .filter {
      flex: 1 1 100%;
    }
  }
</style>
