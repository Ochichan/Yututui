<script lang="ts">
  // Volume gauge; the smoothness contract (70 ms send debounce + 1.8 s local echo) lives
  // in playback.setVolume — this just streams drag positions (docs/gui/05 §5.1).
  interface Props {
    volume: number;
    disabled?: boolean;
    onvolume: (percent: number) => void;
    onvolumeend?: () => void;
  }
  const { volume, disabled = false, onvolume, onvolumeend }: Props = $props();

  let track = $state<HTMLElement | null>(null);
  let dragging = $state(false);

  function pctAt(e: PointerEvent): number {
    const r = track!.getBoundingClientRect();
    return Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100));
  }
  function onpointerdown(e: PointerEvent) {
    if (disabled || !track) return;
    track.setPointerCapture(e.pointerId);
    dragging = true;
    onvolume(pctAt(e));
  }
  function onpointermove(e: PointerEvent) {
    if (dragging) onvolume(pctAt(e));
  }
  function onpointerup() {
    dragging = false;
    onvolumeend?.();
  }
</script>

<div class="vol" class:disabled title="Volume {Math.round(volume)}%">
  <span class="glyph" aria-hidden="true">{volume === 0 ? '🔇' : '🔊'}</span>
  <div
    class="track"
    bind:this={track}
    {onpointerdown}
    {onpointermove}
    {onpointerup}
    onpointercancel={onpointerup}
    role="slider"
    aria-label="Volume"
    aria-valuemin={0}
    aria-valuemax={100}
    aria-valuenow={Math.round(volume)}
    tabindex="-1"
  >
    <div class="fill" style:width="{volume}%"></div>
  </div>
  <span class="pct mono">{Math.round(volume)}</span>
</div>

<style>
  .vol {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 140px;
  }
  .vol.disabled {
    opacity: 0.5;
    pointer-events: none;
  }
  .glyph {
    font-size: 11px;
  }
  .track {
    position: relative;
    flex: 1;
    height: 14px;
    display: flex;
    align-items: center;
    cursor: pointer;
    touch-action: none;
  }
  .track::before {
    content: '';
    position: absolute;
    left: 0;
    right: 0;
    height: 4px;
    border-radius: var(--radius-pill);
    background: var(--role-gauge-empty);
  }
  .fill {
    position: absolute;
    left: 0;
    height: 4px;
    border-radius: var(--radius-pill);
    background: var(--role-gauge-filled);
  }
  .pct {
    min-width: 24px;
    text-align: right;
    font-size: 10px;
    color: var(--role-text-subtle);
  }
  .mono {
    font-family: var(--font-mono);
  }
</style>
