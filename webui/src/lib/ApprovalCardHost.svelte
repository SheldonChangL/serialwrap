<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { fetchApprovals, watchDeviceActivity, type ApprovalSnapshot } from "./approvals";
  import ApprovalCard from "./ApprovalCard.svelte";

  interface Props {
    deviceId: string;
  }
  const { deviceId }: Props = $props();

  let approvals = $state<ApprovalSnapshot[]>([]);
  /** ids this host has noticed are no longer in the server's pending list
   * (someone else — the CLI, another open tab, or the fail-safe timeout —
   * decided them) but is still showing, briefly, in a "resolved" state
   * rather than yanking the card out of the DOM the instant it vanishes
   * from a poll response. Without this grace period, a request the CLI
   * decides *just* before this tab's own countdown reaches zero would make
   * the card disappear with no visible trace at all — the opposite of
   * "後到者要看到已決狀態" (T5.4, issue #21): the point is that a later
   * party *sees* the resolved state, not that it silently vanishes. */
  let resolvedElsewhere = $state<Set<number>>(new Set());

  const REMOVE_GRACE_MS = 3_000;

  /** Coarse fallback poll — catches a decision made elsewhere (CLI, another
   * GUI tab) that this tab's `watchDeviceActivity` ping might have missed
   * (e.g. a reconnect gap), and catches a pending write that existed before
   * this host ever mounted (a page load while something was already
   * pending). `watchDeviceActivity` below is what makes new pending writes
   * appear promptly (T5.4 acceptance criterion: "卡片 3 秒內出現") — this
   * poll is a safety net, not the primary mechanism. */
  const POLL_INTERVAL_MS = 2_000;
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let stopWatch: (() => void) | undefined;
  let removalTimers = new Map<number, ReturnType<typeof setTimeout>>();

  function scheduleRemoval(id: number): void {
    if (removalTimers.has(id)) return;
    removalTimers.set(
      id,
      setTimeout(() => {
        removalTimers.delete(id);
        approvals = approvals.filter((a) => a.id !== id);
        const next = new Set(resolvedElsewhere);
        next.delete(id);
        resolvedElsewhere = next;
      }, REMOVE_GRACE_MS),
    );
  }

  async function refresh(): Promise<void> {
    let fresh: ApprovalSnapshot[];
    try {
      fresh = await fetchApprovals();
    } catch {
      // Best-effort: a transient fetch failure leaves the existing list
      // (and any already-scheduled removals) as-is rather than clearing
      // visible cards on a network hiccup.
      return;
    }
    const freshIds = new Set(fresh.map((a) => a.id));
    const newlyResolved = approvals.filter((a) => !freshIds.has(a.id) && !resolvedElsewhere.has(a.id));
    if (newlyResolved.length > 0) {
      const next = new Set(resolvedElsewhere);
      for (const a of newlyResolved) {
        next.add(a.id);
        scheduleRemoval(a.id);
      }
      resolvedElsewhere = next;
    }
    // Update still-pending cards with fresh data (countdown/session-count
    // fields can change); keep already-resolved-but-not-yet-evicted ones
    // exactly as they last looked (the server has nothing left to tell us
    // about an id it no longer lists).
    const stillTracked = approvals.filter((a) => !freshIds.has(a.id));
    approvals = [...fresh, ...stillTracked];
  }

  function handleDecided(id: number): void {
    // This card resolved its own request — no grace period needed, it
    // already showed its own outcome before this callback fired (see
    // `ApprovalCard.svelte`'s deny/approve handlers).
    if (removalTimers.has(id)) {
      clearTimeout(removalTimers.get(id));
      removalTimers.delete(id);
    }
    approvals = approvals.filter((a) => a.id !== id);
    const next = new Set(resolvedElsewhere);
    next.delete(id);
    resolvedElsewhere = next;
    void refresh();
  }

  onMount(() => {
    void refresh();
    pollTimer = setInterval(() => void refresh(), POLL_INTERVAL_MS);
    stopWatch = watchDeviceActivity(deviceId, () => void refresh());
  });
  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
    stopWatch?.();
    for (const timer of removalTimers.values()) clearTimeout(timer);
  });
</script>

{#if approvals.length > 0}
  <div class="approval-stack" data-testid="approval-stack">
    {#each approvals as approval (approval.id)}
      <ApprovalCard {approval} resolvedElsewhere={resolvedElsewhere.has(approval.id)} onDecided={handleDecided} />
    {/each}
  </div>
{/if}

<style>
  .approval-stack {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
</style>
