<script lang="ts">
  /**
   * The device selector — the control whose absence made the rest of this
   * GUI unusable.
   *
   * "One device per browser tab" stays the model (the UX-design wiki's
   * deliberate omission: no split views, no tab strip). What was missing is
   * that a tab had no way to say *which* device it was, so it always showed
   * whichever one the API happened to list first. Selection lives in the URL
   * (`?device=<id>`), which makes the model actually work: a second port is
   * a second browser tab, bookmarkable and reloadable, and the back button
   * does what it looks like it does.
   */
  import type { DeviceSummary } from "./api";
  import { deviceLabel } from "./devices";

  interface Props {
    /** Already sorted by `devices.ts`'s ranking. `App.svelte` owns the list
     * and its polling, because the shell needs the same data (to validate
     * `?device=` and to tell the write bar whether the port is open) and two
     * independent pollers would be able to disagree with each other. */
    devices: DeviceSummary[];
    loadError: string | null;
    selectedId: string | null;
    onSelect: (id: string) => void;
  }
  const { devices, loadError, selectedId, onSelect }: Props = $props();

  let open = $state(false);
  let rootEl: HTMLDivElement | undefined = $state();

  const selected = $derived(devices.find((d) => d.id === selectedId) ?? null);

  function choose(id: string): void {
    open = false;
    if (id !== selectedId) onSelect(id);
  }

  function onWindowPointerDown(e: PointerEvent): void {
    if (!open || !rootEl) return;
    if (!rootEl.contains(e.target as Node)) open = false;
  }

  function onWindowKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape" && open) {
      open = false;
      e.stopPropagation();
    }
  }
</script>

<svelte:window onpointerdown={onWindowPointerDown} onkeydown={onWindowKeyDown} />

<div class="picker" bind:this={rootEl} data-testid="device-picker">
  <button
    type="button"
    class="trigger"
    data-testid="device-picker-trigger"
    aria-haspopup="listbox"
    aria-expanded={open}
    aria-label="Serial port: {selected ? deviceLabel(selected) : 'none selected'}. Change port."
    onclick={() => (open = !open)}
  >
    <span class="dot" class:connected={selected?.connected} aria-hidden="true"></span>
    <span class="name">{selected ? deviceLabel(selected) : "No device"}</span>
    <span class="caret" aria-hidden="true">▾</span>
  </button>

  {#if open}
    <div class="menu" role="listbox" aria-label="Devices" data-testid="device-picker-menu">
      {#if loadError}
        <p class="empty">Can't reach the daemon — {loadError}</p>
      {:else if devices.length === 0}
        <p class="empty">
          No serial ports found. Plug a board in; recording starts the moment it enumerates.
        </p>
      {:else}
        {#each devices as device (device.id)}
          <button
            type="button"
            class="option"
            class:current={device.id === selectedId}
            role="option"
            aria-selected={device.id === selectedId}
            data-testid="device-picker-option"
            data-device-id={device.id}
            onclick={() => choose(device.id)}
          >
            <span class="dot" class:connected={device.connected} aria-hidden="true"></span>
            <span class="option-body">
              <span class="option-name">{deviceLabel(device)}</span>
              <span class="option-path">{device.path ?? device.id}</span>
            </span>
            {#if !device.connected}
              <span class="option-state">disconnected</span>
            {/if}
          </button>
        {/each}
      {/if}
    </div>
  {/if}
</div>

<style>
  .picker {
    position: relative;
    display: inline-flex;
  }

  .trigger {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font: inherit;
    font-family: var(--font-mono);
    font-size: var(--text-base);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--surface-raised);
    color: var(--text);
    cursor: pointer;
    max-width: 16rem;
  }

  .trigger:hover {
    border-color: var(--border-strong);
  }

  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .caret {
    color: var(--text-faint);
    font-size: var(--text-xs);
  }

  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--text-faint);
    flex: none;
  }
  .dot.connected {
    background: var(--ok);
  }

  .menu {
    position: absolute;
    top: calc(100% + var(--space-1));
    left: 0;
    z-index: 40;
    min-width: 22rem;
    max-width: min(28rem, 90vw);
    max-height: 60vh;
    overflow-y: auto;
    padding: var(--space-1);
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-md);
    box-shadow: 0 12px 32px rgb(0 0 0 / 0.28);
  }

  .empty {
    margin: 0;
    padding: var(--space-3);
    color: var(--text-dim);
    font-size: var(--text-sm);
    line-height: 1.5;
  }

  .option {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    text-align: left;
    font: inherit;
    padding: var(--space-2);
    border: none;
    border-radius: var(--radius-sm);
    background: none;
    color: inherit;
    cursor: pointer;
  }

  .option:hover {
    background: var(--surface-raised);
  }

  .option.current {
    background: var(--rx-bg);
  }

  .option-body {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }

  .option-name {
    font-family: var(--font-mono);
    font-size: var(--text-base);
  }

  .option-path {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .option-state {
    flex: none;
    font-size: var(--text-xs);
    color: var(--text-faint);
  }
</style>
