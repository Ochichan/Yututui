<script lang="ts">
  // Library → Playlists (docs/gui/07 §5): the playlist list, a drill-down of one playlist's
  // tracks, and the three CRUD dialogs. All state lives in the playlists store, so the outer
  // LibraryView header and a library track row's "add to playlist" affordance can drive the
  // same modals. Wired via stores/playlists.svelte.ts — was the library.playlists seam.
  import type { AppCtx } from '../../lib/ctx';
  import Modal from '../../lib/components/Modal.svelte';
  import TrackRow from '../../lib/components/TrackRow.svelte';

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
        <button class="back" onclick={() => playlists.closeDetail()}>← Playlists</button>
        <div class="dmeta">
          <h3>{d.name}</h3>
          <span class="sub mono">{d.tracks.length} tracks</span>
        </div>
        <button class="act" disabled={d.tracks.length === 0} onclick={() => playlists.play(d.id)}
          >▶ Play all</button
        >
      </div>
      {#if d.tracks.length === 0}
        <p class="hint">This playlist is empty — add tracks from the library or search.</p>
      {:else}
        <div class="list" role="list">
          {#each d.tracks as t, i (`${t.video_id}:${i}`)}
            <TrackRow track={t} index={i + 1} ondblclick={() => library.play(t)}>
              {#snippet actions()}
                <button class="ri" title="Add to queue" onclick={() => library.enqueue(t)}
                  >＋</button
                >
                <button
                  class="ri"
                  title="Remove from playlist"
                  onclick={() => playlists.removeTrack(d.id, t.video_id)}>✕</button
                >
              {/snippet}
            </TrackRow>
          {/each}
        </div>
      {/if}
    </div>
  {:else}
    <div class="toolbar">
      <button class="act primary" onclick={() => playlists.beginCreate()}>＋ New playlist</button>
    </div>
    {#if shown.length === 0}
      <p class="hint">
        {playlists.list.length === 0
          ? 'No playlists yet — “＋ New playlist” starts one.'
          : 'No playlist matches the filter.'}
      </p>
    {:else}
      <div class="plist" role="list">
        {#each shown as p (p.id)}
          <div class="prow" role="listitem">
            <button class="popen" onclick={() => void playlists.open(p.id)}>
              <span class="pname">{p.name}</span>
              <span class="pcount mono">{p.count} tracks</span>
              {#if p.description}<span class="pdesc">{p.description}</span>{/if}
            </button>
            <button class="ri" title="Play" onclick={() => playlists.play(p.id)}>▶</button>
            <button class="ri" title="Delete playlist" onclick={() => playlists.beginDelete(p)}
              >✕</button
            >
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</div>

<!-- Create -->
{#if playlists.createOpen}
  <Modal title="New playlist" width="420px" onclose={() => playlists.cancelCreate()}>
    <form
      class="form"
      onsubmit={(e) => {
        e.preventDefault();
        submitCreate();
      }}
    >
      <label class="fl" for="pl-name">Name</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="pl-name"
        class="ti"
        bind:value={newName}
        placeholder="Late-night coding"
        autofocus
      />
      <div class="frow">
        <button type="button" class="btn" onclick={() => playlists.cancelCreate()}>Cancel</button>
        <button type="submit" class="btn primary" disabled={!newName.trim()}>Create</button>
      </div>
    </form>
  </Modal>
{/if}

<!-- Delete confirm -->
{#if playlists.deleteTarget}
  {@const target = playlists.deleteTarget}
  <Modal title="Delete playlist" width="420px" onclose={() => playlists.cancelDelete()}>
    <p class="confirm">
      Delete <strong>{target.name}</strong> ({target.count} tracks)? This can't be undone.
    </p>
    <div class="frow">
      <button class="btn" onclick={() => playlists.cancelDelete()}>Cancel</button>
      <button class="btn danger" onclick={() => playlists.confirmDelete()}>Delete</button>
    </div>
  </Modal>
{/if}

<!-- Add to playlist -->
{#if playlists.addTarget}
  {@const track = playlists.addTarget}
  <Modal title="Add to playlist" width="440px" onclose={() => playlists.cancelAdd()}>
    <p class="confirm">
      Add <strong>{track.display_title ?? track.title}</strong> to…
    </p>
    {#if playlists.list.length === 0}
      <p class="hint">No playlists yet — create one first.</p>
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
      <button class="btn" onclick={() => playlists.cancelAdd()}>Cancel</button>
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
