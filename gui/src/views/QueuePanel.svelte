<script lang="ts">
  // The collapsible right queue dock (docs/gui/07 §2). Rows are TrackRow in a
  // VirtualList; the current row derives from player.queue_pos (the queue topic carries
  // items only). Jump/remove/clear are live; drag-reorder is a pending wire.
  import type { AppCtx } from '../lib/ctx';
  import { fmtTime } from '../lib/format';
  import VirtualList from '../lib/components/VirtualList.svelte';
  import TrackRow from '../lib/components/TrackRow.svelte';
  import WireTag from '../lib/components/WireTag.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { queue, playback, ui, wip } = ctx;

  const currentPos = $derived(playback.model?.queue_pos ?? -1);
  const totalMs = $derived(queue.items.reduce((n, t) => n + (t.duration_ms ?? 0), 0));

  function grip() {
    // TODO(wire:M2/queue.reorder): replace with pointer-drag once QueueMove is wired.
    wip.open('queue.reorder');
  }

  // TODO(wire:M4/ai.whygem): autoplay-added rows grow a "why?" affordance here, anchored
  // to the WhyGem popover (docs/gui/07 §13).
</script>

<aside class="dock" aria-label="Queue" data-kctx="Queue">
  <header>
    <h2>Queue</h2>
    <div class="head-actions">
      <button
        class="ha"
        onclick={() => queue.clearUpcoming()}
        title="Clear upcoming tracks"
        disabled={queue.items.length === 0}>⌫</button
      >
      <button class="ha" onclick={() => ui.toggleQueue()} title="Close queue panel">✕</button>
    </div>
  </header>

  <div class="list">
    <VirtualList items={queue.items} rowHeight={52}>
      {#snippet row(track, i)}
        <TrackRow {track} index={i + 1} current={i === currentPos} ondblclick={() => queue.play(i)}>
          {#snippet actions()}
            <button class="ra" onclick={() => queue.play(i)} title="Play from here">▶</button>
            <button class="ra" onclick={grip} title="Drag to reorder">⠿</button>
            <button class="ra" onclick={() => queue.remove(i)} title="Remove">✕</button>
          {/snippet}
        </TrackRow>
      {/snippet}
      {#snippet empty()}
        <div class="empty">
          <p class="kaomoji mono">=^..^=</p>
          <p>queue is napping…</p>
        </div>
      {/snippet}
    </VirtualList>
  </div>

  <footer>
    <span class="mono">{queue.items.length} tracks · {fmtTime(totalMs)}</span>
    <WireTag id="queue.reorder" {wip} />
  </footer>
</aside>

<style>
  .dock {
    display: flex;
    flex-direction: column;
    width: 300px;
    background: var(--surface-1);
    border-left: 1px solid var(--role-border-muted);
    min-height: 0;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-3) var(--space-3) var(--space-2);
  }
  h2 {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--role-text-muted);
  }
  .head-actions {
    display: flex;
    gap: var(--space-1);
  }
  .ha {
    border: none;
    background: transparent;
    color: var(--role-text-subtle);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
    font-size: 12px;
  }
  .ha:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .ha:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .list {
    flex: 1;
    min-height: 0;
    padding: 0 var(--space-2);
  }
  .empty {
    padding: var(--space-8) var(--space-4);
    text-align: center;
    color: var(--role-text-subtle);
    font-size: 12px;
  }
  .kaomoji {
    font-size: 18px;
    margin: 0 0 var(--space-2);
  }
  .mono {
    font-family: var(--font-mono);
  }
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-top: 1px solid var(--role-border-muted);
    font-size: 10.5px;
    color: var(--role-text-subtle);
  }
  .ra {
    border: none;
    background: transparent;
    color: inherit;
    font-size: 11px;
    padding: 2px 4px;
    border-radius: var(--radius-s);
  }
  .ra:hover {
    background: var(--surface-2);
  }
</style>
