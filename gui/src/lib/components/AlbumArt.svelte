<script lang="ts">
  // Album art card (docs/gui/06 §4): loads from the custom scheme via ArtworkRef; missing
  // art renders a deterministic themed placeholder (hue hashed from the track) — never a
  // remote URL (no-network CSP).
  //
  // TODO(wire:B1/artwork.live): verify against real `artwork` pushes once B1 lands.
  import type { TrackModel } from '../../generated/protocol/TrackModel';
  import { hueOf } from '../format';

  interface Props {
    track: TrackModel | null;
    /** CSS size (width = height). */
    size?: string;
    radius?: string;
    elevated?: boolean;
  }
  const { track, size = '56px', radius = 'var(--radius-m)', elevated = false }: Props = $props();

  // Origin-relative on purpose: the page is ytm://app/… on macOS but https://ytm.app/…
  // on Windows (wry rides custom schemes on https there), and a hardcoded ytm:// URL is
  // an unknown scheme the Windows webview drops. Relative resolves correctly on both.
  const url = $derived(
    track?.artwork?.path != null ? `/art/${encodeURIComponent(track.artwork.key)}` : null,
  );
  const hue = $derived(hueOf(track ? `${track.title}·${track.artist}` : 'ytm-tui'));
  const glyph = $derived(track ? [...(track.display_title ?? track.title)][0].toUpperCase() : '♪');
</script>

<div class="art" class:elevated style:width={size} style:height={size} style:border-radius={radius}>
  {#if url}
    <img src={url} alt="" loading="lazy" />
  {:else}
    <div
      class="placeholder"
      style:background="linear-gradient(135deg, hsl({hue} 42% 36%), hsl({(hue + 50) % 360} 55% 20%))"
    >
      <span>{glyph}</span>
    </div>
  {/if}
</div>

<style>
  .art {
    flex: none;
    overflow: hidden;
    background: var(--surface-2);
  }
  .art.elevated {
    box-shadow: var(--elev-2);
  }
  img,
  .placeholder {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .placeholder {
    display: grid;
    place-items: center;
    color: rgb(255 255 255 / 0.82);
    font-weight: 700;
    font-size: calc(var(--art-glyph, 40%));
  }
  .placeholder span {
    font-size: 1em;
    transform: scale(2.2);
  }
</style>
