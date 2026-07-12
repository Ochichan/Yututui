<script lang="ts">
  // Now Playing (docs/gui/07 §1): blurred backdrop, art card, title/artist with the
  // rating cycle, seek + transport, status chips, and the synced lyrics pane (click a
  // line to seek). Backdrop is a pre-blurred low-res copy, NOT backdrop-filter
  // (WKWebView performance — docs/gui/06 §4).
  import type { AppCtx } from '../lib/ctx';
  import { hueOf } from '../lib/format';
  import { t } from '../lib/i18n.svelte';
  import AlbumArt from '../lib/components/AlbumArt.svelte';
  import SeekBar from '../lib/components/SeekBar.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { playback, lyrics, ui, connection, toasts, client } = ctx;

  const track = $derived(playback.track);
  const model = $derived(playback.model);
  const disabled = $derived(!connection.usable);

  const artUrl = $derived(
    track?.artwork?.path != null ? `ytm://app/art/${encodeURIComponent(track.artwork.key)}` : null,
  );
  const hue = $derived(hueOf(track ? `${track.title}·${track.artist}` : 'yututui'));

  const rating = $derived(
    track == null ? 'none' : track.favorite ? 'up' : track.disliked ? 'down' : 'none',
  );

  const activeLine = $derived(lyrics.activeIndex(playback.positionMs));
  let lyricsPane = $state<HTMLElement | null>(null);
  $effect(() => {
    // Auto-scroll the active line to the pane's center.
    if (activeLine < 0 || !lyricsPane) return;
    const el = lyricsPane.querySelector<HTMLElement>(`[data-line="${activeLine}"]`);
    el?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  });

  async function video() {
    if (!track) return;
    const result = await client.cmd('play_video', { video_id: track.video_id });
    if (result.ok) toasts.show('info', t('np.videoRequested'));
  }
</script>

