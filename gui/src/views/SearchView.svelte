<script lang="ts">
  // Search (docs/gui/07 §3): query input + the 6-catalog source chip row + results list.
  // The execution wire (ticketed RunSearch → search topic) is pending; the whole results
  // surface goes through the patch-bay gate until then.
  import type { AppCtx } from '../lib/ctx';
  import type { SearchSource } from '../generated/protocol/SearchSource';
  import PendingSurface from '../lib/components/PendingSurface.svelte';
  import WireTag from '../lib/components/WireTag.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip } = ctx;

  const SOURCES: Array<{ id: SearchSource; label: string }> = [
    { id: 'all', label: 'All' },
    { id: 'youtube', label: 'YTM' },
    { id: 'sound_cloud', label: 'SoundCloud' },
    { id: 'audius', label: 'Audius' },
    { id: 'jamendo', label: 'Jamendo' },
    { id: 'internet_archive', label: 'Internet Archive' },
    { id: 'radio_browser', label: 'Radio Browser' },
  ];

  let query = $state('');
  let source = $state<SearchSource>('youtube');

  function run() {
    if (query.trim().length === 0) return;
    // TODO(wire:M2/search.run): replace with search.svelte.ts run(ticket, query, source)
    // once the ticketed wire lands; the gate auto-opens on the `search-v8` capability.
    wip.gate('search.run');
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
    <PendingSurface
      id="search.run"
      {wip}
      glyph="⌕"
      body="Results from YTM, SoundCloud, Audius, Jamendo, Internet Archive, and Radio Browser will list here — Enter or double-click plays, the + button enqueues, station rows get badges."
    />
  </div>

  <footer class="foot">
    <WireTag id="search.run" {wip} />
  </footer>
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
    display: grid;
    place-items: center;
    min-height: 0;
  }
  .foot {
    display: flex;
    justify-content: flex-end;
  }
</style>
