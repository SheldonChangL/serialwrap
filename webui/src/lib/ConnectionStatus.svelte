<script lang="ts">
  import type { ConnectionInfo } from "./connection";

  interface Props {
    info: ConnectionInfo;
  }

  let { info }: Props = $props();

  const LABEL: Record<ConnectionInfo["state"], string> = {
    connecting: "Connecting…",
    open: "Connected",
    stale: "No heartbeat — reconnecting",
    closed: "Disconnected — reconnecting",
  };

  let label = $derived(LABEL[info.state]);
  // "connecting"/"open" both status-quo-fine states get no alert role;
  // "stale"/"closed" are the two the acceptance criterion means by "明確
  // disconnect indicator, not silently pretending" — screen readers should
  // hear about those without the page needing focus.
  let isDisconnected = $derived(info.state === "stale" || info.state === "closed");
</script>

<div
  class="status-pill"
  class:connecting={info.state === "connecting"}
  class:open={info.state === "open"}
  class:stale={info.state === "stale"}
  class:closed={info.state === "closed"}
  data-testid="connection-status"
  data-state={info.state}
  role={isDisconnected ? "alert" : "status"}
>
  <span class="dot" aria-hidden="true"></span>
  <span class="label">{label}</span>
  {#if info.serverVersion}
    <span class="meta">serialwrapd v{info.serverVersion}</span>
  {/if}
</div>

<style>
  .status-pill {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.35rem 0.75rem;
    border-radius: 999px;
    font-size: 0.875rem;
    border: 1px solid var(--border);
    background: var(--surface);
  }

  .dot {
    width: 0.6rem;
    height: 0.6rem;
    border-radius: 50%;
    background: var(--dot-connecting);
    flex-shrink: 0;
  }

  .status-pill.open .dot {
    background: var(--dot-open);
  }

  .status-pill.stale .dot {
    background: var(--dot-stale);
    animation: pulse 1.2s ease-in-out infinite;
  }

  .status-pill.closed .dot {
    background: var(--dot-closed);
    animation: pulse 1.2s ease-in-out infinite;
  }

  .label {
    font-weight: 600;
  }

  .meta {
    color: var(--text-dim);
    font-size: 0.8rem;
  }

  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
    }
  }
</style>
