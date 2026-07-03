<script lang="ts">
  // Library (docs/gui/07 §4): tab bar, filter box, list, Play All / Enqueue All. Radio
  // mode swaps the tab set. The page-fetch wire (library topic + Fetch(LibraryPage)) is
  // pending, so every tab shows its designed empty state through the patch-bay gate.
  import type { AppCtx } from '../lib/ctx';
  import type { LibraryTab } from '../lib/stores/ui.svelte';
  import PendingSurface from '../lib/components/PendingSurface.svelte';
  import WireTag from '../lib/components/WireTag.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui, wip, playback } = ctx;

  const MUSIC_TABS: Array<{ id: LibraryTab; label: string }> = [
    { id: 'all', label: 'All' },
    { id: 'favorites', label: 'Favorites' },
    { id: 'history', label: 'History' },
    { id: 'downloads', label: 'Downloads' },
    { id: 'playlists', label: 'Playlists' },
  ];
  const RADIO_TABS: Array<{ id: LibraryTab; label: string }> = [
    { id: 'radio_likes', label: 'Radio Likes' },
    { id: 'radio_history', label: 'Radio History' },
  ];
  const tabs = $derived(playback.model?.radio_mode ? RADIO_TABS : MUSIC_TABS);
  const tab = $derived(tabs.some((t) => t.id === ui.libraryTab) ? ui.libraryTab : tabs[0].id);

  let filter = $state('');

  const EMPTY_BODY: Record<LibraryTab, string> = {
    all: 'Every track you have played, liked, or downloaded lists here with filter-as-you-type.',
    favorites: 'Tracks you ♥ collect here; the heart on any row toggles membership.',
    history: 'Your listening history, newest first.',
    downloads:
      'No downloads yet — press ⬇ on a track. Rows show Running % / Done / Failed with retry.',
    playlists: 'Local playlists with drill-down, plus Create / Delete / Add-to-playlist dialogs.',
    radio_likes: 'Stations you liked in radio mode.',
    radio_history: 'Stations you tuned in radio mode, newest first.',
  };

  function pendingId(t: LibraryTab) {
    return t === 'playlists'
      ? 'library.playlists'
      : t === 'downloads'
        ? 'downloads.manage'
        : 'library.fetch';
  }

  function playAll() {
    // TODO(wire:M2/library.fetch): LibraryPlay { scope, filter } once the wire lands.
    wip.gate('library.fetch');
  }
</script>

<div class="library">
  <header>
    <div class="tabs" role="tablist" aria-label="Library tabs">
      {#each tabs as t (t.id)}
        <button
          class="tab"
          class:on={tab === t.id}
          role="tab"
          aria-selected={tab === t.id}
          onclick={() => (ui.libraryTab = t.id)}>{t.label}</button
        >
      {/each}
    </div>
    <div class="actions">
      <input
        class="ti filter"
        type="search"
        placeholder="Filter…  (/)"
        bind:value={filter}
        aria-label="Filter library"
      />
      <button class="act" onclick={playAll}>▶ Play all</button>
      <button class="act" onclick={playAll}>+ Enqueue all</button>
      {#if tab === 'playlists'}
        <button class="act" onclick={() => wip.gate('library.playlists')}>＋ New playlist</button>
      {/if}
    </div>
  </header>

  <div class="body">
    <PendingSurface id={pendingId(tab)} {wip} glyph="📚" body={EMPTY_BODY[tab]} />
  </div>

  <footer class="foot">
    <WireTag id={pendingId(tab)} {wip} />
  </footer>
</div>

<style>
  .library {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: var(--space-6) var(--space-8);
    gap: var(--space-4);
  }
  header {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .tabs {
    display: flex;
    gap: var(--space-1);
    border-bottom: 1px solid var(--role-border-muted);
  }
  .tab {
    padding: var(--space-2) var(--space-4);
    border: none;
    background: transparent;
    color: var(--role-text-muted);
    font-size: 13px;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
  }
  .tab:hover {
    color: var(--role-text-primary);
  }
  .tab.on {
    color: var(--role-accent);
    border-bottom-color: var(--role-accent);
    font-weight: 600;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .filter {
    width: 220px;
  }
  .act {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .act:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .body {
    flex: 1;
    display: grid;
    place-items: center;
    min-height: 0;
  }
  .foot {
    display: flex;
    justify-content: flex-end;
  }
</style>
