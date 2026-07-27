<script lang="ts">
  import type { TimelineSelection } from "./timeline";
  import {
    binFilterConflict,
    buildExportUrl,
    sourceIsReady,
    type ExportFormat,
    type ExportRequest,
    type ExportSource,
  } from "./exportDialog";

  interface Props {
    deviceId: string;
    /** The timeline's current drag-selection, if any — lifted up through
     * `LiveLog.svelte`'s `onTimelineSelect` prop into `App.svelte` and
     * passed straight through here. `null` means no selection has been
     * made (or it was cleared), which disables the "Timeline selection"
     * source option rather than silently falling back to something else. */
    timelineSelection: TimelineSelection | null;
  }
  const { deviceId, timelineSelection }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let format = $state<ExportFormat>("jsonl");
  let source = $state<ExportSource>("range");
  let fromText = $state("");
  let toText = $state("");
  let filterText = $state("");

  const hasSelection = $derived(timelineSelection !== null);
  /** T5.5 acceptance criterion 6: choosing `bin` together with a filter
   * must be explicitly blocked, never silently ignored — see
   * `exportDialog.ts`'s `binFilterConflict` doc comment. */
  const conflict = $derived(binFilterConflict(format, filterText));
  const ready = $derived(sourceIsReady({ source }, hasSelection));
  const canExport = $derived(ready && !conflict);

  const exportUrl = $derived.by((): string => {
    const req: ExportRequest = { format, source, from: fromText, to: toText, filter: filterText };
    const selection = timelineSelection
      ? { fromSeq: timelineSelection.fromSeq, toSeq: timelineSelection.toSeq }
      : null;
    return buildExportUrl(deviceId, req, selection);
  });

  function openDialog(): void {
    dialogEl?.showModal();
  }
  function closeDialog(): void {
    dialogEl?.close();
  }
</script>

<button type="button" data-testid="export-dialog-open" onclick={openDialog}>Export…</button>

<dialog bind:this={dialogEl} data-testid="export-dialog">
  <form method="dialog">
    <h2>Export</h2>

    <fieldset>
      <legend>Source</legend>
      <label>
        <input
          type="radio"
          name="source"
          value="selection"
          bind:group={source}
          disabled={!hasSelection}
          data-testid="export-source-selection"
        />
        Timeline selection
        {#if hasSelection && timelineSelection}
          <span class="hint">(seq {timelineSelection.fromSeq}–{timelineSelection.toSeq})</span>
        {:else}
          <span class="hint">(drag on the timeline first)</span>
        {/if}
      </label>
      <label>
        <input type="radio" name="source" value="range" bind:group={source} data-testid="export-source-range" />
        Time range
      </label>
      {#if source === "range"}
        <div class="range-inputs">
          <input type="text" placeholder="from (seq or RFC3339)" bind:value={fromText} data-testid="export-from" />
          <input type="text" placeholder="to (seq or RFC3339)" bind:value={toText} data-testid="export-to" />
        </div>
      {/if}
      <label>
        <input type="radio" name="source" value="boot" bind:group={source} data-testid="export-source-boot" />
        --boot (most recent boot marker to now)
      </label>
    </fieldset>

    <fieldset>
      <legend>Format</legend>
      <label>
        <input type="radio" name="format" value="jsonl" bind:group={format} data-testid="export-format-jsonl" />
        jsonl
      </label>
      <label>
        <input type="radio" name="format" value="txt" bind:group={format} data-testid="export-format-txt" />
        txt
      </label>
      <label>
        <input type="radio" name="format" value="bin" bind:group={format} data-testid="export-format-bin" />
        bin
      </label>
    </fieldset>

    <label class="filter-label">
      Filter (regex, jsonl/txt only)
      <input type="text" bind:value={filterText} data-testid="export-filter" placeholder="e.g. ERROR" />
    </label>

    {#if conflict}
      <p class="error" data-testid="export-bin-filter-error">
        bin cannot be combined with a filter — it would silently break byte-exactness. Clear the
        filter, or choose jsonl/txt instead.
      </p>
    {/if}

    <div class="actions">
      <button type="button" onclick={closeDialog}>Cancel</button>
      {#if canExport}
        <a class="export-link" href={exportUrl} download data-testid="export-download" onclick={closeDialog}>
          Export
        </a>
      {:else}
        <button type="button" class="export-link" data-testid="export-download" disabled> Export </button>
      {/if}
    </div>
  </form>
</dialog>

<style>
  button {
    font: inherit;
    padding: 0.25rem 0.75rem;
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

  dialog {
    border: 1px solid var(--border);
    border-radius: 0.6rem;
    background: var(--surface);
    color: inherit;
    padding: 1.25rem;
    min-width: 22rem;
  }

  dialog::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  h2 {
    margin: 0 0 0.75rem;
    font-size: 1rem;
  }

  fieldset {
    border: 1px solid var(--border);
    border-radius: 0.4rem;
    margin: 0 0 0.75rem;
    padding: 0.5rem 0.75rem 0.6rem;
  }

  legend {
    color: var(--text-dim);
    font-size: 0.8125rem;
    padding: 0 0.3rem;
  }

  fieldset label {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.875rem;
    padding: 0.2rem 0;
  }

  .hint {
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .range-inputs {
    display: flex;
    gap: 0.4rem;
    margin: 0.2rem 0 0.2rem 1.4rem;
  }

  .range-inputs input,
  .filter-label input {
    font: inherit;
    font-family: var(--font-mono);
    padding: 0.25rem 0.5rem;
    border-radius: 0.35rem;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: inherit;
  }

  .filter-label {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.8125rem;
    color: var(--text-dim);
    margin-bottom: 0.75rem;
  }

  .error {
    color: var(--dot-closed);
    font-size: 0.8125rem;
    margin: 0 0 0.75rem;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .export-link {
    text-decoration: none;
    display: inline-flex;
    align-items: center;
  }
</style>
