<script lang="ts">
  // Library (docs/gui/07 §4): tab bar, filter box, list, Play All / Enqueue All. Radio mode
  // swaps the tab set. The scope tabs (all/favorites/history + the radio pair) pull windowed
  // pages from library.svelte.ts; the downloads / playlists tabs are their own features and
  // still show their designed empty state through the patch-bay gate.
  import type { AppCtx } from '../lib/ctx';
  import type { LibraryTab } from '../lib/stores/ui.svelte';
  import type { LibraryScope } from '../lib/stores/library.svelte';
  import TrackRow from '../lib/components/TrackRow.svelte';
  import PlaylistsPane from './library/PlaylistsPane.svelte';
  import { t } from '../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui, playback, library, downloads, playlists } = ctx;

  const MUSIC_TABS: Array<{ id: LibraryTab; label: string }> = $derived([
    { id: 'all', label: t('common.all') },
    { id: 'favorites', label: t('library.favorites') },
    { id: 'history', label: t('library.history') },
    { id: 'downloads', label: t('library.downloads') },
    { id: 'playlists', label: t('library.playlists') },
  ]);
  const RADIO_TABS: Array<{ id: LibraryTab; label: string }> = $derived([
    { id: 'radio_likes', label: t('library.radioLikes') },
    { id: 'radio_history', label: t('library.radioHistory') },
  ]);
  const tabs = $derived(playback.model?.radio_mode ? RADIO_TABS : MUSIC_TABS);
  const tab = $derived(tabs.some((t) => t.id === ui.libraryTab) ? ui.libraryTab : tabs[0].id);

  let filter = $state('');

  const SCOPES: readonly LibraryTab[] = [
    'all',
    'favorites',
    'history',
    'radio_likes',
    'radio_history',
  ];
  function isScope(t: LibraryTab): t is LibraryScope {
    return SCOPES.includes(t);
  }
  const removable = (t: LibraryTab) => t !== 'all';

  const EMPTY_BODY: Record<LibraryTab, string> = $derived({
    all: t('library.emptyAll'),
    favorites: t('library.emptyFavorites'),
    history: t('library.emptyHistory'),
    downloads: t('library.emptyDownloads'),
    playlists: t('library.emptyPlaylists'),
    radio_likes: t('library.emptyRadioLikes'),
    radio_history: t('library.emptyRadioHistory'),
  });

  // Pull the active scope, debounced on the filter so typing doesn't spam the core.
  let debounce: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    const t = tab;
    const f = filter;
    if (!isScope(t)) return;
    clearTimeout(debounce);
    debounce = setTimeout(() => void library.load(t, f.trim()), 180);
    return () => clearTimeout(debounce);
  });

  function playAll() {
    if (isScope(tab)) library.playAll();
  }
  function enqueueAll() {
    if (isScope(tab)) library.enqueueAll();
  }
</script>

<div class="library">
  <header>
    <div class="tabs" role="tablist" aria-label={t('library.tabsLabel')}>
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
        placeholder={t('library.filterPlaceholder')}
        bind:value={filter}
        aria-label={t('library.filterLabel')}
      />
      {#if isScope(tab)}
        <button class="act" onclick={playAll}>▶ {t('library.playAll')}</button>
        <button class="act" onclick={enqueueAll}>+ {t('library.enqueueAll')}</button>
      {/if}
    </div>
  </header>

  <div class="body">
    {#if tab === 'downloads'}
      {#if downloads.items.length === 0}
        <div class="center"><p class="hint">{EMPTY_BODY.downloads}</p></div>
      {:else}
        <div class="list" role="list">
          {#each downloads.items as d (d.video_id)}
            <div class="drow" class:failed={d.state === 'failed'} role="listitem">
              <span class="dtitle">{d.title}</span>
              <span class="dstate mono">
                {#if d.state === 'running'}⬇ {d.pct}%
                {:else if d.state === 'done'}✓ {t('library.done')}
                {:else}⚠ {d.error}{/if}
              </span>
              {#if d.state === 'failed'}
                <button class="ri" title={t('common.retry')} onclick={() => downloads.retry(d)}
                  >↻</button
                >
              {/if}
              <button
                class="ri"
                title={t('library.deleteDownload')}
                onclick={() => downloads.remove(d, true)}>✕</button
              >
            </div>
          {/each}
        </div>
      {/if}
    {:else if tab === 'playlists'}
      <PlaylistsPane {ctx} {filter} />
    {:else if library.loading && library.tracks.length === 0}
      <div class="center"><p class="hint">{t('library.loading')}</p></div>
    {:else if library.empty}
      <div class="center"><p class="hint">{EMPTY_BODY[tab]}</p></div>
    {:else}
      <div class="list" role="list">
        {#each library.tracks as track, i (`${track.video_id}:${i}`)}
          <TrackRow {track} index={i + 1} ondblclick={() => library.play(track)}>
            {#snippet actions()}
              <button
                class="ri"
                title={t('library.download')}
                onclick={() => downloads.download(track)}>⬇</button
              >
              <button
                class="ri"
                title={t('library.addToQueue')}
                onclick={() => library.enqueue(track)}>＋</button
              >
              <button
                class="ri"
                title={t('library.addToPlaylist')}
                onclick={() => playlists.beginAdd(track)}>≡</button
              >
              {#if removable(tab)}
                <button class="ri" title={t('common.remove')} onclick={() => library.remove(track)}
                  >✕</button
                >
              {/if}
            {/snippet}
          </TrackRow>
        {/each}
        {#if library.hasMore}
          <button class="more" onclick={() => void library.more()}>
            {t('library.loadMore', { n: library.total - library.tracks.length })}
          </button>
        {/if}
      </div>
    {/if}
  </div>

  <footer class="foot">
    {#if tab === 'downloads'}
      <span class="count mono"
        >{t('library.downloadsCount', {
          active: downloads.active,
          total: downloads.items.length,
        })}</span
      >
    {:else if isScope(tab)}
      <span class="count mono">{t('library.tracksCount', { n: library.total })}</span>
    {:else}
      <span class="count mono">{t('library.playlistsCount', { n: playlists.list.length })}</span>
    {/if}
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
    min-height: 0;
    overflow-y: auto;
  }
  .center {
    display: grid;
    place-items: center;
    height: 100%;
  }
  .hint {
    max-width: 46ch;
    margin: 0;
    text-align: center;
    color: var(--role-text-subtle);
    font-size: 13px;
    line-height: 1.6;
  }
  .list {
    display: flex;
    flex-direction: column;
  }
  .drow {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    height: 44px;
    padding: 0 var(--space-2);
    border-bottom: 1px solid var(--role-border-muted);
  }
  .dtitle {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 13px;
  }
  .dstate {
    font-size: 11.5px;
    color: var(--role-text-subtle);
  }
  .drow.failed .dstate {
    color: var(--role-text-warning, var(--role-accent-alt));
  }
  .ri {
    border: none;
    background: transparent;
    color: var(--role-text-subtle);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
    font-size: 13px;
    line-height: 1;
  }
  .ri:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .more {
    margin: var(--space-3) auto;
    padding: var(--space-2) var(--space-5);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .more:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .foot {
    display: flex;
    justify-content: flex-end;
  }
  .count {
    font-size: 11px;
    color: var(--role-text-subtle);
  }
</style>
