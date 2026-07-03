<script lang="ts">
  // Click/drag-to-seek gauge with the live-stream "ON AIR" treatment (docs/gui/07 §1).
  // Optimism (the 1.5 s push hold) lives in the playback store; this only reports intent.
  import { fmtTime } from '../format';

  interface Props {
    positionMs: number | null;
    durationMs: number | null;
    /** Live stream ⇒ no scrubbing, pulsing full bar. */
    live?: boolean;
    disabled?: boolean;
    labels?: boolean;
    onseek: (ms: number) => void;
  }
  const {
    positionMs,
    durationMs,
    live = false,
    disabled = false,
    labels = true,
    onseek,
  }: Props = $props();

  let track = $state<HTMLElement | null>(null);
  let dragRatio = $state<number | null>(null);

  const ratio = $derived.by(() => {
    if (dragRatio != null) return dragRatio;
    if (live) return 1;
    if (positionMs == null || durationMs == null || durationMs === 0) return 0;
    return Math.max(0, Math.min(1, positionMs / durationMs));
  });

  const shownMs = $derived(
    dragRatio != null && durationMs != null ? dragRatio * durationMs : positionMs,
  );

  function ratioAt(e: PointerEvent): number {
    const r = track!.getBoundingClientRect();
    return Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
  }

  function onpointerdown(e: PointerEvent) {
    if (disabled || live || durationMs == null || !track) return;
    track.setPointerCapture(e.pointerId);
    dragRatio = ratioAt(e);
  }
  function onpointermove(e: PointerEvent) {
    if (dragRatio == null) return;
    dragRatio = ratioAt(e);
  }
  function onpointerup() {
    if (dragRatio == null || durationMs == null) return;
    onseek(dragRatio * durationMs);
    dragRatio = null;
  }
</script>

<div class="seek" class:disabled>
  {#if labels}
    <span class="t mono">{live ? 'ON AIR' : fmtTime(shownMs ?? 0)}</span>
  {/if}
  <div
    class="track"
    class:live
    bind:this={track}
    {onpointerdown}
    {onpointermove}
    {onpointerup}
    role="slider"
    aria-label="Seek"
    aria-valuemin={0}
    aria-valuemax={durationMs ?? 0}
    aria-valuenow={Math.round(shownMs ?? 0)}
    aria-disabled={disabled || live}
    tabindex="-1"
  >
    <div class="fill" style:width="{ratio * 100}%"></div>
    {#if !live && durationMs != null}
      <div class="thumb" style:left="{ratio * 100}%"></div>
    {/if}
  </div>
  {#if labels}
    <span class="t mono right">{live ? 'LIVE' : fmtTime(durationMs)}</span>
  {/if}
</div>

<style>
  .seek {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    width: 100%;
  }
  .seek.disabled {
    opacity: 0.5;
    pointer-events: none;
  }
  .t {
    flex: none;
    min-width: 40px;
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .t.right {
    text-align: right;
  }
  .mono {
    font-family: var(--font-mono);
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
  .track.live .fill {
    animation: onair 2.4s ease-in-out infinite;
  }
  @keyframes onair {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.55;
    }
  }
  .thumb {
    position: absolute;
    width: 12px;
    height: 12px;
    border-radius: var(--radius-pill);
    background: var(--role-gauge-filled);
    box-shadow: var(--elev-1);
    transform: translateX(-50%);
    opacity: 0;
    transition: opacity 120ms ease;
  }
  .track:hover .thumb {
    opacity: 1;
  }
</style>
