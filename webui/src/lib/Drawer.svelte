<script lang="ts">
  /**
   * A panel that slides up over the log from the status bar.
   *
   * The clients list, the audit view and the device list used to be cards
   * stacked below the log, each permanently occupying screen the log wanted
   * and each demanding attention it rarely deserved. They are reference
   * material: you consult them when a question comes up, then you go back to
   * watching the port. A drawer says exactly that — it is one click away, it
   * covers the log while you read it, and it gets out of the way.
   *
   * Deliberately not a modal `<dialog>`: closing it must never be mandatory,
   * and the log behind it keeps streaming and stays legible at the edges.
   */
  import type { Snippet } from "svelte";

  interface Props {
    title: string;
    open: boolean;
    onClose: () => void;
    testid?: string;
    children: Snippet;
  }
  const { title, open, onClose, testid, children }: Props = $props();
</script>

{#if open}
  <section
    class="drawer"
    data-testid={testid}
    aria-label={title}
    tabindex="-1"
  >
    <header>
      <h2 class="label-eyebrow">{title}</h2>
      <button type="button" class="close" onclick={onClose} aria-label="Close {title}">
        Close
      </button>
    </header>
    <div class="body">
      {@render children()}
    </div>
  </section>
{/if}

<style>
  .drawer {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 20;
    max-height: min(60vh, 32rem);
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border-top: 1px solid var(--border-strong);
    box-shadow: 0 -12px 32px rgb(0 0 0 / 0.28);
    animation: drawer-in 140ms ease-out;
  }

  @keyframes drawer-in {
    from {
      transform: translateY(8px);
      opacity: 0;
    }
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border);
  }

  h2 {
    margin: 0;
  }

  .close {
    font: inherit;
    font-size: var(--text-sm);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface-raised);
    color: var(--text-dim);
    cursor: pointer;
  }

  .close:hover {
    color: var(--text);
    border-color: var(--border-strong);
  }

  .body {
    overflow: auto;
    padding: var(--space-3);
  }
</style>
