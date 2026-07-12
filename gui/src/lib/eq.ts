// Pure EQ band math (mirrors lib/dnd/reorder.ts: keep geometry math unit-testable).

export const EQ_MIN = -12;
export const EQ_MAX = 12;

/** Clamp and round a gain to the whole-dB range the core accepts. */
export function clampGain(value: number): number {
  return Math.min(EQ_MAX, Math.max(EQ_MIN, Math.round(value)));
}

/** Map a pointer's clientY inside a vertical band track to a gain: top = +12, bottom = -12. */
export function gainAtPointer(rect: { top: number; height: number }, clientY: number): number {
  if (rect.height <= 0) return 0;
  const f = 1 - Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
  return clampGain(EQ_MIN + f * (EQ_MAX - EQ_MIN));
}
