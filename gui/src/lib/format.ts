// Tiny display formatters shared across views. Locale-aware forms land with i18n (M5).

/** ms → "m:ss" / "h:mm:ss"; null (live stream) → "LIVE". */
export function fmtTime(ms: number | null): string {
  if (ms == null) return 'LIVE';
  const total = Math.max(0, Math.floor(ms / 1000));
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m);
  return `${h > 0 ? `${h}:` : ''}${mm}:${String(s).padStart(2, '0')}`;
}

/** Playback speed in tenths → "1.5×"; 10 (1.0×) renders empty (the TUI hides the default). */
export function fmtSpeed(tenths: number): string {
  return tenths === 10 ? '' : `${(tenths / 10).toFixed(1)}×`;
}

/** Stable tiny hash for deterministic per-track placeholder art hues. */
export function hueOf(text: string): number {
  let h = 0;
  for (let i = 0; i < text.length; i++) h = (h * 31 + text.charCodeAt(i)) | 0;
  return ((h % 360) + 360) % 360;
}
