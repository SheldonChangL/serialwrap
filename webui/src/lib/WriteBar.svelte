<script lang="ts">
  /**
   * The operator's half of the conversation.
   *
   * Until this existed the GUI could only listen: the CLI had
   * `serialwrap write` and an agent had the MCP `write` tool, but the one
   * surface a person actually sits in front of couldn't send a byte, which
   * made "share the port" true for every client except the human. This is
   * the missing half.
   *
   * Three decisions worth naming:
   *
   * - **Line ending is a visible control, not a default.** Sending `LF` to a
   *   board that wants `CRLF` looks exactly like the board ignoring you, and
   *   it is the single most common way a serial session wastes ten minutes.
   *   It sits next to Send, at the size of a thing you are expected to check.
   * - **HEX mode parses in the browser.** The daemon's write endpoint takes
   *   text-or-base64 exactly like its UDS counterpart; keeping hex parsing
   *   here means the daemon has one byte-decoding path, not two.
   * - **History is per device.** Up-arrow recalls what you sent to *this*
   *   board, which is the only recall that is ever useful.
   */
  import { tick } from "svelte";
  import { completeWord, longestCommonPrefix } from "./completion";
  import { writeToDevice, type LineEnding } from "./logStream";

  interface Props {
    deviceId: string;
    connected: boolean;
  }
  const { deviceId, connected }: Props = $props();

  type Mode = "text" | "hex";

  let value = $state("");
  let mode = $state<Mode>("text");
  let lineEnding = $state<LineEnding>("lf");
  let sending = $state(false);
  let error = $state<string | null>(null);
  let inputEl: HTMLInputElement | undefined = $state();

  /** Sent payloads, newest last. `historyIndex === null` means "editing a
   * fresh line"; otherwise it indexes into `history` from the end. */
  let history = $state<string[]>([]);
  let historyIndex = $state<number | null>(null);
  let draft = "";

  // Switching devices does not need to be handled here: `App.svelte` keys
  // this whole subtree on the selected device id, so a change remounts the
  // bar with a fresh history and an empty entry. Resetting in an effect as
  // well would be a second, redundant mechanism for the same guarantee.

  export function focus(): void {
    inputEl?.focus();
  }

  /** Parse `DE AD BE EF` / `deadbeef` into bytes. Whitespace and an optional
   * `0x` per pair are tolerated because both are how people actually paste
   * hex; everything else is rejected loudly rather than silently dropped —
   * a mistyped byte going to a serial port is not a typo you want swallowed. */
  function parseHex(input: string): Uint8Array {
    const cleaned = input.replace(/0x/gi, "").replace(/[\s,]/g, "");
    if (cleaned.length === 0) throw new Error("No bytes to send");
    if (cleaned.length % 2 !== 0) {
      throw new Error(`Hex needs an even number of digits — got ${cleaned.length}`);
    }
    const bad = cleaned.match(/[^0-9a-f]/i);
    if (bad) throw new Error(`"${bad[0]}" isn't a hex digit`);
    const out = new Uint8Array(cleaned.length / 2);
    for (let i = 0; i < out.length; i++) {
      out[i] = Number.parseInt(cleaned.slice(i * 2, i * 2 + 2), 16);
    }
    return out;
  }

  function toBase64(bytes: Uint8Array): string {
    let bin = "";
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin);
  }

  async function send(): Promise<void> {
    if (sending) return;
    error = null;
    const raw = value;
    try {
      const payload =
        mode === "hex"
          ? { data_b64: toBase64(parseHex(raw)) }
          : { text: raw, line_ending: lineEnding };
      sending = true;
      await writeToDevice(deviceId, payload);
      // Only remember payloads that actually went out, and don't stack
      // duplicates: holding Enter on the same command shouldn't bury the
      // rest of the history.
      if (raw.length > 0 && history[history.length - 1] !== raw) history = [...history, raw];
      historyIndex = null;
      value = "";
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      sending = false;
    }
    // Refocus only after `sending` is back to false *and* the DOM has
    // caught up: the entry is `disabled` while sending, and calling
    // `focus()` on a still-disabled input is silently ignored — which is
    // exactly the bug this ordering fixes (the old code focused inside the
    // `try`, before `finally` re-enabled the input). On error the entry
    // keeps the rejected payload and the focus, ready to fix and resend.
    await tick();
    inputEl?.focus();
  }

  /** Tab-completion state (see `completion.ts` for where candidates come
   * from). `suggestions` non-empty means a strip of candidates is showing
   * and further Tabs cycle through it; any edit or Escape dismisses it. */
  let suggestions = $state<string[]>([]);
  let suggestIndex = $state(-1);
  let suggestWordStart = 0;

  function clearSuggestions(): void {
    suggestions = [];
    suggestIndex = -1;
  }

  function applyCandidate(candidate: string): void {
    value = value.slice(0, suggestWordStart) + candidate;
    inputEl?.focus();
  }

  /** Terminal-style Tab: first press completes to the longest common
   * prefix (applying outright if only one candidate), further presses
   * cycle the remaining candidates. History words complete too — commands
   * you've sent are as likely to be retyped as paths are. */
  function completeAtCursor(): void {
    if (suggestions.length > 0) {
      suggestIndex = (suggestIndex + 1) % suggestions.length;
      applyCandidate(suggestions[suggestIndex]);
      return;
    }
    const wordStart = Math.max(value.lastIndexOf(" "), value.lastIndexOf("\t")) + 1;
    const word = value.slice(wordStart);
    if (word === "") return;
    const fromHistory = word.startsWith("/")
      ? []
      : history.flatMap((h) => h.split(/\s+/)).filter((w) => w.startsWith(word) && w !== word);
    const candidates = [...new Set([...completeWord(deviceId, word), ...fromHistory])];
    if (candidates.length === 0) return;
    suggestWordStart = wordStart;
    if (candidates.length === 1) {
      applyCandidate(candidates[0]);
      return;
    }
    const lcp = longestCommonPrefix(candidates);
    if (lcp.length > word.length) value = value.slice(0, wordStart) + lcp;
    suggestions = candidates;
    suggestIndex = -1;
  }

  function recall(direction: -1 | 1): void {
    if (history.length === 0) return;
    if (historyIndex === null) {
      if (direction === 1) return;
      draft = value;
      historyIndex = history.length - 1;
    } else {
      const next = historyIndex + direction;
      if (next >= history.length) {
        historyIndex = null;
        value = draft;
        return;
      }
      historyIndex = Math.max(0, next);
    }
    value = history[historyIndex];
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Enter") {
      e.preventDefault();
      clearSuggestions();
      void send();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      clearSuggestions();
      recall(-1);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      clearSuggestions();
      recall(1);
    } else if (e.key === "Tab" && !e.shiftKey && mode === "text" && value !== "") {
      // Hijacked only mid-composition: an empty entry keeps Tab's normal
      // focus-order meaning, so keyboard users aren't trapped in the input.
      e.preventDefault();
      completeAtCursor();
    } else if (e.key === "Escape") {
      clearSuggestions();
    } else if (e.key !== "Shift") {
      // Any real edit invalidates the strip — the word it was computed
      // for no longer exists.
      clearSuggestions();
    }
  }

  const placeholder = $derived(
    !connected
      ? "Port is closed — nothing to send to"
      : mode === "hex"
        ? "DE AD BE EF"
        : "Type a command, press Enter",
  );
