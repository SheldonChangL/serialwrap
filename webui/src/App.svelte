<script lang="ts">
  /**
   * The application shell.
   *
   * # Layout thesis: the log is the page
   *
   * This used to be five equally-weighted cards stacked down a scrolling
   * document — live log, clients, audit, export, devices — which gave the
   * one thing an operator opens this page to see about a fifth of the screen
   * and buried the device list below everything else. The shell is now a
   * fixed-height column:
   *
   *     approval card   (only when a write is waiting on a human)
   *     log             (everything left over)
   *     write bar       (the operator's half of the conversation)
   *     status bar      (counters, and the drawers everything else moved to)
   *
   * Clients and audit became drawers because they are reference material —
   * consulted when a question comes up, not watched. Export was already a
   * dialog and stays one. The device list stopped being a panel at all: it
   * became the picker in the log's own header, which is where you look when
   * you want to know what you're reading.
   *
   * # Device selection lives in the URL
   *
   * `?device=<id>` is the whole of this page's state. It keeps "one device
   * per browser tab" (the UX-design wiki's deliberate omission) honest: a
   * second port is a second tab, reload and back both work, and a link to a
   * specific board is just a link.
   */
  import { onDestroy, onMount } from "svelte";
  import { Connection, type ConnectionInfo } from "./lib/connection";
  import ConnectionStatus from "./lib/ConnectionStatus.svelte";
  import DevicePicker from "./lib/DevicePicker.svelte";
  import Drawer from "./lib/Drawer.svelte";
  import LiveLog from "./lib/LiveLog.svelte";
  import WriteBar from "./lib/WriteBar.svelte";
  import ApprovalCardHost from "./lib/ApprovalCardHost.svelte";
  import ClientsPanel from "./lib/ClientsPanel.svelte";
  import AuditPanel from "./lib/AuditPanel.svelte";
  import ExportDialog from "./lib/ExportDialog.svelte";
  import DeviceList from "./lib/DeviceList.svelte";
  import { fetchDevices, type DeviceSummary } from "./lib/api";
  import { pickDefaultDevice, sortDevices } from "./lib/devices";
  import type { TimelineSelection } from "./lib/timeline";

  const connection = new Connection();
  let info = $state<ConnectionInfo>({
    state: "connecting",
    serverVersion: null,
    deviceCount: null,
    lastMessageAt: null,
    attempt: 0,
  });

  const unsubscribe = connection.info.subscribe((value) => {
    info = value;
  });

  let devices = $state<DeviceSummary[]>([]);
  let devicesError = $state<string | null>(null);
  let selectedId = $state<string | null>(null);

  const selectedDevice = $derived(devices.find((d) => d.id === selectedId) ?? null);

  /** Remount the log, write bar and panels when the device changes. They all
   * hold per-device state (a stream subscription, a scroll position, a
   * command history); keying on the id is simpler and less error-prone than
   * teaching each of them to reset itself. */
  const paneKey = $derived(selectedId ?? "none");

  function deviceFromUrl(): string | null {
    return new URLSearchParams(location.search).get("device");
  }

  async function loadDevices(): Promise<void> {
    try {
      devices = sortDevices(await fetchDevices());
      devicesError = null;
    } catch (e) {
      devicesError = e instanceof Error ? e.message : String(e);
      return;
    }
    // Only ever *fill in* a missing or stale selection. A device that
    // disconnects while you're reading it must keep its log on screen — the
    // recorded history is still there and still the point — so an id that no
    // longer appears in the list is left selected rather than snapped away.
    if (selectedId === null || devices.length === 0) {
      selectedId = deviceFromUrl() ?? pickDefaultDevice(devices);
    }
  }

  function selectDevice(id: string): void {
    if (id === selectedId) return;
    selectedId = id;
    const url = new URL(location.href);
    url.searchParams.set("device", id);
    history.pushState({ device: id }, "", url);
  }

  function onPopState(): void {
    selectedId = deviceFromUrl() ?? pickDefaultDevice(devices);
  }

  // ---- panels ----
  type DrawerName = "clients" | "audit" | "devices";
  let openDrawer = $state<DrawerName | null>(null);

  function toggleDrawer(name: DrawerName): void {
    openDrawer = openDrawer === name ? null : name;
  }

  let timelineSelection = $state<TimelineSelection | null>(null);
  let jumpToSeq: ((seq: number) => void) | null = null;
  let focusFilter: (() => void) | null = null;
  let clearView: (() => void) | null = null;

  /** True when the keystroke is going somewhere that wants it — never steal
   * `/` from someone typing a regex, or from the write bar. */
  function isTyping(target: EventTarget | null): boolean {
    const el = target as HTMLElement | null;
    if (!el) return false;
    const tag = el.tagName;
    return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || el.isContentEditable;
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      if (openDrawer !== null) {
        openDrawer = null;
        e.preventDefault();
      }
      return;
    }
    // Ctrl-L, never Cmd-L: on macOS Cmd-L is the address bar, and a web page
    // that swallows it has broken the browser, not added a shortcut.
    if (e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "l") {
      e.preventDefault();
      clearView?.();
      return;
    }
    if (isTyping(e.target) || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "/") {
      e.preventDefault();
      focusFilter?.();
    }
  }

  onMount(() => {
    connection.start();
    selectedId = deviceFromUrl();
    void loadDevices();
    // Ports come and go while the page is open. Cheap poll, and it is the
    // only thing standing between "you plugged a board in" and the picker
    // knowing about it.
    const timer = setInterval(() => void loadDevices(), 5_000);
    return () => clearInterval(timer);
  });

  onDestroy(() => {
    unsubscribe();
    connection.stop();
  });
