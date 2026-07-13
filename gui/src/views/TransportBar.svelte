<script lang="ts">
  // The persistent bottom transport bar (docs/gui/07 §0): absorbs the TUI status line's
  // density — state, N/M queue, shuffle/repeat/speed/EQ/streaming chips — so playback
  // control never requires navigating to Now Playing.
  import type { AppCtx } from '../lib/ctx';
  import { fmtSpeed } from '../lib/format';
  import AlbumArt from '../lib/components/AlbumArt.svelte';
  import SeekBar from '../lib/components/SeekBar.svelte';
  import VolumeBar from '../lib/components/VolumeBar.svelte';
  import { t } from '../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { playback, queue, ui, connection, downloads } = ctx;

  const track = $derived(playback.track);
  const model = $derived(playback.model);
  const disabled = $derived(!connection.usable);
</script>

<footer class="bar">
  <button
    class="meta"
    onclick={() => ui.setView('now')}
    title={t('transport.openNowPlaying')}
    disabled={track == null}
  >
    <AlbumArt {track} size="44px" />
    <span class="meta-text">
      {#if track}
        <span class="t">{track.display_title ?? track.title}</span>
        <span class="a">{track.display_artist ?? track.artist}</span>
      {:else}
        <span class="t idle">{t('transport.nothingPlaying')}</span>
        <span class="a">{t('transport.idleHint')}</span>
      {/if}
    </span>
  </button>

  <div class="controls">
    <button class="ctl" onclick={() => playback.prev()} {disabled} title={t('transport.previous')}
      >⏮</button
    >
    <button
      class="ctl play"
      onclick={() => playback.togglePause()}
      {disabled}
      title={t('transport.playPause')}
    >
      {playback.paused ? '▶' : '⏸'}
    </button>
    <button class="ctl" onclick={() => playback.next()} {disabled} title={t('transport.next')}
      >⏭</button
    >
  </div>

  <div class="seek">
    <SeekBar
      mediaKey={track ? `${track.video_id}:${model?.position_epoch ?? 0}` : null}
      positionMs={playback.positionMs}
      durationMs={playback.durationMs}
      live={playback.live}
      disabled={disabled || track == null}
      onseek={(ms) => playback.seekTo(ms)}
    />
  </div>

  <div class="chips">
    {#if model}
      <button
        class="chip mono"
        onclick={() => ui.toggleQueue()}
        title={t('transport.queuePosition')}
      >
        {model.queue_len === 0 ? '–/–' : `${model.queue_pos + 1}/${model.queue_len}`}
      </button>
      <button
        class="chip"
        class:on={model.shuffle}
        onclick={() => playback.toggleShuffle()}
        {disabled}
        title={t('transport.shuffle')}>⇄</button
      >
      <button
        class="chip"
        class:on={model.repeat !== 'off'}
        onclick={() => playback.cycleRepeat()}
        {disabled}
        title={t('transport.repeatMode', { mode: t(`repeat.${model.repeat}`) })}
        >{model.repeat === 'one' ? '🔂' : '🔁'}</button
      >
      {#if fmtSpeed(model.speed_tenths)}
        <span class="chip mono passive" title={t('transport.playbackSpeed')}
          >{fmtSpeed(model.speed_tenths)}</span
        >
      {/if}
      {#if model.eq.preset !== 'flat'}
        <button
          class="chip mono on"
          onclick={() => {
            ui.setView('settings');
            ui.settingsTab = 'playback';
          }}
          title={t('transport.eqPreset')}>EQ:{model.eq.preset}</button
        >
      {/if}
      {#if model.streaming}
        <span class="chip on passive" title={t('transport.autoplayOn')}>✦</span>
      {/if}
    {/if}
    {#if downloads.active > 0}
      <button
        class="chip on mono"
        onclick={() => {
          ui.setView('library');
          ui.libraryTab = 'downloads';
        }}
        title={t('transport.downloadsInProgress')}>⬇ {downloads.active}</button
      >
    {/if}
  </div>

  <VolumeBar
    volume={playback.volume}
    {disabled}
    onvolume={(v) => playback.setVolume(v)}
    onvolumeend={() => playback.flushVolume()}
  />

  <button
    class="queue-btn"
    class:on={ui.queueOpen}
    onclick={() => ui.toggleQueue()}
    title={t('transport.toggleQueue')}>☰ {queue.items.length}</button
  >
</footer>

<style>
  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-4);
    height: var(--transport-h);
    padding: 0 var(--space-4);
    background: var(--surface-1);
    border-top: 1px solid var(--role-border-muted);
  }
  .meta {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    /* Shrinkable: at narrow windows the track meta gives way first so the volume and
       queue controls never clip off the right edge. */
    flex: 0 1 220px;
    width: 220px;
    min-width: 140px;
    border: none;
    background: transparent;
    padding: 0;
    text-align: left;
  }
  .meta:disabled {
    cursor: default;
  }
  .meta-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
    gap: 2px;
  }
  .t {
    font-size: 12.5px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .t.idle {
    color: var(--role-text-muted);
  }
  .a {
    font-size: 11px;
    color: var(--role-text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .controls {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex: none;
  }
  .ctl {
    width: 34px;
    height: 34px;
    border: none;
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-player-control);
    font-size: 14px;
    display: grid;
    place-items: center;
  }
  .ctl:hover:not(:disabled) {
    background: var(--surface-2);
  }
  .ctl:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .ctl.play {
    width: 42px;
    height: 42px;
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-size: 16px;
  }
  .ctl.play:hover:not(:disabled) {
    background: var(--role-accent-alt);
  }
  .seek {
    flex: 1;
    min-width: 120px;
  }
  .chips {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex: 0 1 auto;
    min-width: 0;
    overflow: hidden;
  }
  .chip {
    padding: 3px 10px;
    border: 1px solid transparent;
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-player-label);
    font-size: 11px;
    line-height: 18px;
  }
  .chip.mono {
    font-family: var(--font-mono);
  }
  .chip.on {
    color: var(--role-accent);
    border-color: var(--role-border-muted);
    background: var(--surface-2);
  }
  .chip.passive {
    cursor: default;
  }
  button.chip:hover:not(.passive):not(:disabled) {
    border-color: var(--role-border-primary);
    color: var(--role-text-primary);
  }
  .chip:disabled {
    opacity: 0.4;
  }
  .queue-btn {
    flex: none;
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 11px;
    font-family: var(--font-mono);
  }
  .queue-btn.on,
  .queue-btn:hover {
    color: var(--role-text-primary);
    background: var(--surface-2);
  }
</style>
