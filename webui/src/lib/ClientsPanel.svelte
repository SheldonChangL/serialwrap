<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    clientTypeIcon,
    demoteClient,
    fetchClients,
    formatBytes,
    kickClient,
    nextDemotion,
    permissionBadge,
    type ClientRow,
    type Permission,
  } from "./clients";

  /** Coarse poll interval — same reasoning/value as
   * `ApprovalCardHost.svelte`'s `POLL_INTERVAL_MS`: a `wait_for` countdown's
   * `remaining_s` is computed fresh server-side on every request, so a
   * poll (rather than a client-side ticking clock) is enough to keep it
   * visibly counting down without drifting from the daemon's own notion of
   * "how much time is left". */
  const POLL_INTERVAL_MS = 2_000;

  type LoadState = { kind: "loading" } | { kind: "error"; message: string } | { kind: "loaded"; rows: ClientRow[] };

  let panelState = $state<LoadState>({ kind: "loading" });
  let pending = $state<Set<string>>(new Set());
  let pollTimer: ReturnType<typeof setInterval> | undefined;

  function rowKey(row: ClientRow): string {
    return row.status === "active" ? `active-${row.client_id}` : `lease-${row.device}-${row.ended_seq}`;
  }

  async function load(): Promise<void> {
    try {
      const rows = await fetchClients();
      panelState = { kind: "loaded", rows };
    } catch (e) {
      panelState = { kind: "error", message: e instanceof Error ? e.message : String(e) };
    }
  }

  async function withPending(key: string, action: () => Promise<unknown>): Promise<void> {
    const next = new Set(pending);
    next.add(key);
    pending = next;
    try {
      await action();
      await load();
    } finally {
      const after = new Set(pending);
      after.delete(key);
      pending = after;
    }
  }

  function onKick(clientId: number): void {
    void withPending(`kick-${clientId}`, () => kickClient(clientId));
  }

  function onDemote(clientId: number, current: Permission): void {
    const target = nextDemotion(current);
    if (!target) return;
    void withPending(`demote-${clientId}`, () => demoteClient(clientId, target));
  }

  function remainingLabel(remainingS: number): string {
    return `${Math.max(0, Math.ceil(remainingS))}s left`;
  }

  function durationLabel(durationMs: number | null): string {
    if (durationMs === null) return "";
    return `${(durationMs / 1000).toFixed(0)}s`;
  }

  onMount(() => {
    void load();
    pollTimer = setInterval(() => void load(), POLL_INTERVAL_MS);
  });
  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });
</script>

<section class="clients" data-testid="clients-panel" data-state={panelState.kind}>
  <div class="header">
    <h2>Connected clients{panelState.kind === "loaded" ? ` (${panelState.rows.length})` : ""}</h2>
    <button type="button" onclick={() => void load()}>Refresh</button>
  </div>

  {#if panelState.kind === "loading"}
    <p class="hint">Loading…</p>
  {:else if panelState.kind === "error"}
    <p class="hint error">GET /api/clients failed: {panelState.message}</p>
  {:else if panelState.rows.length === 0}
    <p class="hint">No clients connected.</p>
  {:else}
    <ul>
      {#each panelState.rows as row (rowKey(row))}
        <li
          class="row"
          class:offline={row.status === "offline"}
          data-testid="client-row"
          data-status={row.status}
          data-client-id={row.status === "active" ? row.client_id : undefined}
          data-pid={row.pid ?? ""}
        >
          <div class="identity">
            <span class="icon">{clientTypeIcon(row.type)}</span>
            <span class="name">{row.name}</span>
            <span class="sep">&middot;</span>
            <span class="kind-label">{row.status === "active" ? "GUI/MCP" : "lease"}</span>
            <span class="sep">&middot;</span>
            <span class="pid">pid {row.pid ?? "?"}</span>
            {#if row.status === "active"}
              <span class="badge type-badge">{row.type}</span>
              <span class="badge permission-badge">{permissionBadge(row.permission)}</span>
            {:else}
              <span class="badge type-badge">{row.type}</span>
              <span class="badge offline-badge">offline</span>
            {/if}
          </div>

          {#if row.status === "active"}
            <div class="detail">
              <span class="traffic">{formatBytes(row.bytes_in)} sent &middot; {formatBytes(row.bytes_out)} received</span>
              {#if row.activity.state === "waiting_for"}
                <span class="waiting" data-testid="client-waiting" data-pattern={row.activity.pattern} data-remaining-s={row.activity.remaining_s}>
                  waiting: wait_for "{row.activity.pattern}" &mdash; {remainingLabel(row.activity.remaining_s)}
                </span>
              {:else}
                <span class="idle-label">active</span>
              {/if}
            </div>
            <div class="actions">
              <button
                type="button"
                data-testid="demote-button"
                disabled={nextDemotion(row.permission) === null || pending.has(`demote-${row.client_id}`)}
                onclick={() => onDemote(row.client_id, row.permission)}
              >
                Demote
              </button>
              <button
                type="button"
                data-testid="kick-button"
                disabled={pending.has(`kick-${row.client_id}`)}
                onclick={() => onKick(row.client_id)}
              >
                Kick
              </button>
            </div>
          {:else}
            <div class="detail">
              <span class="lease-detail" data-testid="finished-lease-detail">
                held the port exclusively for {durationLabel(row.duration_ms)} ({row.command})
                &middot; ended {new Date(row.ended_at).toLocaleTimeString()}
              </span>
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .clients {
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
    padding: 0.25rem 0.6rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.6;
    cursor: default;
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
    gap: 0.5rem;
  }

  .row {
    border: 1px solid var(--border);
    border-radius: 0.4rem;
    padding: 0.5rem 0.7rem;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }

  .row.offline {
    opacity: 0.75;
  }

  .identity {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-family: var(--font-mono);
    font-size: 0.875rem;
    flex-wrap: wrap;
  }

  .sep {
    color: var(--text-dim);
  }

  .kind-label,
  .pid {
    color: var(--text-dim);
  }

  .badge {
    margin-left: auto;
    font-size: 0.7rem;
    padding: 0.05rem 0.4rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: var(--text-dim);
  }

  .badge + .badge {
    margin-left: 0.3rem;
  }

  .offline-badge {
    color: var(--dot-closed);
  }

  .detail {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.8125rem;
    color: var(--text-dim);
    flex-wrap: wrap;
  }

  .waiting {
    color: var(--dot-stale);
  }

  .actions {
    display: flex;
    gap: 0.4rem;
    justify-content: flex-end;
  }

  .lease-detail {
    font-size: 0.8125rem;
  }
</style>