</script>

{#if suggestions.length > 0}
  <div class="suggestions" data-testid="write-suggestions" role="listbox" aria-label="Completions">
    {#each suggestions as s, i}
      <button
        type="button"
        class="suggestion"
        class:selected={i === suggestIndex}
        role="option"
        aria-selected={i === suggestIndex}
        onclick={() => {
          applyCandidate(s);
          clearSuggestions();
        }}>{s}</button
      >
    {/each}
  </div>
{/if}

<form
  class="write-bar"
  data-testid="write-bar"
  onsubmit={(e) => {
    e.preventDefault();
    void send();
  }}
>
  <span class="arrow" aria-hidden="true">›</span>

  <input
    type="text"
    class="entry"
    data-testid="write-input"
    bind:this={inputEl}
    bind:value
    {placeholder}
    disabled={!connected || sending}
    spellcheck="false"
    autocomplete="off"
    autocapitalize="off"
    aria-label="Bytes to send to the serial port"
    onkeydown={onKeyDown}
    oninput={clearSuggestions}
  />

  <div class="modes" role="group" aria-label="Payload format">
    <button
      type="button"
      class="mode"
      class:on={mode === "text"}
      data-testid="write-mode-text"
      aria-pressed={mode === "text"}
      onclick={() => (mode = "text")}>Text</button
    >
    <button
      type="button"
      class="mode"
      class:on={mode === "hex"}
      data-testid="write-mode-hex"
      aria-pressed={mode === "hex"}
      onclick={() => (mode = "hex")}>Hex</button
    >
  </div>

  <label class="ending">
    <span class="label-eyebrow">Ends with</span>
    <select
      data-testid="write-line-ending"
      bind:value={lineEnding}
      disabled={mode === "hex"}
      title={mode === "hex" ? "Hex is sent exactly as typed, with nothing appended" : undefined}
    >
      <option value="lf">LF</option>
      <option value="crlf">CRLF</option>
      <option value="cr">CR</option>
      <option value="none">Nothing</option>
    </select>
  </label>

  <button
    type="submit"
    class="send"
    data-testid="write-send"
    disabled={!connected || sending}
  >
    {sending ? "Sending…" : "Send"}
  </button>
</form>

{#if error}
  <p class="error" role="alert" data-testid="write-error">{error}</p>
{/if}

<style>
  .write-bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-top: 1px solid var(--border);
    /* The write bar wears the TX channel: a thicker version of the same
     * gutter every sent row carries in the log above it, so "the amber band
     * means you did that" holds from the control down to its own record. */
    border-left: var(--gutter-w) solid var(--tx);
    background: var(--surface);
  }

  .arrow {
    color: var(--tx);
    font-family: var(--font-mono);
    font-size: var(--text-lg);
    line-height: 1;
    flex: none;
  }

  .entry {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-family: var(--font-mono);
    font-size: var(--text-base);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface-sunken);
    color: var(--text);
  }

  .entry:disabled {
    color: var(--text-faint);
    background: var(--surface-raised);
  }

  .entry:focus {
    border-color: var(--tx);
  }

  .modes {
    display: inline-flex;
    flex: none;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }

  .mode {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border: none;
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
  }

  .mode.on {
    background: var(--tx-bg);
    color: var(--tx);
    font-weight: 600;
  }

  .ending {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    flex: none;
  }

  .ending select {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface-raised);
    color: var(--text);
  }

  .ending select:disabled {
    color: var(--text-faint);
  }

  .send {
    flex: none;
    font: inherit;
    font-size: var(--text-sm);
    font-weight: 600;
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--tx);
    border-radius: var(--radius-sm);
    background: var(--tx-bg);
    color: var(--tx);
    cursor: pointer;
  }

  .send:disabled {
    opacity: 0.45;
    cursor: default;
  }

  /* The completion strip sits where a terminal would print its candidate
   * list: directly above the prompt, in the data font, gone on the next
   * keystroke. */
  .suggestions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
    padding: var(--space-1) var(--space-3);
    border-top: 1px solid var(--border);
    border-left: var(--gutter-w) solid var(--tx);
    background: var(--surface);
  }

  .suggestion {
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
  }

  .suggestion:hover,
  .suggestion.selected {
    color: var(--tx);
    border-color: var(--tx);
    background: var(--tx-bg);
  }

  .error {
    margin: 0;
    padding: var(--space-1) var(--space-3);
    color: var(--gate);
    font-size: var(--text-sm);
    background: var(--gate-bg);
  }

  /* Narrow: the format toggle and line ending drop below the entry rather
   * than squeezing it to nothing. The entry and Send stay on the first row —
   * those two are the whole point of the bar. */
  @media (max-width: 40rem) {
    .write-bar {
      flex-wrap: wrap;
    }
    .entry {
      flex: 1 1 100%;
      order: -1;
    }
  }
</style>
