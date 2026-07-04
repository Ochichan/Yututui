<script lang="ts">
  // The shared track row for queue / search / library lists (docs/gui/05 §2). Fixed-height
  // by contract with VirtualList. Right-side actions come from the caller as a snippet;
  // context menus (replacing TUI right-click semantics) land with their features.
  import type { Snippet } from 'svelte';
  import type { TrackModel } from '../../generated/protocol/TrackModel';
  import { fmtTime } from '../format';
  import { t } from '../i18n.svelte';
  import AlbumArt from './AlbumArt.svelte';

  interface Props {
    track: TrackModel;
    /** 1-based display index; omitted ⇒ art thumbnail instead. */
    index?: number;
    /** This row is the playing cursor (derived from player.queue_pos). */
    current?: boolean;
    showArt?: boolean;
    ondblclick?: () => void;
    actions?: Snippet;
  }
  const { track, index, current = false, showArt = true, ondblclick, actions }: Props = $props();

  const title = $derived(track.display_title ?? track.title);
  const artist = $derived(track.display_artist ?? track.artist);
</script>

<div class="row" class:current {ondblclick} role="row" tabindex="-1">
  {#if current}
    <span class="vu" aria-label={t('track.playing')}>
      <i></i><i></i><i></i>
    </span>
  {:else if index != null}
    <span class="idx mono">{index}</span>
  {:else}
    <span class="idx"></span>
  {/if}
  {#if showArt}
    <AlbumArt {track} size="32px" radius="var(--radius-s)" />
  {/if}
  <div class="text">
    <div class="title">
      {title}
      {#if track.favorite}<span class="heart" title={t('track.inFavorites')}>♥</span>{/if}
      {#if track.downloaded}<span class="dl" title={t('track.downloaded')}>⬇</span>{/if}
      {#if track.duration_ms == null}<span class="live-badge">{t('track.live')}</span>{/if}
    </div>
    <div class="artist">
      {artist}{#if track.album}&nbsp;·&nbsp;{track.album}{/if}
    </div>
  </div>
  <span class="dur mono">{track.duration_ms == null ? '' : fmtTime(track.duration_ms)}</span>
  {#if actions}
    <div class="actions">{@render actions()}</div>
  {/if}
</div>

<style>
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    height: 100%;
    padding: 0 var(--space-3);
    border-radius: var(--radius-s);
    user-select: none;
  }
  .row:hover {
    background: var(--surface-2);
  }
  .row.current {
    background: var(--role-selection-bg);
    color: var(--role-selection-fg);
  }
  .row.current .artist,
  .row.current .dur {
    color: var(--role-selection-fg);
    opacity: 0.75;
  }
  .idx {
    flex: none;
    width: 22px;
    text-align: right;
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .mono {
    font-family: var(--font-mono);
  }
  .vu {
    flex: none;
    width: 22px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    gap: 2px;
    height: 14px;
  }
  .vu i {
    width: 3px;
    border-radius: 1px;
    background: currentColor;
    animation: vu 900ms ease-in-out infinite;
    height: 40%;
  }
  .vu i:nth-child(2) {
    animation-delay: 150ms;
    height: 90%;
  }
  .vu i:nth-child(3) {
    animation-delay: 320ms;
    height: 60%;
  }
  @keyframes vu {
    0%,
    100% {
      transform: scaleY(0.5);
    }
    50% {
      transform: scaleY(1);
    }
  }
  .text {
    flex: 1;
    min-width: 0;
  }
  .title {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .heart {
    color: var(--role-accent-alt);
    font-size: 11px;
  }
  .dl {
    color: var(--role-success);
    font-size: 10px;
  }
  .live-badge {
    padding: 0 5px;
    border-radius: var(--radius-pill);
    background: var(--role-error);
    color: var(--role-text-inverse);
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.06em;
  }
  .artist {
    font-size: 11px;
    color: var(--role-text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dur {
    flex: none;
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .actions {
    flex: none;
    display: flex;
    gap: var(--space-1);
    opacity: 0;
    transition: opacity 120ms ease;
  }
  .row:hover .actions,
  .row:focus-within .actions {
    opacity: 1;
  }
</style>
