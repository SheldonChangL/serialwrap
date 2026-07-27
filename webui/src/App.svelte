<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { Connection, type ConnectionInfo } from "./lib/connection";
  import ConnectionStatus from "./lib/ConnectionStatus.svelte";
  import DeviceList from "./lib/DeviceList.svelte";

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

  onMount(() => connection.start());
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
    Web GUI foundation (T5.1) — WebSocket connectivity and one live API call.
    The log view, timeline, approval cards, and clients/audit/export panels
    land in later tasks.
  </p>

  <DeviceList />
</main>

<style>
  main {
    max-width: 40rem;
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
