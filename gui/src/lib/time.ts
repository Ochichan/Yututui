// Position interpolation — the mini player's proven constants, ported verbatim per
// docs/gui/05 §5.1 (panel.rs:1728,1797,1845,2006). These numbers are the difference
// between "feels native" and "feels laggy"; do NOT re-derive them.

/** After an optimistic local seek, ignore incoming position pushes for this long. */
export const SEEK_HOLD_MS = 1500;
/** After a local volume drag, the local echo wins over authoritative pushes this long. */
export const VOLUME_ECHO_MS = 1800;
/** Volume drag send debounce. */
export const VOLUME_SEND_DEBOUNCE_MS = 70;

export interface PositionAnchor {
  /** Sampled elapsed at the last snapshot; null ⇒ nothing playing / unknown. */
  elapsedMs: number | null;
  /** `performance.now()` when the snapshot (or optimistic seek) was anchored. */
  anchorAt: number;
  /** Playback speed in tenths (10 = 1.0×). */
  speedTenths: number;
  paused: boolean;
  /** null means unbounded/unknown; callers must use TrackModel.is_live for "ON AIR". */
  durationMs: number | null;
}

/** position = elapsed_ms + (now − anchorAt) × speed while playing, clamped to duration. */
export function interpolate(a: PositionAnchor, now: number): number | null {
  if (a.elapsedMs == null) return null;
  if (a.paused) return a.elapsedMs;
  const pos = a.elapsedMs + (now - a.anchorAt) * (a.speedTenths / 10);
  return a.durationMs == null ? pos : Math.min(pos, a.durationMs);
}
