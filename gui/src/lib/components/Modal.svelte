<script lang="ts">
  // Generic modal: backdrop, card, focus trap, Escape to close (docs/gui/05 §10). Esc
  // routes through the M3 dispatcher as `Back` once it exists; until then it's direct.
  import type { Snippet } from 'svelte';

  interface Props {
    title?: string;
    width?: string;
    onclose: () => void;
    children: Snippet;
  }
  const { title, width = '520px', onclose, children }: Props = $props();

  let card = $state<HTMLElement | null>(null);
  let previouslyFocused: Element | null = null;

  $effect(() => {
    previouslyFocused = document.activeElement;
    card?.focus();
    return () => {
      if (previouslyFocused instanceof HTMLElement) previouslyFocused.focus();
    };
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key !== 'Tab' || !card) return;
    // Minimal focus trap: cycle within the card.
    const focusables = card.querySelectorAll<HTMLElement>(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="backdrop"
  onclick={(e) => e.target === e.currentTarget && onclose()}
  role="presentation"
>
  <div
    class="card"
    role="dialog"
    aria-modal="true"
    aria-label={title}
    tabindex="-1"
    bind:this={card}
    {onkeydown}
    style:max-width={width}
  >
    {#if title}
      <header>
        <h2>{title}</h2>
        <button class="x" onclick={onclose} aria-label="Close">✕</button>
      </header>
    {/if}
    <div class="body">
      {@render children()}
    </div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    background: rgb(0 0 0 / 0.45);
    padding: var(--space-6);
  }
  .card {
    width: 100%;
    max-height: 85vh;
    overflow: auto;
    background: var(--surface-1);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-l);
    box-shadow: var(--elev-3);
    outline: none;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-4) var(--space-6) 0;
  }
  h2 {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
  }
  .x {
    border: none;
    background: transparent;
    color: var(--role-text-muted);
    font-size: 14px;
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
  }
  .x:hover {
    color: var(--role-text-primary);
    background: var(--surface-2);
  }
  .body {
    padding: var(--space-4) var(--space-6) var(--space-6);
  }
</style>