<div class="np">
  {#if track}
    <div class="backdrop" aria-hidden="true">
      {#if artUrl}
        <img src={artUrl} width="24" alt="" />
      {:else}
        <div
          class="backdrop-fill"
          style:background="radial-gradient(80% 100% at 30% 0%, hsl({hue} 45% 30%), transparent 70%),
          radial-gradient(70% 90% at 80% 100%, hsl({(hue + 60) % 360} 40% 24%), transparent 65%)"
        ></div>
      {/if}
      <div class="scrim"></div>
    </div>

    <div class="stage" class:with-lyrics={lyrics.lines.length > 0} data-ui-scroll-key="now-stage">
      <section class="main">
        <AlbumArt {track} size="min(300px, 32vw)" radius="var(--radius-l)" elevated />
        <div class="title-block">
          <h1>{track.display_title ?? track.title}</h1>
          <p class="artist">
            {track.display_artist ?? track.artist}{#if track.album}
              <span class="album">&nbsp;·&nbsp;{track.album}</span>{/if}
          </p>
        </div>

        <div class="seek-row">
          <SeekBar
            positionMs={playback.positionMs}
            durationMs={playback.durationMs}
            live={playback.live}
            {disabled}
            onseek={(ms) => playback.seekTo(ms)}
          />
        </div>
        {#if playback.live && model?.stream_now_playing}
          <p class="icy mono">{model.stream_now_playing}</p>
        {/if}

        <div class="transport">
          <button
            class="tp rate"
            class:up={rating === 'up'}
            class:down={rating === 'down'}
            onclick={() => playback.cycleRating()}
            disabled={disabled || !track}
            title={t('np.cycleRating')}
            >{rating === 'up' ? '👍' : rating === 'down' ? '👎' : '–'}</button
          >
          <button class="tp" onclick={() => playback.prev()} {disabled} title={t('np.previous')}
            >⏮</button
          >
          <button
            class="tp"
            onclick={() => playback.seekTo(Math.max(0, (playback.positionMs ?? 0) - 5000))}
            disabled={disabled || playback.live}
            title={t('np.seekBack')}>⏪</button
          >
          <button
            class="tp play"
            onclick={() => playback.togglePause()}
            {disabled}
            title={t('np.playPause')}
          >
            {playback.paused ? '▶' : '⏸'}
          </button>
          <button
            class="tp"
            onclick={() => playback.seekTo((playback.positionMs ?? 0) + 5000)}
            disabled={disabled || playback.live}
            title={t('np.seekForward')}>⏩</button
          >
          <button class="tp" onclick={() => playback.next()} {disabled} title={t('np.next')}
            >⏭</button
          >
          <button
            class="tp"
            onclick={video}
            disabled={disabled || track.watch_url == null}
            title={t('np.openVideo')}>🎬</button
          >
        </div>

        {#if model}
          <div class="chips">
            <button
              class="chip"
              class:on={model.shuffle}
              onclick={() => playback.toggleShuffle()}
              {disabled}
            >
              ⇄ {t('np.shuffle')}
            </button>
            <button
              class="chip"
              class:on={model.repeat !== 'off'}
              onclick={() => playback.cycleRepeat()}
              {disabled}
            >
              🔁 {t(`repeat.${model.repeat}`)}
            </button>
            <button
              class="chip"
              class:on={model.streaming}
              onclick={() =>
                void client.cmd('streaming', { state: model.streaming ? 'off' : 'on' })}
              {disabled}
            >
              ✦ {t('np.djGem')}
              {t(model.streaming ? 'common.on' : 'common.off')}
            </button>
            <button
              class="chip mono"
              onclick={() => {
                ui.setView('settings');
                ui.settingsTab = 'playback';
              }}
              title={t('np.openEq')}>EQ: {model.eq.preset}</button
            >
          </div>
        {/if}
      </section>

      {#if lyrics.lines.length > 0}
        <aside
          class="lyrics"
          bind:this={lyricsPane}
          aria-label={t('np.lyrics')}
          data-ui-scroll-key="now-lyrics"
        >
          {#each lyrics.lines as line, i (i)}
            <button
              class="line"
              class:active={i === activeLine}
              class:past={i < activeLine}
              data-line={i}
              onclick={() => line.ms != null && playback.seekTo(line.ms)}
              disabled={line.ms == null || disabled || playback.live}>{line.text}</button
            >
          {/each}
        </aside>
      {/if}
    </div>
  {:else}
    <div class="empty">
      <p class="kaomoji mono">=^..^=</p>
      <h1>{t('np.nothingPlaying')}</h1>
      <p class="sub">{t('np.nothingSub')}</p>
      <div class="empty-actions">
        <button class="primary" onclick={() => ui.setView('search')}>{t('nav.search')}</button>
        <button class="ghost" onclick={() => ui.setView('library')}>{t('np.openLibrary')}</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .np {
    position: relative;
    height: 100%;
    overflow: hidden;
  }
  .backdrop {
    position: absolute;
    inset: 0;
  }
  .backdrop img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    filter: blur(48px) saturate(1.3);
    transform: scale(1.2);
  }
  .backdrop-fill {
    width: 100%;
    height: 100%;
  }
  .scrim {
    position: absolute;
    inset: 0;
    background: var(--role-background);
    opacity: 0.8;
  }
  .stage {
    position: relative;
    display: grid;
    grid-template-columns: 1fr;
    height: 100%;
    overflow-y: auto;
  }
  .stage.with-lyrics {
    grid-template-columns: 1fr minmax(220px, 340px);
  }
  .main {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-4);
    padding: var(--space-8);
    min-width: 0;
  }
  .title-block {
    text-align: center;
    max-width: 560px;
  }
  h1 {
    margin: 0 0 var(--space-1);
    font-size: 24px;
    font-weight: 700;
    line-height: 1.25;
  }
  .artist {
    margin: 0;
    font-size: 14px;
    color: var(--role-text-muted);
  }
  .album {
    color: var(--role-text-subtle);
  }
  .seek-row {
    width: min(560px, 100%);
  }
  .icy {
    margin: calc(-1 * var(--space-2)) 0 0;
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .transport {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .tp {
    width: 40px;
    height: 40px;
    border: none;
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-player-control);
    font-size: 16px;
    display: grid;
    place-items: center;
  }
  .tp:hover:not(:disabled) {
    background: var(--surface-2);
  }
  .tp:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .tp.play {
    width: 54px;
    height: 54px;
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-size: 20px;
    box-shadow: var(--elev-1);
  }
  .tp.play:hover:not(:disabled) {
    background: var(--role-accent-alt);
  }
  .tp.rate.up {
    color: var(--role-success);
  }
  .tp.rate.down {
    color: var(--role-error);
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    justify-content: center;
    gap: var(--space-2);
  }
  .chip {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-player-label);
    font-size: 11.5px;
  }
  .chip.on {
    color: var(--role-accent);
    border-color: var(--role-accent);
    background: var(--surface-2);
  }
  .chip:hover:not(:disabled) {
    background: var(--surface-2);
  }
  .chip:disabled {
    opacity: 0.4;
  }
  .mono {
    font-family: var(--font-mono);
  }

  .lyrics {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-8) var(--space-6);
    overflow-y: auto;
    scrollbar-width: thin;
  }
  .line {
    border: none;
    background: transparent;
    text-align: left;
    font-size: 14px;
    line-height: 1.5;
    color: var(--role-lyrics-dim);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-s);
    transition:
      color 160ms ease,
      transform 160ms ease;
  }
  .line.active {
    color: var(--role-lyrics-current);
    font-weight: 600;
    transform: scale(1.03);
    transform-origin: left center;
  }
  .line.past {
    opacity: 0.6;
  }
  .line:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .line:disabled {
    cursor: default;
  }

  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    text-align: center;
  }
  .kaomoji {
    font-size: 26px;
    margin: 0;
    color: var(--role-accent);
  }
  .sub {
    margin: 0 0 var(--space-4);
    color: var(--role-text-muted);
    font-size: 13px;
  }
  .empty-actions {
    display: flex;
    gap: var(--space-2);
  }
  .primary {
    padding: var(--space-2) var(--space-6);
    border: none;
    border-radius: var(--radius-pill);
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-weight: 600;
  }
  .ghost {
    padding: var(--space-2) var(--space-6);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: transparent;
  }
</style>
