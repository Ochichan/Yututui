<script lang="ts">
  // Chord capture (docs/gui/07 §8, 05 §8.4 branch 3): a modal that grabs exactly one chord.
  // Code-based so it works while a Korean IME is active; Esc cancels. Mounted by App while
  // `keymap.capture` is set — the global dispatcher stands down for that window.
  import type { KeymapStore } from '../stores/keymap.svelte';
  import { CONTEXT_LABELS } from '../stores/keymap.svelte';
  import { chordFromCapture, isModifierKey } from '../keyboard/chord';

  interface Props {
    keymap: KeymapStore;
  }
  const { keymap }: Props = $props();

  const target = $derived(keymap.capture);
  const action = $derived(
    target
      ? keymap.actions.find((a) => a.context === target.context && a.id === target.action)
      : null,
  );

  function onkeydown(e: KeyboardEvent) {
    if (!keymap.capture) return;
    // The capture owns the keyboard while it's open — nothing else should react.
    e.preventDefault();
    e.stopPropagation();
    if (e.key === 'Escape' && !e.isComposing) {
      keymap.cancelCapture();
      return;
    }
    if (isModifierKey(e.key)) return; // wait for the non-modifier key of the chord
    const chord = chordFromCapture(e);
    if (chord) keymap.applyCapture(chord);
  }
</script>

<svelte:window {onkeydown} />

{#if target}
  <div class="scrim" role="dialog" aria-modal="true" aria-label="Capture a shortcut">
    <div class="card">
      <p class="prompt">Press a key…</p>
      {#if action}
        <p class="sub">
          for <strong>{action.label}</strong>
          <span class="ctx">· {CONTEXT_LABELS[target.context]}</span>
        </p>
      {/if}
      <p class="hint">Esc to cancel</p>
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 90;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgb(0 0 0 / 0.5);
  }
  .card {
    min-width: 260px;
    padding: var(--space-6) var(--space-8);
    border: 1px solid var(--role-border-focused);
    border-radius: var(--radius-l);
    background: var(--surface-1);
    box-shadow: var(--elev-3);
    text-align: center;
  }
  .prompt {
    margin: 0 0 var(--space-2);
    font-size: 18px;
    font-weight: 600;
    color: var(--role-text-primary);
  }
  .sub {
    margin: 0 0 var(--space-3);
    font-size: 12.5px;
    color: var(--role-text-muted);
  }
  .ctx {
    color: var(--role-text-subtle);
  }
  .hint {
    margin: 0;
    font-size: 11px;
    color: var(--role-text-subtle);
  }
</style>
