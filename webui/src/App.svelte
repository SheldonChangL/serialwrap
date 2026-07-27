<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { Connection, type ConnectionInfo } from "./lib/connection";
  import ConnectionStatus from "./lib/ConnectionStatus.svelte";
  import DeviceList from "./lib/DeviceList.svelte";
  import LiveLog from "./lib/LiveLog.svelte";
  import ApprovalCardHost from "./lib/ApprovalCardHost.svelte";
  import { fetchDevices } from "./lib/api";

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
    port settings popover (T5.3), and the approval card (T5.4). Clients,
    audit, and export panels land in T5.5.
  </p>

  {#if primaryDeviceId}
    <ApprovalCardHost deviceId={primaryDeviceId} />
    <LiveLog deviceId={primaryDeviceId} />
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
