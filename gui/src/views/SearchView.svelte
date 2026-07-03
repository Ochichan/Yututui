<script lang="ts">
  // Search (docs/gui/07 §3): query input + the 6-catalog source chip row + results list.
  // Ticketed execution lives in search.svelte.ts; this view just drives it and renders the
  // per-source groups (each with a play-on-double-click row and a + enqueue action).
  import type { AppCtx } from '../lib/ctx';
  import type { SearchSource } from '../generated/protocol/SearchSource';
  import TrackRow from '../lib/components/TrackRow.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { search } = ctx;

  const SOURCES: Array<{ id: SearchSource; label: string }> = [
    { id: 'all', label: 'All' },
    { id: 'youtube', label: 'YTM' },
    { id: 'sound_cloud', label: 'SoundCloud' },
    { id: 'audius', label: 'Audius' },
    { id: 'jamendo', label: 'Jamendo' },
    { id: 'internet_archive', label: 'Internet Archive' },
    { id: 'radio_browser', label: 'Radio Browser' },
  ];
  const LABELS: Record<SearchSource, string> = Object.fromEntries(
    SOURCES.map((s) => [s.id, s.label]),
  ) as Record<SearchSource, string>;

  let query = $state('');
  let source = $state<SearchSource>('youtube');

  function run() {
    search.run(query, source);
  }
</script>

<div class="search">
  <header>
    <form
      class="query"
      onsubmit={(e) => {
        e.preventDefault();
        run();
      }}
    >
      <span class="glass" aria-hidden="true">⌕</span>
      <input
        class="ti"
        type="search"
        placeholder="Search songs, artists, stations…"
        bind:value={query}
        aria-label="Search query"
      />
      <button class="go" type="submit">Search</button>
    </form>
    <div class="chips" role="tablist" aria-label="Search source">
      {#each SOURCES as s (s.id)}
        <button
          class="chip"
          class:on={source === s.id}
          role="tab"
          aria-selected={source === s.id}
          onclick={() => (source = s.id)}>{s.label}</button
        >
      {/each}
    </div>
  </header>

  <div class="results">
    {#if search.pending}
      <p class="hint">Searching “{search.query}”…</p>
    {:else if !search.ran}
      <p class="hint">
        Results from YTM, SoundCloud, Audius, Jamendo, Internet Archive, and Radio Browser list here
        — double-click a row to play, the + button enqueues.
      </p>
    {:else if search.empty}
      <p class="hint">No results for “{search.query}”.</p>
    {:else}
      <div class="groups" role="list">
        {#each search.groups as g (g.source)}
          {#if g.tracks.length > 0 || g.error}
            <section class="group">
              {#if source === 'all' || g.error}
                <h3 class="ghead">
                  <span>{LABELS[g.source] ?? g.source}</span>
                  {#if g.error}<span class="err" title={g.error}>⚠ {g.error}</span>{/if}
                </h3>
              {/if}
              {#each g.tracks as t (t.video_id)}
                <TrackRow track={t} ondblclick={() => search.play(t)}>
                  {#snippet actions()}
                    <button class="enq" title="Add to queue" onclick={() => search.enqueue(t)}
                      >＋</button
                    >
                  {/snippet}
                </TrackRow>
              {/each}
            </section>
          {/if}
        {/each}
      </div>
    {/if}
  </div>
</div>

<style>
  .search {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: var(--space-6) var(--space-8);
    gap: var(--space-4);
  }
  .query {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0 var(--space-3);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-m);
    background: var(--surface-1);
  }
  .query:focus-within {
    border-color: var(--role-border-focused);
  }
  .glass {
    color: var(--role-text-subtle);
  }
  input {
    flex: 1;
    height: 40px;
    border: none;
    background: transparent;
    color: var(--role-text-primary);
    font: inherit;
    outline: none;
  }
  .go {
    border: none;
    border-radius: var(--radius-pill);
    padding: var(--space-1) var(--space-4);
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-size: 12px;
    font-weight: 600;
  }
  .go:hover {
    background: var(--role-accent-alt);
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
  }
  .chip {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 11.5px;
  }
  .chip:hover {
    background: var(--surface-2);
  }
  .chip.on {
    background: var(--role-accent);
    border-color: var(--role-accent);
    color: var(--role-text-inverse);
    font-weight: 600;
  }
  .results {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .hint {
    display: grid;
    place-items: center;
    height: 100%;
    max-width: 46ch;
    margin: 0 auto;
    text-align: center;
    color: var(--role-text-subtle);
    font-size: 13px;
    line-height: 1.6;
  }
  .groups {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .group {
    display: flex;
    flex-direction: column;
  }
  .ghead {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    margin: 0 0 var(--space-1);
    padding: 0 var(--space-2);
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--role-text-muted);
  }
  .err {
    text-transform: none;
    letter-spacing: 0;
    font-weight: 500;
    color: var(--role-text-warning, var(--role-accent-alt));
  }
  .enq {
    border: none;
    background: transparent;
    color: var(--role-text-subtle);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
    font-size: 15px;
    line-height: 1;
  }
  .enq:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
</style>
