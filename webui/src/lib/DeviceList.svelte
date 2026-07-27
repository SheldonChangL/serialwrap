<script lang="ts">
  import { fetchDevices, type DeviceSummary } from "./api";

  type LoadState =
    | { kind: "loading" }
    | { kind: "error"; message: string }
    | { kind: "loaded"; devices: DeviceSummary[] };

  let state = $state<LoadState>({ kind: "loading" });

  async function load(): Promise<void> {
    state = { kind: "loading" };
    try {
      const devices = await fetchDevices();
      state = { kind: "loaded", devices };
    } catch (e) {
      state = { kind: "error", message: e instanceof Error ? e.message : String(e) };
    }
  }

  load();
</script>

<section class="devices" data-testid="device-list" data-state={state.kind}>
  <div class="header">
    <h2>Devices</h2>
    <button type="button" onclick={load}>Refresh</button>
  </div>

  {#if state.kind === "loading"}
    <p class="hint">Loading…</p>
  {:else if state.kind === "error"}
    <p class="hint error">GET /api/devices failed: {state.message}</p>
  {:else if state.devices.length === 0}
    <p class="hint">No devices connected.</p>
  {:else}
    <ul>
      {#each state.devices as device (device.id)}
        <li>
          <span class="dot" class:connected={device.connected}></span>
          <span class="id">{device.id}</span>
          <span class="path">{device.path ?? "(no path)"}</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .devices {
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    padding: 1rem;
    background: var(--surface);
  }

  .header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }

  h2 {
    margin: 0;
    font-size: 1rem;
  }

  button {
    font: inherit;
    padding: 0.25rem 0.75rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }

  .hint {
    color: var(--text-dim);
    margin: 0.75rem 0 0;
  }

  .hint.error {
    color: var(--dot-closed);
  }

  ul {
    list-style: none;
    margin: 0.75rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }

  li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.875rem;
  }

  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--dot-closed);
  }

  .dot.connected {
    background: var(--dot-open);
  }

  .path {
    color: var(--text-dim);
  }
</style>
