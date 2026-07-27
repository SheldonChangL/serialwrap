<script lang="ts">
  import { tick } from "svelte";
  import { fetchDeviceConfig, setControlLines, setDeviceConfig, type DeviceConfig } from "./logStream";

  interface Props {
    deviceId: string;
    open: boolean;
    onClose: () => void;
    /** Called after a successful `Apply`/revert/control-line change — the
     * parent (`LiveLog.svelte`) uses this to refresh its own status-bar
     * config label immediately, rather than waiting for the `config_change`
     * event to round-trip back over the WS stream (it still will, for
     * *other* open tabs — see the T5.3 broadcast acceptance criterion). */
    onApplied: () => void;
    /** The config-chip button that opens this popover — its position is
     * used to place the popover with `position: fixed` (viewport-relative,
     * escaping `LiveLog.svelte`'s `.live-log { overflow: hidden }` clip
     * rather than being cut off/unclickable inside it, which a plain
     * `position: absolute` nested popover was). */
    anchorEl: HTMLElement | undefined;
  }
  const { deviceId, open, onClose, onApplied, anchorEl }: Props = $props();

  let popoverEl: HTMLDivElement | undefined = $state();
  let popoverStyle = $state("position: fixed; visibility: hidden;");

  const VIEWPORT_MARGIN = 8;

  /** Clamp the popover fully inside the viewport, flipping above the
   * anchor when there isn't enough room below it — a fixed-height guess
   * before the popover has actually rendered would be wrong the moment
   * `decode_health`'s hint paragraph does or doesn't appear, so this reads
   * `popoverEl`'s *real*, already-laid-out height instead (this function
   * only runs once `popoverEl` exists — see the `$effect` below). */
  function positionPopover(): void {
    if (!anchorEl || !popoverEl) return;
    const anchorRect = anchorEl.getBoundingClientRect();
    const popRect = popoverEl.getBoundingClientRect();
    const spaceBelow = window.innerHeight - anchorRect.bottom;
    const openUpward = spaceBelow < popRect.height + VIEWPORT_MARGIN && anchorRect.top > spaceBelow;
    const top = openUpward
      ? Math.max(VIEWPORT_MARGIN, anchorRect.top - popRect.height - 4)
      : Math.min(anchorRect.bottom + 4, window.innerHeight - popRect.height - VIEWPORT_MARGIN);
    const left = Math.min(
      Math.max(VIEWPORT_MARGIN, anchorRect.left),
      window.innerWidth - popRect.width - VIEWPORT_MARGIN,
    );
    popoverStyle = `position: fixed; top: ${Math.max(VIEWPORT_MARGIN, top)}px; left: ${Math.max(VIEWPORT_MARGIN, left)}px; max-height: calc(100vh - ${VIEWPORT_MARGIN * 2}px); overflow-y: auto;`;
  }

  // Runs once `popoverEl` exists (the `{#if open}` block just mounted) —
  // at that point it has real, laid-out dimensions to measure, unlike
  // trying to position it from the `open` transition itself, before any
  // content has rendered at all.
  $effect(() => {
    if (popoverEl) positionPopover();
  });

  /** Baud rates common enough to offer as one-click presets — the UX-design
   * wiki's own point is that this list is deliberately *not* exhaustive:
   * "Custom…" plus a freely-typeable numeric field is the first-class path,
   * this is just a shortcut for the common case. */
  const COMMON_BAUDS = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 74880, 115200, 230400, 460800, 921600];

  let baud = $state(9600);
  let dataBits = $state<"five" | "six" | "seven" | "eight">("eight");
  let parity = $state<"none" | "odd" | "even">("none");
  let stopBits = $state<"one" | "two">("one");
  let flowControl = $state<"none" | "software" | "hardware">("none");
  let dontTouchOnOpen = $state(true);
  let dtrAssert = $state(false);
  let rtsAssert = $state(false);

  let decodeHealth = $state<DeviceConfig["decode_health"]>(undefined);
  let loadError = $state<string | null>(null);
  let applyError = $state<string | null>(null);
  let applying = $state(false);
  let controlLineBusy = $state(false);

  function loadFromConfig(c: Record<string, unknown>): void {
    baud = Number(c.baud ?? 9600);
    dataBits = (c.data_bits as typeof dataBits) ?? "eight";
    parity = (c.parity as typeof parity) ?? "none";
    stopBits = (c.stop_bits as typeof stopBits) ?? "one";
    flowControl = (c.flow_control as typeof flowControl) ?? "none";
    const openLines = c.open_control_lines as { mode?: string; dtr?: boolean; rts?: boolean } | undefined;
    if (openLines?.mode === "assert") {
      dontTouchOnOpen = false;
      dtrAssert = openLines.dtr ?? false;
      rtsAssert = openLines.rts ?? false;
    } else {
      dontTouchOnOpen = true;
    }
  }

  async function refresh(): Promise<void> {
    loadError = null;
    try {
      const full = await fetchDeviceConfig(deviceId);
      loadFromConfig(full.config);
      decodeHealth = full.decode_health;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
    // `refresh` is async and can change the popover's actual rendered
    // height after the fact (the decode-health hint paragraph, in
    // particular, only appears once this resolves) — `tick()` first so the
    // DOM has actually applied the new `decodeHealth`-driven content before
    // measuring, rather than repositioning against stale, pre-fetch layout.
    await tick();
    positionPopover();
  }

  // Refetch every time the popover opens, not just once on mount — the
  // decode-health hint in particular needs to reflect whatever has arrived
  // on the device *since* it was last opened (see `compute_decode_health`'s
  // doc comment: a short recent window, not a running-forever average).
  let wasOpen = false;
  $effect(() => {
    if (open && !wasOpen) void refresh();
    wasOpen = open;
  });

  const baudIsCustom = $derived(!COMMON_BAUDS.includes(baud));

  function onBaudPresetChange(e: Event): void {
    const value = (e.target as HTMLSelectElement).value;
    if (value !== "custom") baud = Number(value);
  }

  function useSuggestedBaud(): void {
    if (decodeHealth?.suggested_baud) baud = decodeHealth.suggested_baud;
  }

  async function applyConfig(): Promise<void> {
    applying = true;
    applyError = null;
    try {
      const patch: Record<string, unknown> = {
        baud,
        data_bits: dataBits,
        parity,
        stop_bits: stopBits,
        flow_control: flowControl,
        open_control_lines: dontTouchOnOpen
          ? { mode: "preserve" }
          : { mode: "assert", dtr: dtrAssert, rts: rtsAssert },
      };
      await setDeviceConfig(deviceId, patch);
      onApplied();
    } catch (e) {
      applyError = e instanceof Error ? e.message : String(e);
    } finally {
      applying = false;
    }
  }

  /** Flipping a control-line toggle is a live, immediate, individually
   * risky action — deliberately its own request rather than folded into
   * `applyConfig`'s general "Apply" click (see `crates/serialwrapd/src/web/api.rs`'s
   * `set_control_lines` doc comment: baud/frame changes must never, as a
   * side effect of one button, also pulse a physical line). */
  async function toggleDtr(): Promise<void> {
    controlLineBusy = true;
    const previous = dtrAssert;
    dtrAssert = !dtrAssert;
    try {
      await setControlLines(deviceId, { dtr: dtrAssert });
      onApplied();
    } catch (e) {
      // Roll back the optimistic flip: a display that claims "asserted"
      // when the request actually failed would be actively misleading for
      // a physical line this dangerous to be wrong about.
      dtrAssert = previous;
      applyError = e instanceof Error ? e.message : String(e);
    } finally {
      controlLineBusy = false;
    }
  }

  async function toggleRts(): Promise<void> {
    controlLineBusy = true;
    const previous = rtsAssert;
    rtsAssert = !rtsAssert;
    try {
      await setControlLines(deviceId, { rts: rtsAssert });
      onApplied();
    } catch (e) {
      rtsAssert = previous;
      applyError = e instanceof Error ? e.message : String(e);
    } finally {
      controlLineBusy = false;
    }
  }
</script>

{#if open}
  <div class="popover" data-testid="config-popover" style={popoverStyle} bind:this={popoverEl}>
    <section class="group">
      <h3>Baud rate</h3>
      <div class="baud-row">
        <select data-testid="baud-preset-select" value={baudIsCustom ? "custom" : String(baud)} onchange={onBaudPresetChange}>
          {#each COMMON_BAUDS as preset (preset)}
            <option value={String(preset)}>{preset}</option>
          {/each}
          <option value="custom">Custom…</option>
        </select>
        <input
          type="number"
          min="1"
          data-testid="baud-input"
          bind:value={baud}
        />
      </div>
      {#if decodeHealth?.suggested_baud}
        <p class="hint" data-testid="decode-health-hint">
          &#9432; {Math.round(decodeHealth.undecodable_ratio * 100)}% of recent output failed to decode — baud may
          be wrong.
          <button type="button" data-testid="use-suggested-baud" onclick={useSuggestedBaud}>
            Try {decodeHealth.suggested_baud}
          </button>
        </p>
      {/if}
    </section>

    <section class="group">
      <h3>Frame</h3>
      <label>
        Data
        <select data-testid="data-bits-select" bind:value={dataBits}>
          <option value="five">5</option>
          <option value="six">6</option>
          <option value="seven">7</option>
          <option value="eight">8</option>
        </select>
      </label>
      <label>
        Parity
        <select data-testid="parity-select" bind:value={parity}>
          <option value="none">None</option>
          <option value="odd">Odd</option>
          <option value="even">Even</option>
        </select>
      </label>
      <label>
        Stop
        <select data-testid="stop-bits-select" bind:value={stopBits}>
          <option value="one">1</option>
          <option value="two">2</option>
        </select>
      </label>
      <label>
        Flow control
        <select data-testid="flow-control-select" bind:value={flowControl}>
          <option value="none">None</option>
          <option value="software">Software</option>
          <option value="hardware">Hardware</option>
        </select>
      </label>
    </section>

    <section class="group control-lines">
      <h3>Control lines <span class="warn">(may reset the board)</span></h3>
      <div class="control-line-row">
        <button
          type="button"
          data-testid="dtr-toggle"
          data-asserted={dtrAssert}
          disabled={controlLineBusy}
          onclick={toggleDtr}
        >
          DTR {dtrAssert ? "asserted" : "released"}
        </button>
        <button
          type="button"
          data-testid="rts-toggle"
          data-asserted={rtsAssert}
          disabled={controlLineBusy}
          onclick={toggleRts}
        >
          RTS {rtsAssert ? "asserted" : "released"}
        </button>
      </div>
      <label class="dont-touch">
        <input type="checkbox" data-testid="dont-touch-on-open" bind:checked={dontTouchOnOpen} />
        Don't touch DTR/RTS when opening
      </label>
    </section>

    <section class="group memory-note">
      Remembered for <code>{deviceId}</code>.
    </section>

    {#if loadError}
      <p class="error" data-testid="config-load-error">{loadError}</p>
    {/if}
    {#if applyError}
      <p class="error" data-testid="config-apply-error">{applyError}</p>
    {/if}

    <div class="footer">
      <button type="button" data-testid="apply-config" disabled={applying} onclick={applyConfig}>Apply</button>
      <button type="button" data-testid="close-popover" onclick={onClose}>Close</button>
      <span class="broadcast-note">Broadcast to all clients and recorded. One-click revert from the log.</span>
    </div>
  </div>
{/if}

<style>
  .popover {
    /* `position`/`top`/`left` come from the inline `style` (`popoverStyle`)
     * — computed at open time from the config-chip button's own bounding
     * rect, viewport-relative so it escapes `.live-log`'s `overflow:
     * hidden` clip (see this component's `anchorEl` prop doc comment). */
    z-index: 10;
    width: 22rem;
    max-width: calc(100vw - 2rem);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    font-size: 0.8rem;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.3);
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    border-bottom: 1px solid var(--border);
    padding-bottom: 0.5rem;
  }
  .group:last-of-type {
    border-bottom: none;
  }

  h3 {
    margin: 0;
    font-size: 0.8rem;
  }

  .warn {
    color: var(--dot-closed);
    font-weight: 400;
    font-size: 0.7rem;
  }

  .baud-row {
    display: flex;
    gap: 0.4rem;
  }

  select,
  input[type="number"] {
    font: inherit;
    padding: 0.2rem 0.4rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
  }

  input[type="number"] {
    width: 6rem;
  }

  .hint {
    color: var(--text-dim);
    margin: 0;
  }
  .hint button {
    font: inherit;
    margin-left: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    border-radius: 0.3rem;
    cursor: pointer;
  }

  label {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    justify-content: space-between;
  }

  .control-line-row {
    display: flex;
    gap: 0.5rem;
  }
  .control-line-row button {
    flex: 1;
    font: inherit;
    padding: 0.3rem 0.5rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }
  .control-line-row button[data-asserted="true"] {
    border-color: var(--dot-closed);
  }

  .dont-touch {
    justify-content: flex-start;
  }

  .memory-note {
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .error {
    color: var(--dot-closed);
    margin: 0;
  }

  .footer {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .footer button {
    font: inherit;
    padding: 0.3rem 0.8rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }
  .broadcast-note {
    color: var(--text-dim);
    font-size: 0.7rem;
  }
</style>