</script>

<svelte:window onkeydown={onKeyDown} onpopstate={onPopState} />

{#snippet devicePicker()}
  <DevicePicker
    {devices}
    loadError={devicesError}
    {selectedId}
    onSelect={selectDevice}
  />
{/snippet}

<main class="shell">
  {#if selectedId}
    {#key paneKey}
      <ApprovalCardHost deviceId={selectedId} />

      <div class="stage">
        <LiveLog
          deviceId={selectedId}
          headerLead={devicePicker}
          onTimelineSelect={(selection) => (timelineSelection = selection)}
          registerJumpToSeq={(fn) => (jumpToSeq = fn)}
          registerFocusFilter={(fn) => (focusFilter = fn)}
          registerClearView={(fn) => (clearView = fn)}
        />

        <Drawer
          title="Connected clients"
          testid="clients-drawer"
          open={openDrawer === "clients"}
          onClose={() => (openDrawer = null)}
        >
          <ClientsPanel />
        </Drawer>

        <Drawer
          title="Audit trail"
          testid="audit-drawer"
          open={openDrawer === "audit"}
          onClose={() => (openDrawer = null)}
        >
          <!-- "Jump to log" is a request to go look at the log, so the
               drawer that was covering it gets out of the way. -->
          <AuditPanel
            deviceId={selectedId}
            onJumpToSeq={(seq) => {
              openDrawer = null;
              jumpToSeq?.(seq);
            }}
          />
        </Drawer>

        <Drawer
          title="All serial ports"
          testid="devices-drawer"
          open={openDrawer === "devices"}
          onClose={() => (openDrawer = null)}
        >
          <DeviceList
            {selectedId}
            onSelect={(id) => {
              selectDevice(id);
              openDrawer = null;
            }}
          />
        </Drawer>
      </div>

      <WriteBar deviceId={selectedId} connected={selectedDevice?.connected ?? false} />
    {/key}
  {:else}
    <div class="stage">
      <div class="startup">
        <h1>serialwrap</h1>
        {#if devicesError}
          <p>Can't reach the daemon — {devicesError}</p>
          <p class="hint">Start it with <code>serialwrap daemon</code>, then reload.</p>
        {:else}
          <p>No serial ports yet.</p>
          <p class="hint">
            Plug a board in. Recording starts the moment it enumerates, so the boot log will
            already be here when this page catches up.
          </p>
        {/if}
      </div>
    </div>
  {/if}

  <footer class="status-bar">
    <button
      type="button"
      class:on={openDrawer === "clients"}
      data-testid="open-clients"
      onclick={() => toggleDrawer("clients")}>Clients</button
    >
    <button
      type="button"
      class:on={openDrawer === "audit"}
      data-testid="open-audit"
      onclick={() => toggleDrawer("audit")}
      disabled={!selectedId}>Audit</button
    >
    <button
      type="button"
      class:on={openDrawer === "devices"}
      data-testid="open-devices"
      onclick={() => toggleDrawer("devices")}>Ports ({devices.length})</button
    >
    {#if selectedId}
      <ExportDialog deviceId={selectedId} {timelineSelection} />
    {/if}

    <span class="spacer"></span>
    <span class="shortcut-hint" aria-hidden="true">/ filter · Ctrl-L clear · Esc close</span>
    <ConnectionStatus {info} />
  </footer>
</main>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    /* `dvh` so the mobile URL bar collapsing doesn't leave the status bar
     * stranded off-screen; the fallback below covers browsers without it. */
    max-height: 100dvh;
  }

  @supports not (height: 100dvh) {
    .shell {
      height: 100vh;
      max-height: 100vh;
    }
  }

  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    /* Backstop for `LiveLog`'s viewport `min-height`: if the window is short
     * enough that the log's floor plus its chrome exceeds the space left
     * over, the excess is clipped here rather than pushing the write bar and
     * status bar off the bottom of the screen. */
    overflow: hidden;
  }

  .startup {
    margin: auto;
    padding: var(--space-6);
    max-width: 32rem;
    text-align: center;
  }

  .startup h1 {
    margin: 0 0 var(--space-4);
    font-family: var(--font-mono);
    font-size: 1.25rem;
    letter-spacing: -0.01em;
  }

  .startup p {
    margin: 0 0 var(--space-2);
    color: var(--text-dim);
    line-height: 1.6;
  }

  .startup .hint {
    color: var(--text-faint);
    font-size: var(--text-sm);
  }

  .startup code {
    font-family: var(--font-mono);
    background: var(--surface-raised);
    padding: 0 var(--space-1);
    border-radius: var(--radius-sm);
  }

  .status-bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-3);
    border-top: 1px solid var(--border);
    background: var(--surface);
    flex: none;
    flex-wrap: wrap;
  }

  .status-bar button {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text-dim);
    cursor: pointer;
  }

  .status-bar button:hover:not(:disabled) {
    color: var(--text);
    background: var(--surface-raised);
  }

  .status-bar button.on {
    color: var(--rx);
    border-color: var(--border);
    background: var(--rx-bg);
  }

  .status-bar button:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .spacer {
    flex: 1;
  }

  .shortcut-hint {
    font-size: var(--text-xs);
    color: var(--text-faint);
    font-family: var(--font-mono);
  }

  /* Narrow: the keyboard hint is the first thing to go — it documents keys
   * a touch device doesn't have. */
  @media (max-width: 52rem) {
    .shortcut-hint {
      display: none;
    }
  }
</style>
