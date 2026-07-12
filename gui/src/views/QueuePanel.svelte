<script lang="ts">
  // The collapsible right queue dock (docs/gui/07 §2). Rows are TrackRow in a
  // VirtualList; the current row derives from player.queue_pos (the queue topic carries
  // items only). Jump/remove/clear/drag-reorder are live; autoplay picks grow a why? popover.
  import type { AppCtx } from '../lib/ctx';
  import { fmtTime } from '../lib/format';
  import { t } from '../lib/i18n.svelte';
  import VirtualList from '../lib/components/VirtualList.svelte';
  import TrackRow from '../lib/components/TrackRow.svelte';
  import WhyGemPopover from '../lib/components/WhyGemPopover.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { queue, playback, ui, whygem } = ctx;

  const currentPos = $derived(playback.model?.queue_pos ?? -1);
  const totalMs = $derived(queue.items.reduce((n, track) => n + (track.duration_ms ?? 0), 0));

  /** Open the Why-Gem popover anchored to the affordance's bottom-left corner. */
  function whyPick(videoId: string, e: MouseEvent) {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    whygem.open(videoId, { x: r.left, y: r.bottom });
  }
</script>

<aside class="dock" aria-label={t('queue.title')} data-kctx="Queue">
  <header>
    <h2>{t('queue.title')}</h2>
    <div class="head-actions">
      <button
        class="ha"
        onclick={() => queue.clearUpcoming()}
        title={t('queue.clearUpcoming')}
        disabled={queue.items.length === 0}>⌫</button
      >
      <button class="ha" onclick={() => ui.toggleQueue()} title={t('queue.close')}>✕</button>
    </div>
  </header>

  <div class="list">
    <VirtualList
      items={queue.items}
      rowHeight={52}
      reorder={(from, to) => queue.move(from, to)}
      snapshotKey="queue-list"
    >
      {#snippet row(track, i)}
        <TrackRow {track} index={i + 1} current={i === currentPos} ondblclick={() => queue.play(i)}>
          {#snippet actions()}
            <button class="ra" onclick={() => queue.play(i)} title={t('queue.playFromHere')}
              >▶</button
            >
            {#if whygem.has(track.video_id)}
              <button
                class="ra gem"
                onclick={(e) => whyPick(track.video_id, e)}
                title={t('whygem.affordance')}>?</button
              >
            {/if}
            <button class="ra grip" data-drag-handle title={t('queue.dragReorder')}>⠿</button>
            <button
              class="ra"
              onclick={() => queue.remove(i)}
              title={t('common.remove')}
              aria-label={`${t('common.remove')}: ${track.display_title ?? track.title}`}>✕</button
            >
          {/snippet}
        </TrackRow>
      {/snippet}
      {#snippet empty()}
        <div class="empty">
          <p class="kaomoji mono">=^..^=</p>
          <p>{t('queue.napping')}</p>
        </div>
      {/snippet}
    </VirtualList>
  </div>

  <footer>
    <span class="mono">{t('queue.summary', { n: queue.items.length, time: fmtTime(totalMs) })}</span
    >
  </footer>
</aside>

<WhyGemPopover {whygem} />

<style>
  .dock {
    display: flex;
    flex-direction: column;
    width: 320px;
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
    padding: 4px 6px;
    border-radius: var(--radius-s);
  }
  .ra:hover {
    background: var(--surface-2);
  }
  .ra.gem {
    color: var(--role-accent);
    font-weight: 700;
  }
  .ra.grip {
    cursor: grab;
    touch-action: none;
  }
  .ra.grip:active {
    cursor: grabbing;
  }
</style>
