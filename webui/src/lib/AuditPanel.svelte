<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    byteFieldValue,
    decodeBytesField,
    extraFields,
    fetchAudit,
    rowMatchesFilter,
    statusTag,
    summarizeRow,
    type AuditRow,
  } from "./audit";
  import { watchDeviceActivity } from "./approvals";

  interface Props {
    deviceId: string;
    /** Forwarded up to `App.svelte`, which routes it into `LiveLog.svelte`'s
     * imperative `jumpToSeq` (T5.5's "跳到當時的 log" — the row's own `seq`
     * is already a real position in the same stream the main log view
     * renders, so no correlation/lookup is needed here at all). */
    onJumpToSeq?: (seq: number) => void;
  }
  const { deviceId, onJumpToSeq }: Props = $props();

  /** Coarse safety-net poll, same value/reasoning as
   * `ApprovalCardHost.svelte`'s `POLL_INTERVAL_MS`. */
  const POLL_INTERVAL_MS = 2_000;

  type LoadState = { kind: "loading" } | { kind: "error"; message: string } | { kind: "loaded"; rows: AuditRow[] };

  let panelState = $state<LoadState>({ kind: "loading" });
  let filterText = $state("");
  let expandedSeqs = $state<Set<number>>(new Set());
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let stopWatch: (() => void) | undefined;

  async function load(): Promise<void> {
    try {
      const rows = await fetchAudit(deviceId);
      panelState = { kind: "loaded", rows };
    } catch (e) {
      panelState = { kind: "error", message: e instanceof Error ? e.message : String(e) };
    }
  }

  function toggleExpand(seq: number): void {
    const next = new Set(expandedSeqs);
    if (next.has(seq)) next.delete(seq);
    else next.add(seq);
    expandedSeqs = next;
  }

  const visibleRows = $derived.by((): AuditRow[] => {
    if (panelState.kind !== "loaded") return [];
    return panelState.rows.filter((r) => rowMatchesFilter(r, filterText));
  });

  const totalTodayLabel = $derived.by((): number => (panelState.kind === "loaded" ? panelState.rows.length : 0));

  function statusClass(tag: string): string {
    const lower = tag.toLowerCase();
    if (lower === "deny" || lower.startsWith("timeout")) return "status-denied";
    if (lower.startsWith("approve")) return "status-approved";
    if (lower.startsWith("whitelist")) return "status-whitelisted";
    return "status-plain";
  }

  onMount(() => {
    void load();
    pollTimer = setInterval(() => void load(), POLL_INTERVAL_MS);
    stopWatch = watchDeviceActivity(deviceId, () => void load());
  });
  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
    stopWatch?.();
  });
</script>

<section class="audit" data-testid="audit-panel" data-state={panelState.kind}>
  <div class="header">
    <h2>Audit ({totalTodayLabel})</h2>
    <input
      type="text"
      class="filter"
      placeholder="filter (actor, reason, bytes…)"
      data-testid="audit-filter"
      bind:value={filterText}
    />
    <button type="button" onclick={() => void load()}>Refresh</button>
  </div>

  {#if panelState.kind === "loading"}
    <p class="hint">Loading…</p>
  {:else if panelState.kind === "error"}
    <p class="hint error">GET /api/devices/{deviceId}/audit failed: {panelState.message}</p>
  {:else if visibleRows.length === 0}
    <p class="hint">No audit records match.</p>
  {:else}
    <ul>
      {#each visibleRows as row (row.seq)}
        {@const tag = statusTag(row)}
        {@const bytes = decodeBytesField(byteFieldValue(row))}
        {@const extra = extraFields(row)}
        {@const expanded = expandedSeqs.has(row.seq)}
        <li class="row" data-testid="audit-row" data-seq={row.seq} data-kind={row.kind}>
          <button type="button" class="row-summary" onclick={() => toggleExpand(row.seq)} data-testid="audit-row-toggle">
            <span class="time">{new Date(row.t_wall).toLocaleTimeString()}</span>
            <span class="summary">{summarizeRow(row)}</span>
            {#if tag}
              <span class="tag {statusClass(tag)}">{tag}</span>
            {/if}
          </button>
          {#if expanded}
            <div class="expanded" data-testid="audit-row-expanded">
              {#if bytes}
                <div class="bytes">
                  <div class="bytes-text"><span class="label">text</span> {bytes.text}</div>
                  <div class="bytes-hex"><span class="label">hex</span> {bytes.hex}</div>
                </div>
              {/if}
              {#if extra.length > 0}
                <dl class="fields">
                  {#each extra as [key, value] (key)}
                    <dt>{key}</dt>
                    <dd>{value === null || value === undefined ? "—" : JSON.stringify(value)}</dd>
                  {/each}
                </dl>
              {/if}
              <button
                type="button"
                class="jump-link"
                data-testid="audit-jump-to-log"
                data-seq={row.seq}
                onclick={() => onJumpToSeq?.(row.seq)}
              >
                &rarr; Jump to the log at seq {row.seq.toLocaleString()}
              </button>
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .audit {
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    padding: 1rem;
    background: var(--surface);
  }

  .header {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }

  h2 {
    margin: 0;
    font-size: 1rem;
    white-space: nowrap;
  }

  .filter {
    flex: 1;
    font: inherit;
    font-family: var(--font-mono);
    padding: 0.25rem 0.5rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
  }

  button {
    font: inherit;
    cursor: pointer;
  }

  .header > button {
    padding: 0.25rem 0.6rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
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
    gap: 0.3rem;
  }

  .row {
    border: 1px solid var(--border);
    border-radius: 0.35rem;
  }

  .row-summary {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.35rem 0.6rem;
    background: transparent;
    border: none;
    color: inherit;
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    text-align: left;
  }

  .time {
    color: var(--text-dim);
  }

  .summary {
    flex: 1;
  }

  .tag {
    font-size: 0.7rem;
    padding: 0.05rem 0.4rem;
    border-radius: 0.3rem;
    border: 1px solid var(--border);
  }

  .status-denied {
    color: var(--dot-closed);
    border-color: var(--dot-closed);
  }

  .status-approved {
    color: var(--dot-open);
    border-color: var(--dot-open);
  }

  .status-whitelisted {
    color: var(--text-dim);
  }

  .expanded {
    padding: 0.5rem 0.6rem 0.6rem;
    border-top: 1px solid var(--border);
    font-size: 0.8125rem;
  }

  .bytes {
    font-family: var(--font-mono);
    margin-bottom: 0.4rem;
    overflow-wrap: anywhere;
  }

  .label {
    color: var(--text-dim);
    margin-right: 0.4rem;
  }

  .fields {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.15rem 0.6rem;
    margin: 0 0 0.5rem;
  }

  .fields dt {
    color: var(--text-dim);
    font-family: var(--font-mono);
  }

  .fields dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .jump-link {
    background: transparent;
    border: none;
    color: var(--text);
    text-decoration: underline;
    padding: 0;
  }
</style>
