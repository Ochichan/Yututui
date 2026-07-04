<script lang="ts">
  // Library → Playlists (docs/gui/07 §5): the playlist list, a drill-down of one playlist's
  // tracks, and the three CRUD dialogs. All state lives in the playlists store, so the outer
  // LibraryView header and a library track row's "add to playlist" affordance can drive the
  // same modals. Wired via stores/playlists.svelte.ts — was the library.playlists seam.
  import type { AppCtx } from '../../lib/ctx';
  import Modal from '../../lib/components/Modal.svelte';
  import TrackRow from '../../lib/components/TrackRow.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
    /** The outer filter box — narrows the playlist list by name. */
    filter?: string;
  }
  const { ctx, filter = '' }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores are reactive
  const { playlists, library } = ctx;

  const shown = $derived(
    filter.trim()
      ? playlists.list.filter((p) => p.name.toLowerCase().includes(filter.trim().toLowerCase()))
      : playlists.list,
  );

  let newName = $state('');
  function submitCreate() {
    playlists.submitCreate(newName);
    newName = '';
  }
</script>

<div class="pane">
  {#if playlists.detail}
    {@const d = playlists.detail}
    <div class="detail">
      <div class="dhead">
        <button class="back" onclick={() => playlists.closeDetail()}
          >← {t('playlists.backToList')}</button
        >
        <div class="dmeta">
          <h3>{d.name}</h3>
          <span class="sub mono">{t('playlists.trackCount', { count: d.tracks.length })}</span>
        </div>
        <button class="act" disabled={d.tracks.length === 0} onclick={() => playlists.play(d.id)}
          >▶ {t('playlists.playAll')}</button
        >
      </div>
      {#if d.tracks.length === 0}
        <p class="hint">{t('playlists.emptyDetail')}</p>
      {:else}
        <div class="list" role="list">
          {#each d.tracks as tr, i (`${tr.video_id}:${i}`)}
            <TrackRow track={tr} index={i + 1} ondblclick={() => library.play(tr)}>
              {#snippet actions()}
                <button
                  class="ri"
                  title={t('playlists.addToQueue')}
                  onclick={() => library.enqueue(tr)}>＋</button
                >
                <button
                  class="ri"
                  title={t('playlists.removeFromPlaylist')}
                  onclick={() => playlists.removeTrack(d.id, tr.video_id)}>✕</button
                >
              {/snippet}
            </TrackRow>
          {/each}
        </div>
      {/if}
    </div>
  {:else}
    <div class="toolbar">
      <button class="act primary" onclick={() => playlists.beginCreate()}
        >＋ {t('playlists.newPlaylist')}</button
      >
    </div>
    {#if shown.length === 0}
      <p class="hint">
        {playlists.list.length === 0 ? t('playlists.emptyNone') : t('playlists.emptyFiltered')}
      </p>
    {:else}
      <div class="plist" role="list">
        {#each shown as p (p.id)}
          <div class="prow" role="listitem">
            <button class="popen" onclick={() => void playlists.open(p.id)}>
              <span class="pname">{p.name}</span>
              <span class="pcount mono">{t('playlists.trackCount', { count: p.count })}</span>
              {#if p.description}<span class="pdesc">{p.description}</span>{/if}
            </button>
            <button class="ri" title={t('playlists.play')} onclick={() => playlists.play(p.id)}
              >▶</button
            >
            <button
              class="ri"
              title={t('playlists.deletePlaylist')}
              onclick={() => playlists.beginDelete(p)}>✕</button
            >
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</div>

<!-- Create -->
{#if playlists.createOpen}
  <Modal title={t('playlists.newPlaylist')} width="420px" onclose={() => playlists.cancelCreate()}>
    <form
      class="form"
      onsubmit={(e) => {
        e.preventDefault();
        submitCreate();
      }}
    >
      <label class="fl" for="pl-name">{t('playlists.name')}</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="pl-name"
        class="ti"
        bind:value={newName}
        placeholder={t('playlists.namePlaceholder')}
        autofocus
      />
      <div class="frow">
        <button type="button" class="btn" onclick={() => playlists.cancelCreate()}
          >{t('common.cancel')}</button
        >
        <button type="submit" class="btn primary" disabled={!newName.trim()}
          >{t('playlists.create')}</button
        >
      </div>
    </form>
  </Modal>
{/if}

<!-- Delete confirm -->
{#if playlists.deleteTarget}
  {@const target = playlists.deleteTarget}
  <Modal
    title={t('playlists.deletePlaylist')}
    width="420px"
    onclose={() => playlists.cancelDelete()}
  >
    <p class="confirm">
      {t('playlists.deleteConfirm', { name: target.name, count: target.count })}
    </p>
    <div class="frow">
      <button class="btn" onclick={() => playlists.cancelDelete()}>{t('common.cancel')}</button>
      <button class="btn danger" onclick={() => playlists.confirmDelete()}
        >{t('common.delete')}</button
      >
    </div>
  </Modal>
{/if}

<!-- Add to playlist -->
{#if playlists.addTarget}
  {@const track = playlists.addTarget}
  <Modal title={t('playlists.addToPlaylist')} width="440px" onclose={() => playlists.cancelAdd()}>
    <p class="confirm">
      {t('playlists.addTrackTo', { name: track.display_title ?? track.title })}
    </p>
    {#if playlists.list.length === 0}
      <p class="hint">{t('playlists.emptyNoneAdd')}</p>
    {:else}
      <div class="picklist" role="list">
        {#each playlists.list as p (p.id)}
          <button class="pick" onclick={() => playlists.addTo(p.id)}>
            <span class="pname">{p.name}</span>
            <span class="pcount mono">{p.count}</span>
          </button>
        {/each}
      </div>
    {/if}
    <div class="frow">
      <button class="btn" onclick={() => playlists.cancelAdd()}>{t('common.cancel')}</button>
    </div>
  </Modal>
{/if}

<style>
  .pane {
    height: 100%;
  }
  .toolbar {
    display: flex;
    justify-content: flex-end;
    margin-bottom: var(--space-3);
  }
  .plist,
  .list {
    display: flex;
    flex-direction: column;
  }
  .prow {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    border-bottom: 1px solid var(--role-border-muted);
  }
  .popen {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-2);
    border: none;
    background: transparent;
    color: var(--role-text-primary);
    text-align: left;
  }
  .popen:hover {
    background: var(--surface-2);
  }
  .pname {
    font-size: 13.5px;
    font-weight: 500;
  }
  .pcount {
    font-size: 11px;
    color: var(--role-text-subtle);
    flex: none;
  }
  .pdesc {
    font-size: 11.5px;
    color: var(--role-text-subtle);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .detail {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .dhead {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }
  .back {
    border: none;
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
  }
  .back:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .dmeta {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
  }
  .dmeta h3 {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
  }
  .sub {
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .act {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .act:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .act:disabled {
    opacity: 0.4;
  }
  .act.primary {
    border-color: var(--role-border-primary);
    color: var(--role-text-primary);
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
  .hint {
    max-width: 46ch;
    margin: var(--space-6) auto;
    text-align: center;
    color: var(--role-text-subtle);
    font-size: 13px;
    line-height: 1.6;
  }

  /* modals */
  .form {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .fl {
    font-size: 12px;
    color: var(--role-text-muted);
  }
  .confirm {
    margin: 0 0 var(--space-4);
    font-size: 13px;
    line-height: 1.55;
    color: var(--role-text-primary);
  }
  .frow {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .btn {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .btn:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .btn:disabled {
    opacity: 0.4;
  }
  .btn.primary {
    border-color: var(--role-accent);
    color: var(--role-accent);
  }
  .btn.danger {
    border-color: var(--role-error);
    color: var(--role-error);
  }
  .picklist {
    display: flex;
    flex-direction: column;
    max-height: 240px;
    overflow-y: auto;
  }
  .pick {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: none;
    border-bottom: 1px solid var(--role-border-muted);
    background: transparent;
    color: var(--role-text-primary);
    font-size: 13px;
    text-align: left;
  }
  .pick:hover {
    background: var(--surface-2);
  }
  .mono {
    font-family: var(--font-mono);
  }
</style>
