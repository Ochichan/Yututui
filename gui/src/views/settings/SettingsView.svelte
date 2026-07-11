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
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui } = ctx;

  const TABS: Array<{ id: SettingsTab; label: string }> = $derived([
    { id: 'general', label: t('settings.tab.general') },
    { id: 'playback', label: t('settings.tab.playback') },
    { id: 'hotkeys', label: t('settings.tab.hotkeys') },
    { id: 'graphics', label: t('settings.tab.graphics') },
    { id: 'djgem', label: t('settings.tab.djgem') },
    { id: 'accounts', label: t('settings.tab.accounts') },
  ]);
</script>

<div class="settings">
  <div class="tabs" role="tablist" aria-label={t('settings.tabsAria')}>
    {#each TABS as tab (tab.id)}
      <button
        class="tab"
        class:on={ui.settingsTab === tab.id}
        role="tab"
        aria-selected={ui.settingsTab === tab.id}
        onclick={() => (ui.settingsTab = tab.id)}>{tab.label}</button
      >
    {/each}
  </div>

  <div class="pane" data-ui-scroll-key="settings-pane">
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
