<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { approveApproval, denyApproval, type ApprovalSnapshot } from "./approvals";

  interface Props {
    approval: ApprovalSnapshot;
    /** `true` once the host (`ApprovalCardHost.svelte`) has noticed this
     * request no longer appears in `GET /api/approvals` — decided by the
     * CLI, another open GUI tab, or the daemon's own fail-safe timeout,
     * none of which this card's own click handlers ever ran for. Shown as
     * a settled state (no buttons) exactly like a local timeout, just
     * without knowing this card's own countdown was the *reason* — see
     * `settled`/`resolutionLabel` below. */
    resolvedElsewhere: boolean;
    /** Called once this card is fully resolved — by its own Deny/Allow
     * click, or by losing a decide race to someone else (CLI or another
     * GUI tab). The host component removes the card from its list; this
     * component never removes itself from the DOM (a local timeout is
     * shown here, but its *removal* is still the host's call, on the same
     * grace-period schedule as any other externally-resolved request). */
    onDecided: (id: number) => void;
  }
  const { approval, resolvedElsewhere, onDecided }: Props = $props();

  let denyButtonEl: HTMLButtonElement | undefined = $state();
  let whitelist = $state(false);
  let busy = $state(false);
  let conflictMessage = $state<string | null>(null);

  // Local countdown: `approval.age_s`/`timeout_s` are a snapshot from
  // whenever this card's data was fetched, not a live value the server
  // pushes every tick — `mountedAtMs` anchors "how much additional time has
  // passed since that snapshot" against the browser's own clock, ticked by
  // a plain `setInterval` redraw (a UI animation, not a test-synchronization
  // primitive — the actual fail-safe timeout is the daemon's own spawned
  // task in `crates/serialwrapd/src/gate.rs`; this is only a visual mirror
  // of it).
  const mountedAtMs = Date.now();
  let nowMs = $state(Date.now());
  let tickTimer: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    tickTimer = setInterval(() => {
      nowMs = Date.now();
    }, 200);
    // Deliberately not the "Allow once" button (T5.4 acceptance criterion
    // 10, and the UX-design wiki: "No default focus on Allow once —
    // keyboard-dismissing a dialog must not be able to approve a
    // destructive write"). Focusing Deny instead means a keyboard Enter
    // on an unfocused card denies, never approves.
    denyButtonEl?.focus();
  });
  onDestroy(() => {
    if (tickTimer) clearInterval(tickTimer);
  });

  const elapsedS = $derived(approval.age_s + (nowMs - mountedAtMs) / 1000);
  const remainingS = $derived(Math.max(0, approval.timeout_s - elapsedS));
  const timedOut = $derived(remainingS <= 0);
  const progressPct = $derived(
    approval.timeout_s > 0 ? Math.max(0, Math.min(100, (remainingS / approval.timeout_s) * 100)) : 0,
  );
  /** Either this card's own countdown ran out, or the host learned the
   * request was decided some other way — either case means no more
   * buttons and a settled-state banner (T5.4 acceptance criterion 8: "不
   * 殘留可點按鈕"). */
  const settled = $derived(timedOut || resolvedElsewhere);
  const resolutionLabel = $derived(
    timedOut ? "已逾時拒絕 — timed out, denied" : "already decided elsewhere",
  );

  const isDanger = $derived(approval.danger_reason !== null);

  function ordinal(n: number): string {
    const mod100 = n % 100;
    if (mod100 >= 11 && mod100 <= 13) return `${n}th`;
    switch (n % 10) {
      case 1:
        return `${n}st`;
      case 2:
        return `${n}nd`;
      case 3:
        return `${n}rd`;
      default:
        return `${n}th`;
    }
  }

  async function handleDeny(): Promise<void> {
    if (busy || settled) return;
    busy = true;
    try {
      const outcome = await denyApproval(approval.id);
      if (outcome === "conflict") conflictMessage = "already decided elsewhere";
      onDecided(approval.id);
    } catch (e) {
      busy = false;
      conflictMessage = e instanceof Error ? e.message : String(e);
    }
  }

  async function handleAllowOnce(): Promise<void> {
    if (busy || settled) return;
    busy = true;
    try {
      const outcome = await approveApproval(approval.id);
      if (outcome === "conflict") conflictMessage = "already decided elsewhere";
      onDecided(approval.id);
    } catch (e) {
      busy = false;
      conflictMessage = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="approval-card" data-testid="approval-card" data-approval-id={approval.id} data-settled={settled}>
  <div class="header">
    <span class="lock" aria-hidden="true">&#128274;</span>
    <span class="title">Write approval</span>
    {#if !settled}
      <span class="countdown-label" data-testid="approval-countdown">{Math.ceil(remainingS)}s until denial</span>
    {:else}
      <span class="countdown-label timed-out" data-testid="approval-countdown">resolved</span>
    {/if}
  </div>

  <div class="progress-track" aria-hidden="true">
    <div class="progress-fill" style="width: {progressPct}%;"></div>
  </div>

  <div class="requester" data-testid="approval-requester">
    <strong>{approval.requester_name}</strong>
    ({approval.requester_type} &middot; pid {approval.requester_pid}) on {approval.device}
    <br />
    {approval.requester_type === "human" ? "write" : "read + gated write"} &middot;
    {ordinal(approval.session_request_no)} write request this session
  </div>

  <div class="bytes-box">
    <div class="bytes-text" data-testid="approval-bytes-text">{approval.bytes_text}</div>
    <div class="bytes-hex" data-testid="approval-bytes-hex">{approval.bytes_hex}</div>
  </div>

  {#if approval.matched_rule}
    <div class="matched-rule" data-testid="approval-matched-rule" class:danger={isDanger}>
      &#9888; matched rule <code>{approval.matched_rule}</code>
      {#if approval.danger_reason}
        &mdash; {approval.danger_reason}
      {/if}
    </div>
  {/if}

  {#if approval.log_context.length > 0}
    <div class="log-context" data-testid="approval-log-context">
      <div class="log-context-title">Log context before this request:</div>
      {#each approval.log_context.slice(-8) as line, i (i)}
        <div class="log-context-line">{line}</div>
      {/each}
    </div>
  {/if}

  {#if conflictMessage}
    <div class="conflict" data-testid="approval-conflict">{conflictMessage}</div>
  {/if}

  {#if !settled}
    <div class="actions">
      <button
        type="button"
        bind:this={denyButtonEl}
        data-testid="approval-deny"
        class="deny"
        disabled={busy}
        onclick={handleDeny}
      >
        Deny
      </button>
      <button
        type="button"
        data-testid="approval-allow-once"
        class="allow"
        disabled={busy}
        onclick={handleAllowOnce}
      >
        Allow once
      </button>
    </div>
    <label class="whitelist-toggle">
      <input
        type="checkbox"
        data-testid="approval-whitelist-checkbox"
        bind:checked={whitelist}
        disabled={isDanger}
      />
      Whitelist this pattern
      {#if isDanger}
        <span class="na">(n/a for danger patterns — edit rules.toml instead)</span>
      {/if}
    </label>
  {:else}
    <div class="timed-out-banner" data-testid="approval-timed-out-banner">{resolutionLabel}</div>
  {/if}

  <div class="footnote">On denial or timeout, the requester receives a structured reason — no silent failure.</div>
</div>

<style>
  .approval-card {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    border: 1px solid var(--dot-closed);
    border-radius: 0.5rem;
    background: var(--surface);
    padding: 0.85rem 1rem;
    font-size: 0.85rem;
  }

  .header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .title {
    font-weight: 700;
  }

  .countdown-label {
    margin-left: auto;
    color: var(--text-dim);
    font-variant-numeric: tabular-nums;
  }
  .countdown-label.timed-out {
    color: var(--dot-closed);
    font-weight: 600;
  }

  .progress-track {
    height: 4px;
    border-radius: 999px;
    background: var(--surface-raised);
    overflow: hidden;
  }
  .progress-fill {
    height: 100%;
    background: var(--dot-closed);
    transition: width 0.2s linear;
  }

  .requester {
    color: var(--text-dim);
    line-height: 1.4;
  }

  .bytes-box {
    border: 1px solid var(--border);
    border-radius: 0.35rem;
    padding: 0.4rem 0.6rem;
    background: var(--surface-raised);
    font-family: var(--font-mono);
    font-size: 0.8rem;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    overflow-x: auto;
  }
  .bytes-hex {
    color: var(--text-dim);
    letter-spacing: 0.02em;
  }

  .matched-rule {
    color: var(--text-dim);
  }
  .matched-rule.danger {
    color: var(--dot-closed);
    font-weight: 600;
  }

  .log-context {
    border-left: 3px solid var(--border);
    padding-left: 0.6rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--text-dim);
    max-height: 8rem;
    overflow-y: auto;
  }
  .log-context-title {
    font-family: var(--font-sans);
    margin-bottom: 0.2rem;
  }

  .conflict {
    color: var(--dot-stale);
  }

  .actions {
    display: flex;
    gap: 0.6rem;
  }
  .actions button {
    font: inherit;
    padding: 0.35rem 0.9rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }
  .actions button.deny {
    border-color: var(--dot-closed);
  }
  .actions button:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .whitelist-toggle {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--text-dim);
  }
  .na {
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .timed-out-banner {
    color: var(--dot-closed);
    font-weight: 600;
  }

  .footnote {
    color: var(--text-dim);
    font-size: 0.7rem;
  }
</style>
