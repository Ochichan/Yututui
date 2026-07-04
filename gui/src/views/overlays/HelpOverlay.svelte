<script lang="ts">
  // Help overlay (docs/gui/07 §17): a cheat-sheet auto-generated from the live keymap read
  // model — grouped by context title, each row a chord + localized action label, searchable.
  // One source with Settings→Hotkeys and the dispatcher (lib/stores/keymap.svelte.ts), so the
  // GUI's help reads exactly the bindings the app honors.
  import type { AppCtx } from '../../lib/ctx';
  import Modal from '../../lib/components/Modal.svelte';
  import Kbd from '../../lib/components/Kbd.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui, keymap } = ctx;

  let query = $state('');

  const groups = $derived.by(() => {
    const q = query.trim().toLowerCase();
    return keymap.groups
      .map((g) => ({
        ...g,
        rows: g.actions
          .map((a) => ({ label: a.label, chord: keymap.chordFor(a) }))
          .filter(
            (r) => !q || r.label.toLowerCase().includes(q) || r.chord.toLowerCase().includes(q),
          ),
      }))
      .filter((g) => g.rows.length > 0);
  });
</script>

<Modal title={t('help.title')} onclose={() => (ui.helpOpen = false)} width="520px">
  <input
    class="search"
    type="search"
    placeholder={t('help.filterPlaceholder')}
    bind:value={query}
    aria-label={t('help.filterAria')}
  />
  {#if groups.length === 0}
    <p class="empty">{keymap.model ? t('help.noMatch') : t('help.loading')}</p>
  {/if}
  <div class="groups">
    {#each groups as g (g.context)}
      <section>
        <h3>{g.label}</h3>
        <div class="rows">
          {#each g.rows as r (r.label)}
            <div class="row">
              {#if r.chord}
                <Kbd chord={r.chord} />
              {:else}
                <span class="unbound">—</span>
              {/if}
              <span class="action">{r.label}</span>
            </div>
          {/each}
        </div>
      </section>
    {/each}
  </div>
  <p class="foot">
    {t('help.customize')}
    <button
      class="link"
      onclick={() => {
        ui.helpOpen = false;
        ui.setView('settings');
        ui.settingsTab = 'hotkeys';
      }}>{t('help.open')}</button
    >
  </p>
</Modal>

<style>
  .search {
    width: 100%;
    margin: 0 0 var(--space-4);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-m);
    background: var(--surface-1);
    color: var(--role-text-primary);
    font-size: 12.5px;
  }
  .empty {
    margin: 0 0 var(--space-4);
    font-size: 12px;
    color: var(--role-text-subtle);
  }
  .groups {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    max-height: 52vh;
    overflow-y: auto;
  }
  h3 {
    margin: 0 0 var(--space-2);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--role-help-group);
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
  .unbound {
    min-width: 20px;
    text-align: center;
    color: var(--role-text-subtle);
    font-size: 11px;
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
