<script lang="ts">
  // Help overlay (docs/gui/07 §17). Until the keymap read model lands this shows the
  // GUI's real provisional shortcuts (the exact table App.svelte executes) — never a
  // fabricated TUI cheat sheet.
  import type { AppCtx } from '../../lib/ctx';
  import { PROVISIONAL_SHORTCUTS } from '../../lib/keyboard/provisional';
  import Modal from '../../lib/components/Modal.svelte';
  import Kbd from '../../lib/components/Kbd.svelte';
  import WireTag from '../../lib/components/WireTag.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui, wip } = ctx;
</script>

<Modal title="Keyboard shortcuts" onclose={() => (ui.helpOpen = false)} width="480px">
  <p class="note">
    GUI provisional set — the full remappable cheat sheet, auto-generated from your keymap, lands
    with the M3 dispatcher. <WireTag id="help.keymap" {wip} />
  </p>
  <div class="rows">
    {#each PROVISIONAL_SHORTCUTS as s (s.chord)}
      <div class="row">
        <Kbd chord={s.chord} />
        <span class="action">{s.label}</span>
      </div>
    {/each}
  </div>
  <p class="foot">
    Customize in Settings → Hotkeys
    <button
      class="link"
      onclick={() => {
        ui.helpOpen = false;
        ui.setView('settings');
        ui.settingsTab = 'hotkeys';
      }}>open</button
    >
  </p>
</Modal>

<style>
  .note {
    margin: 0 0 var(--space-4);
    font-size: 12px;
    line-height: 1.5;
    color: var(--role-text-muted);
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .action {
    font-size: 12.5px;
    color: var(--role-help-action);
  }
  .foot {
    margin: var(--space-6) 0 0;
    font-size: 11.5px;
    color: var(--role-text-subtle);
  }
  .link {
    border: none;
    background: transparent;
    color: var(--role-accent);
    font-size: 11.5px;
    padding: 0;
    text-decoration: underline;
  }
</style>
