<script lang="ts">
  // Settings host (docs/gui/07 §6–§11): sub-tab bar + the six tabs. General/Playback/DJ Gem
  // bind the live `settings` read model; Hotkeys/Graphics/Accounts stay on their own wires.
  import type { AppCtx } from '../../lib/ctx';
  import type { SettingsTab } from '../../lib/stores/ui.svelte';
  import GeneralTab from './GeneralTab.svelte';
  import PlaybackTab from './PlaybackTab.svelte';
  import HotkeysTab from './HotkeysTab.svelte';
  import GraphicsTab from './GraphicsTab.svelte';
  import DjGemTab from './DjGemTab.svelte';
  import AccountsTab from './AccountsTab.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui } = ctx;

  const TABS: Array<{ id: SettingsTab; label: string }> = [
    { id: 'general', label: 'General' },
    { id: 'playback', label: 'Playback' },
    { id: 'hotkeys', label: 'Hotkeys' },
    { id: 'graphics', label: 'Graphics' },
    { id: 'djgem', label: 'DJ Gem' },
    { id: 'accounts', label: 'Accounts' },
  ];
</script>

<div class="settings">
  <div class="tabs" role="tablist" aria-label="Settings tabs">
    {#each TABS as t (t.id)}
      <button
        class="tab"
        class:on={ui.settingsTab === t.id}
        role="tab"
        aria-selected={ui.settingsTab === t.id}
        onclick={() => (ui.settingsTab = t.id)}>{t.label}</button
      >
    {/each}
  </div>

  <div class="pane">
    {#if ui.settingsTab === 'general'}
      <GeneralTab {ctx} />
    {:else if ui.settingsTab === 'playback'}
      <PlaybackTab {ctx} />
    {:else if ui.settingsTab === 'hotkeys'}
      <HotkeysTab {ctx} />
    {:else if ui.settingsTab === 'graphics'}
      <GraphicsTab {ctx} />
    {:else if ui.settingsTab === 'djgem'}
      <DjGemTab {ctx} />
    {:else}
      <AccountsTab {ctx} />
    {/if}
  </div>
</div>

<style>
  .settings {
    display: flex;
    height: 100%;
    min-height: 0;
  }
  .tabs {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: 148px;
    flex: none;
    padding: var(--space-6) var(--space-3);
  }
  .tab {
    padding: var(--space-2) var(--space-3);
    border: none;
    border-radius: var(--radius-m);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 13px;
    text-align: left;
  }
  .tab:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .tab.on {
    background: var(--surface-2);
    color: var(--role-accent);
    font-weight: 600;
  }
  .pane {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: var(--space-6) var(--space-8) var(--space-8) var(--space-4);
  }
</style>
