<script lang="ts">
  /**
   * The full port inventory, shown in a status-bar drawer.
   *
   * The device *picker* in the log header answers "which port am I reading";
   * this answers "what else is attached, and what does the daemon know about
   * it" — full ids and paths, including ports that have disconnected. Rows
   * are selectable here too, because a list of ports you can't switch to is
   * exactly the dead end this page used to be.
   */
  import { fetchDevices, type DeviceSummary } from "./api";
  import { deviceLabel, sortDevices } from "./devices";

  interface Props {
    selectedId?: string | null;
    onSelect?: (id: string) => void;
  }
  const { selectedId = null, onSelect }: Props = $props();

  type LoadState =
    | { kind: "loading" }
    | { kind: "error"; message: string }
    | { kind: "loaded"; devices: DeviceSummary[] };

  let state = $state<LoadState>({ kind: "loading" });

  async function load(): Promise<void> {
    state = { kind: "loading" };
    try {
      state = { kind: "loaded", devices: sortDevices(await fetchDevices()) };
    } catch (e) {
      state = { kind: "error", message: e instanceof Error ? e.message : String(e) };
    }
  }

  load();
</script>

<section class="devices" data-testid="device-list" data-state={state.kind}>
  <div class="header">
    <span class="label-eyebrow">Everything the daemon has seen</span>
    <button type="button" class="refresh" onclick={load}>Refresh</button>
  </div>

  {#if state.kind === "loading"}
    <p class="hint">Loading…</p>
  {:else if state.kind === "error"}
    <p class="hint error">Can't reach the daemon — {state.message}</p>
  {:else if state.devices.length === 0}
    <p class="hint">
      No serial ports found. Plug a board in; recording starts the moment it enumerates.
    </p>
  {:else}
    <ul>
      {#each state.devices as device (device.id)}
        <li>
          <button
            type="button"
            class="row"
            class:current={device.id === selectedId}
            disabled={!onSelect}
            aria-current={device.id === selectedId}
            onclick={() => onSelect?.(device.id)}
          >
            <span class="dot" class:connected={device.connected}></span>
            <span class="cell name">{deviceLabel(device)}</span>
            <span class="cell path">{device.path ?? "(no path)"}</span>
            <span class="cell id">{device.id}</span>
            {#if !device.connected}<span class="cell state">disconnected</span>{/if}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }

  .refresh {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
  }

  .refresh:hover {
    color: var(--text);
    border-color: var(--border-strong);
  }

  .hint {
    color: var(--text-dim);
    font-size: var(--text-sm);
    margin: var(--space-3) 0 0;
    line-height: 1.6;
  }

  .hint.error {
    color: var(--gate);
  }

  ul {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
  }

  .row {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    width: 100%;
    text-align: left;
    font: inherit;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border: none;
    border-radius: var(--radius-sm);
    background: none;
    color: inherit;
    cursor: pointer;
  }

  .row:hover:not(:disabled) {
    background: var(--surface-raised);
  }

  .row:disabled {
    cursor: default;
  }

  .row.current {
    background: var(--rx-bg);
  }

  .cell {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .name {
    flex: none;
    min-width: 10rem;
  }

  .path {
    flex: 1;
    color: var(--text-dim);
  }

  .id {
    color: var(--text-faint);
    font-size: var(--text-xs);
  }

  .state {
    flex: none;
    color: var(--text-faint);
    font-size: var(--text-xs);
  }

  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--text-faint);
    flex: none;
    align-self: center;
  }

  .dot.connected {
    background: var(--ok);
  }

  /* Narrow: the daemon id is the first thing to drop — the path already
   * identifies the port, and the id is only needed when you're comparing
   * against a CLI invocation. */
  @media (max-width: 40rem) {
    .id {
      display: none;
    }
    .name {
      min-width: 0;
    }
  }
</style>
