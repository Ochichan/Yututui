<script lang="ts">
  // Settings → Hotkeys (docs/gui/07 §8). Until the keymap read model + ChordCapture land,
  // this honestly documents the GUI's provisional shortcuts (the same table App.svelte
  // executes and the Help overlay shows) and gates rebinding through the patch bay.
  //
  // TODO(wire:M3/settings.hotkeys): render the 11 KeyContext groups from the pushed
  // keymap model, with per-row Rebind (ChordCapture) and core-side conflict display.
  import type { AppCtx } from '../../lib/ctx';
  import { PROVISIONAL_SHORTCUTS } from '../../lib/keyboard/provisional';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Kbd from '../../lib/components/Kbd.svelte';
  import PendingSurface from '../../lib/components/PendingSurface.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip } = ctx;
</script>

<SettingSection title="GUI shortcuts (provisional)">
  {#each PROVISIONAL_SHORTCUTS as s (s.chord)}
    <SettingRow label={s.label}>
      <Kbd chord={s.chord} />
      <button class="rebind" onclick={() => wip.gate('settings.hotkeys')}>Rebind</button>
    </SettingRow>
  {/each}
</SettingSection>

<div class="pending">
  <PendingSurface
    id="settings.hotkeys"
    {wip}
    glyph="⌨"
    body="The full remappable keymap — all 11 contexts, your TUI bindings honored identically (Korean IME included), chord capture, and core-side conflict detection."
  />
</div>

<style>
  .rebind {
    padding: 2px 10px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 11px;
  }
  .rebind:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .pending {
    margin-top: var(--space-6);
  }
</style>
