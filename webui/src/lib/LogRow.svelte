<script lang="ts">
  /**
   * One row of the log.
   *
   * # The gutter is the index
   *
   * Every row carries a 3px left band whose color says what kind of row it
   * is, and device output — the overwhelming majority — is the one kind that
   * leaves it blank. Scanning down the left edge of a full screen therefore
   * shows the *shape* of a session (a burst of output, an amber write, a red
   * gate decision, a warn-colored config change) without reading a word of
   * it. That is the job the original all-blue `border-left` on events was
   * half-doing; making it a complete system costs nothing and turns the edge
   * of the log into an index.
   *
   * # Rows never wrap, and that is now honest
   *
   * Row height is fixed (`LiveLog`'s `ROW_HEIGHT`) because fixed-size virtual
   * scrolling is what keeps 100k lines cheap. Previously each row was its own
   * `overflow-x: auto` container with the scrollbar hidden, so a long line was
   * silently cut off with no indication and no way to reach the rest of it.
   * Now the whole viewport scrolls horizontally as one surface (see
   * `LiveLog`'s `.spacer` width): every row shifts together under a single
   * visible scrollbar, exactly like a terminal's own pager.
   */
  import type { LogItem } from "./liveLog";
  import { describeEvent, type EventTone } from "./eventText";
  import { setDeviceConfig } from "./logStream";

  interface Props {
    item: LogItem;
    timestamp: string;
    expanded: boolean;
    onToggleExpand: (id: number) => void;
    /** Needed only for the `config_change` row's revert button (T5.3, issue
     * #20) — every other row kind ignores this. */
    deviceId?: string;
    /** Flashes a highlight after a timeline jump lands on this row (T5.3
     * acceptance criterion 1) — see `LiveLog.svelte`'s `jumpToSeq`. */
    highlighted?: boolean;
    onReverted?: () => void;
    /** Set when the filter box is in "highlight" rather than "narrow" mode:
     * matches are marked in place instead of non-matching rows being
     * removed, so a match keeps the lines around it. `null` in narrow mode
     * (the rows that survive are all matches, so marking them is noise). */
    highlightRe?: RegExp | null;
  }

  const {
    item,
    timestamp,
    expanded,
    onToggleExpand,
    deviceId,
    highlighted = false,
    onReverted,
    highlightRe = null,
  }: Props = $props();

  function fmtCount(n: number): string {
    return n.toLocaleString();
  }

  /** Split `text` into alternating plain/matched segments for the highlight
   * mode. A global-flag clone is used so the caller's regex `lastIndex` is
   * never mutated across rows, and a zero-length match (`a*` and friends)
   * advances by one rather than looping forever. */
  function segments(text: string, re: RegExp | null): Array<{ t: string; hit: boolean }> {
    if (!re) return [{ t: text, hit: false }];
    const g = new RegExp(re.source, re.flags.includes("g") ? re.flags : `${re.flags}g`);
    const out: Array<{ t: string; hit: boolean }> = [];
    let last = 0;
    let m: RegExpExecArray | null;
    while ((m = g.exec(text)) !== null) {
      if (m.index > last) out.push({ t: text.slice(last, m.index), hit: false });
      if (m[0].length === 0) {
        g.lastIndex++;
        continue;
      }
      out.push({ t: m[0], hit: true });
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push({ t: text.slice(last), hit: false });
    return out.length > 0 ? out : [{ t: text, hit: false }];
  }

  const described = $derived(
    item.kind === "event" ? describeEvent(item.name, item.extra) : null,
  );

  /** The gutter color channel for this row — see this component's doc
   * comment. Data rows deliberately return `null` (no band). */
  const tone = $derived.by((): EventTone | null => {
    if (item.kind === "line" || item.kind === "gap") return null;
    if (item.kind === "tx") return "tx";
    if (item.kind === "gate") return "gate";
    return described?.tone ?? "warn";
  });

  let reverting = $state(false);
  let revertError = $state<string | null>(null);

  async function revertConfigChange(): Promise<void> {
    if (item.kind !== "event" || !deviceId) return;
    const old = item.extra.old;
    if (old === null || old === undefined) return;
    reverting = true;
    revertError = null;
    try {
      await setDeviceConfig(deviceId, old as Record<string, unknown>);
      onReverted?.();
    } catch (e) {
      revertError = e instanceof Error ? e.message : String(e);
    } finally {
      reverting = false;
    }
  }
</script>

{#if item.kind === "gap"}
  <div class="row gap" data-testid="log-row" data-row-kind="gap" data-highlighted={highlighted}>
    <span class="chip tnum">no output for {item.deltaS.toFixed(1)}s</span>
  </div>
{:else if item.kind === "line"}
  <div
    class="row data"
    class:folded={item.folded}
    class:highlighted
    data-testid="log-row"
    data-row-kind="line"
    data-folded={item.folded}
    data-highlighted={highlighted}
    data-binary={item.render.kind === "binary_summary" ||
      (item.render.kind === "text" && item.render.rawHex !== null)}
    data-seq={item.seq}
    data-last-seq={item.lastSeq}
  >
    <span class="ts tnum">{timestamp}</span>
    {#if item.folded}
      <button type="button" class="inline-toggle fold-toggle" onclick={() => onToggleExpand(item.id)}>
        {#if item.render.kind === "text"}
          <span class="text">{item.render.text}</span>
        {:else}
          <span class="text dim">{item.render.length} bytes of binary</span>
        {/if}
        <span class="meta"
          >×{fmtCount(item.count)} identical{#if expanded}, {item.tWall} – {item.lastTWall}
            <span class="affordance">· collapse</span>{:else}<span class="affordance"
              >&nbsp;· expand</span
            >{/if}</span
        >
      </button>
    {:else if item.render.kind === "binary_summary"}
      <button type="button" class="inline-toggle binary-toggle" onclick={() => onToggleExpand(item.id)}>
        {#if expanded}
          <span class="hex">{item.render.hexPreview}</span>
        {:else}
          <span class="text dim">{item.render.length} bytes of binary</span>
          <span class="meta affordance">· view as hex</span>
        {/if}
      </button>
    {:else if item.render.rawHex !== null}
      <button type="button" class="inline-toggle binary-toggle" onclick={() => onToggleExpand(item.id)}>
        <span class="text">{item.render.text}</span>
        {#if expanded}
          <span class="hex">{item.render.rawHex}</span>
        {:else}
          <span class="meta affordance">· view as hex</span>
        {/if}
      </button>
    {:else}
      <span class="text"
        >{#each segments(item.render.text, highlightRe) as seg}{#if seg.hit}<mark>{seg.t}</mark
            >{:else}{seg.t}{/if}{/each}</span
      >
    {/if}
  </div>
{:else if item.kind === "tx"}
  <div
    class="row event"
    class:highlighted
    data-testid="log-row"
    data-row-kind="tx"
    data-highlighted={highlighted}
    data-seq={item.seq}
    style="--tone: var(--tx); --tone-bg: var(--tx-bg);"
  >
    <span class="ts tnum">{timestamp}</span>
    <span class="label">Sent</span>
    <span class="text">{item.text}</span>
    <span class="detail">by {item.client} · {item.clientType}</span>
    <!-- How this write got past the gate ("human_rw", "whitelist:…",
         "approved_by:…"). Kept as its own element rather than folded into
         the line above: it is the one field an audit reader looks for, and
         `webui/e2e/approval-card.spec.ts` asserts on it directly. -->
    <span class="gate-badge">{item.gate}</span>
  </div>
{:else if item.kind === "event"}
  <div
    class="row event"
    class:highlighted
    data-testid="log-row"
    data-row-kind="event"
    data-event-name={item.name}
    data-highlighted={highlighted}
    data-seq={item.seq}
    style="--tone: var(--{tone}); --tone-bg: var(--{tone}-bg);"
  >
    <span class="ts tnum">{timestamp}</span>
    <button
      type="button"
      class="inline-toggle"
      title={expanded ? "Hide the original record" : "Show the original record"}
      onclick={() => onToggleExpand(item.id)}
    >
      <span class="label">{described?.summary}</span>
      {#if described?.detail}
        <span class="detail">{described.detail}</span>
      {/if}
      <!-- Rule 1 of `eventText.ts`: the summary is a demotion of the payload,
           never a replacement for it — one click restores the exact record. -->
      {#if expanded}
        <span class="raw">{JSON.stringify(item.extra)}</span>
      {/if}
    </button>
    {#if item.name === "config_change" && deviceId}
      <button
        type="button"
        class="revert-btn"
        data-testid="config-revert"
        disabled={reverting || item.extra.old === null || item.extra.old === undefined}
        onclick={revertConfigChange}
      >
        {reverting ? "Undoing…" : "Undo"}
      </button>
      {#if revertError}
        <span class="revert-error">{revertError}</span>
      {/if}
    {/if}
  </div>
{:else}
  <div
    class="row event"
    class:highlighted
    data-testid="log-row"
    data-row-kind="gate"
    data-gate-action={item.action}
    data-highlighted={highlighted}
    data-seq={item.seq}
    style="--tone: var(--gate); --tone-bg: var(--gate-bg);"
  >
    <span class="ts tnum">{timestamp}</span>
    <span class="label">{item.action === "deny" ? "Blocked" : "Allowed"}</span>
    <span class="detail">{item.reason}</span>
  </div>
{/if}

<style>
  .row {
    height: 22px;
    line-height: 22px;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0 var(--space-2);
    /* `pre` (not `nowrap`) so runs of spaces in device output survive —
     * column-aligned firmware tables are common and collapsing their padding
     * would misrepresent what the board printed. Horizontal overflow is the
     * viewport's job now, not each row's; see the doc comment. */
    white-space: pre;
    font-size: var(--text-base);
    /* The gutter. Data rows leave it transparent — see the doc comment on
     * why the unmarked state belongs to device output. */
    border-left: var(--gutter-w) solid transparent;
  }

  .data {
    font-family: var(--font-mono);
    color: var(--text);
  }

  .event {
    font-family: var(--font-ui);
    background: var(--tone-bg);
    border-left-color: var(--tone);
  }

  .gap {
    justify-content: center;
    font-family: var(--font-ui);
    font-size: var(--text-xs);
    color: var(--text-faint);
  }

  .gap .chip {
    border-top: 1px dashed var(--border-strong);
    padding: 0 var(--space-2);
    line-height: 1;
  }

  .ts {
    color: var(--text-faint);
    flex: none;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
  }

  .label {
    flex: none;
    font-weight: 600;
    color: var(--tone, inherit);
  }

  .detail,
  .meta {
    color: var(--text-dim);
    font-size: var(--text-sm);
  }

  /* The clickable part of an otherwise descriptive label. Without this,
   * "6 bytes of binary" and "view as hex" sit side by side and read as one
   * run-on sentence rather than a description plus an action. */
  .affordance {
    text-decoration: underline dotted;
    text-underline-offset: 2px;
  }

  .inline-toggle:hover .affordance {
    color: var(--text);
    text-decoration-style: solid;
  }

  .dim {
    color: var(--text-dim);
  }

  .gate-badge {
    flex: none;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--text-faint);
  }

  .raw {
    color: var(--text-faint);
    font-family: var(--font-mono);
    font-size: var(--text-sm);
  }

  .inline-toggle {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    cursor: pointer;
    padding: 0;
    white-space: pre;
  }

  .hex {
    color: var(--text-dim);
    font-family: var(--font-mono);
    letter-spacing: 0.03em;
  }

  mark {
    background: var(--tx-bg);
    color: var(--text);
    border-bottom: 1px solid var(--tx);
    border-radius: 1px;
  }

  .highlighted {
    animation: highlight-flash 1.2s ease-out 2;
  }

  @keyframes highlight-flash {
    0% {
      background-color: var(--rx-bg);
    }
    100% {
      background-color: transparent;
    }
  }

  .revert-btn {
    flex: none;
    font: inherit;
    font-size: var(--text-xs);
    padding: 0 var(--space-2);
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
    cursor: pointer;
  }
  .revert-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .revert-error {
    color: var(--gate);
    font-size: var(--text-xs);
  }
</style>
