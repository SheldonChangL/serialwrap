<script lang="ts">
  import type { LogItem } from "./liveLog";
  import { setDeviceConfig } from "./logStream";

  interface Props {
    item: LogItem;
    timestamp: string;
    expanded: boolean;
    onToggleExpand: (id: number) => void;
    /** Needed only for the `config_change` row's "還原" (revert) button
     * (T5.3, issue #20) — every other row kind ignores this. */
    deviceId?: string;
    /** Flashes a highlight after a timeline jump lands on this row (T5.3
     * acceptance criterion 1) — see `LiveLog.svelte`'s `jumpToSeq`. */
    highlighted?: boolean;
    onReverted?: () => void;
  }

  const { item, timestamp, expanded, onToggleExpand, deviceId, highlighted = false, onReverted }: Props = $props();

  function fmtCount(n: number): string {
    return n.toLocaleString();
  }

  let reverting = $state(false);
  let revertError = $state<string | null>(null);

  async function revertConfigChange(): Promise<void> {
    if (item.kind !== "event" || !deviceId) return;
    const old = item.extra.old;
    if (old === null || old === undefined) return;
    reverting = true;
    revertError = null;
    try {
      await setDeviceConfig(deviceId, old as Record<string, unknown>);
      onReverted?.();
    } catch (e) {
      revertError = e instanceof Error ? e.message : String(e);
    } finally {
      reverting = false;
    }
  }
</script>

{#if item.kind === "gap"}
  <div class="row gap" data-testid="log-row" data-row-kind="gap" data-highlighted={highlighted}>
    <span class="chip">+{item.deltaS.toFixed(1)}s</span>
  </div>
{:else if item.kind === "line"}
  <div
    class="row data"
    class:folded={item.folded}
    class:highlighted
    data-testid="log-row"
    data-row-kind="line"
    data-folded={item.folded}
    data-highlighted={highlighted}
    data-binary={item.render.kind === "binary_summary" || (item.render.kind === "text" && item.render.rawHex !== null)}
  >
    <span class="ts">{timestamp}</span>
    {#if item.folded}
      <button type="button" class="fold-toggle" onclick={() => onToggleExpand(item.id)}>
        <span class="bar" aria-hidden="true"></span>
        {#if item.render.kind === "text"}
          <span class="text">{item.render.text}</span>
        {:else}
          <span class="text">[{item.render.length} bytes binary]</span>
        {/if}
        <span class="meta">
          repeated {fmtCount(item.count)}&times;
          {#if expanded}
            ({item.tWall} &ndash; {item.lastTWall}) &mdash; collapse
          {:else}
            &mdash; expand
          {/if}
        </span>
      </button>
    {:else if item.render.kind === "binary_summary"}
      <button type="button" class="binary-toggle" onclick={() => onToggleExpand(item.id)}>
        {#if expanded}
          <span class="hex">{item.render.hexPreview}</span>
        {:else}
          <span class="text">[{item.render.length} bytes binary &mdash; view as hex]</span>
        {/if}
      </button>
    {:else if item.render.rawHex !== null}
      <button type="button" class="binary-toggle" onclick={() => onToggleExpand(item.id)}>
        <span class="text">{item.render.text}</span>
        {#if expanded}
          <span class="hex">{item.render.rawHex}</span>
        {:else}
          <span class="hint">[view as hex]</span>
        {/if}
      </button>
    {:else}
      <span class="text">{item.render.text}</span>
    {/if}
  </div>
{:else if item.kind === "tx"}
  <div class="row event tx" class:highlighted data-testid="log-row" data-row-kind="tx" data-highlighted={highlighted}>
    <span class="ts">{timestamp}</span>
    <span class="bar" aria-hidden="true"></span>
    <span class="label">TX &middot; {item.client} ({item.clientType})</span>
    <span class="text">{item.text}</span>
    <span class="gate-badge">{item.gate}</span>
  </div>
{:else if item.kind === "event"}
  <div
    class="row event"
    class:highlighted
    data-testid="log-row"
    data-row-kind="event"
    data-event-name={item.name}
    data-highlighted={highlighted}
  >
    <span class="ts">{timestamp}</span>
    <span class="bar" aria-hidden="true"></span>
    <span class="label">{item.name}</span>
    <span class="text">{JSON.stringify(item.extra)}</span>
    {#if item.name === "config_change" && deviceId}
      <button
        type="button"
        class="revert-btn"
        data-testid="config-revert"
        disabled={reverting || item.extra.old === null || item.extra.old === undefined}
        onclick={revertConfigChange}
      >
        {reverting ? "reverting…" : "revert"}
      </button>
      {#if revertError}
        <span class="revert-error">{revertError}</span>
      {/if}
    {/if}
  </div>
{:else}
  <div
    class="row event gate"
    class:highlighted
    data-testid="log-row"
    data-row-kind="gate"
    data-gate-action={item.action}
    data-highlighted={highlighted}
  >
    <span class="ts">{timestamp}</span>
    <span class="bar" aria-hidden="true"></span>
    <span class="label">gate: {item.action}</span>
    <span class="text">{item.reason}</span>
  </div>
{/if}

<style>
  .row {
    height: 22px;
    line-height: 22px;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0 0.5rem;
    white-space: nowrap;
    overflow-x: auto;
    scrollbar-width: none;
    box-sizing: border-box;
  }
  .row::-webkit-scrollbar {
    display: none;
  }

  .data {
    font-family: var(--font-mono);
    font-size: 0.8125rem;
  }

  .event {
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    background: var(--event-bg, rgba(88, 166, 255, 0.08));
    border-left: 3px solid var(--event-band, #58a6ff);
  }

  .event.tx {
    --event-band: #d29922;
    --event-bg: rgba(210, 153, 34, 0.1);
  }

  .event.gate {
    --event-band: #f85149;
    --event-bg: rgba(248, 81, 73, 0.1);
  }

  .gap {
    justify-content: center;
    font-family: var(--font-sans);
    font-size: 0.75rem;
    color: var(--text-dim);
  }

  .gap .chip {
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 0 0.5rem;
    background: var(--surface-raised);
  }

  .ts {
    color: var(--text-dim);
    flex: none;
    font-variant-numeric: tabular-nums;
  }

  .bar {
    display: inline-block;
    width: 3px;
    align-self: stretch;
  }

  .label {
    flex: none;
    font-weight: 600;
    color: var(--event-band, inherit);
  }

  .gate-badge {
    flex: none;
    color: var(--text-dim);
    font-size: 0.7rem;
    text-transform: uppercase;
  }

  .fold-toggle,
  .binary-toggle {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    cursor: pointer;
    padding: 0;
  }

  .meta,
  .hint {
    color: var(--text-dim);
  }

  .hex {
    color: var(--text-dim);
    letter-spacing: 0.03em;
  }

  .folded .bar {
    background: var(--border);
  }

  .highlighted {
    animation: highlight-flash 1.2s ease-out 2;
  }

  @keyframes highlight-flash {
    0% {
      background-color: rgba(88, 166, 255, 0.35);
    }
    100% {
      background-color: transparent;
    }
  }

  .revert-btn {
    flex: none;
    font: inherit;
    font-size: 0.7rem;
    padding: 0.05rem 0.4rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }
  .revert-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .revert-error {
    color: var(--dot-closed);
    font-size: 0.7rem;
  }
</style>
