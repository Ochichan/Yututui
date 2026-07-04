<script lang="ts">
  // Settings → Hotkeys (docs/gui/07 §8): the remappable keymap, grouped by the 11 KeyContexts,
  // rendered from the live `settings` keymap model. Each row shows the current chord, a Rebind
  // (ChordCapture) affordance, and a per-row reset; conflict detection stays core-side and its
  // reply is shown inline. The in-webview dispatcher (App.svelte + lib/keyboard) consumes the
  // same model, so a remap takes effect immediately.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Kbd from '../../lib/components/Kbd.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { keymap } = ctx;

  const groups = $derived(keymap.groups);

  function labelFor(shadows: string): string {
    return keymap.actions.find((a) => a.id === shadows)?.label ?? shadows;
  }
</script>

{#if groups.length === 0}
  <p class="empty">Loading the keymap…</p>
{/if}

{#each groups as g (g.context)}
  <SettingSection title={g.label}>
    {#each g.actions as a (a.context + '.' + a.id)}
      {@const chord = keymap.chordFor(a)}
      {@const conflicted = keymap.conflict?.key === a.context + '.' + a.id}
      <SettingRow label={a.label}>
        {#if chord}
          <Kbd {chord} />
        {:else}
          <span class="unbound">unbound</span>
        {/if}
        {#if conflicted}
          <span class="conflict" title="Also bound to {labelFor(keymap.conflict!.shadows)}">
            ⚠ {labelFor(keymap.conflict!.shadows)}
          </span>
        {/if}
        <button class="mini" onclick={() => keymap.startCapture(a.context, a.id)}>Rebind</button>
        {#if chord !== a.default_chord}
          <button
            class="mini ghost"
            title="Reset to {a.default_chord}"
            onclick={() => keymap.resetBinding(a)}>↺</button
          >
        {/if}
      </SettingRow>
    {/each}
  </SettingSection>
{/each}

{#if groups.length}
  <SettingSection title="Danger zone">
    <SettingRow label="Reset all keybindings" hint="Restore every chord to its default">
      <button class="mini danger" onclick={() => keymap.resetAll()}>Reset all…</button>
    </SettingRow>
  </SettingSection>
{/if}

<style>
  .empty {
    padding: var(--space-6);
    color: var(--role-text-subtle);
    font-size: 12.5px;
  }
  .unbound {
    font-size: 11px;
    color: var(--role-text-subtle);
    font-style: italic;
  }
  .conflict {
    font-size: 10.5px;
    color: var(--role-warning);
  }
  .mini {
    padding: 2px 10px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 11px;
  }
  .mini:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .mini.ghost {
    padding: 2px 8px;
    color: var(--role-accent);
  }
  .mini.danger {
    border-color: var(--role-error);
    color: var(--role-error);
  }
</style>
