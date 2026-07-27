<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { Connection, type ConnectionInfo } from "./lib/connection";
  import ConnectionStatus from "./lib/ConnectionStatus.svelte";
  import DeviceList from "./lib/DeviceList.svelte";
  import LiveLog from "./lib/LiveLog.svelte";
  import ApprovalCardHost from "./lib/ApprovalCardHost.svelte";
  import ClientsPanel from "./lib/ClientsPanel.svelte";
  import AuditPanel from "./lib/AuditPanel.svelte";
  import ExportDialog from "./lib/ExportDialog.svelte";
  import { fetchDevices } from "./lib/api";
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

  // One device per browser tab (see the UX-design wiki's "deliberate
  // omissions" section — no multi-device tabs in v1): the live log view
  // below simply follows whichever device `GET /api/devices` lists first.
  let primaryDeviceId = $state<string | null>(null);

  // ---- T5.5 (issue #22): clients / audit / export plumbing ----
  // `timelineSelection` is a second copy of `LiveLog.svelte`'s own
  // internal state, kept in sync via its `onTimelineSelect` callback prop
  // (see that component's doc comment) so `ExportDialog` — a sibling, not
  // a child, of `LiveLog` — can offer the current drag-selection as an
  // export source without `LiveLog` needing to know `ExportDialog` exists.
  let timelineSelection = $state<TimelineSelection | null>(null);
  // An imperative handle into `LiveLog`'s own `jumpToSeq`, captured once
  // via `registerJumpToSeq` — see that prop's doc comment on why this is a
  // captured function reference rather than a reactive prop.
  let jumpToSeq: ((seq: number) => void) | null = null;

  async function loadPrimaryDevice(): Promise<void> {
    try {
      const devices = await fetchDevices();
      primaryDeviceId = devices[0]?.id ?? null;
    } catch {
      primaryDeviceId = null;
    }
  }

  onMount(() => {
    connection.start();
    void loadPrimaryDevice();
  });
  onDestroy(() => {
    unsubscribe();
    connection.stop();
  });
</script>

<main>
  <header>
    <h1>serialwrap</h1>
    <ConnectionStatus {info} />
  </header>

  <p class="tagline">
    Web GUI foundation (T5.1), the live log view (T5.2), the timeline and
    port settings popover (T5.3), the approval card (T5.4), and the
    clients/audit/export panels (T5.5).
  </p>

  {#if primaryDeviceId}
    <ApprovalCardHost deviceId={primaryDeviceId} />
    <LiveLog
      deviceId={primaryDeviceId}
      onTimelineSelect={(selection) => (timelineSelection = selection)}
      registerJumpToSeq={(fn) => (jumpToSeq = fn)}
    />
    <ClientsPanel />
    <AuditPanel deviceId={primaryDeviceId} onJumpToSeq={(seq) => jumpToSeq?.(seq)} />
    <ExportDialog deviceId={primaryDeviceId} {timelineSelection} />
  {/if}

  <DeviceList />
</main>

<style>
  main {
    max-width: 60rem;
    margin: 0 auto;
    padding: 2rem 1.25rem 3rem;
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    flex-wrap: wrap;
  }

  h1 {
    margin: 0;
    font-size: 1.5rem;
    font-family: var(--font-mono);
  }

  .tagline {
    color: var(--text-dim);
    font-size: 0.9rem;
    margin: 0;
  }
</style>
