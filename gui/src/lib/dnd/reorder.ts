// Pointer-drag reorder math (docs/gui/05 §7, queue.reorder). Kept pure and separate from
// VirtualList so the index arithmetic — the part worth trusting — is unit-tested without a
// DOM. The pointer wiring in VirtualList.svelte drives these; the store applies `applyMove`
// optimistically and the demo/real core mirrors it under a rev guard.

/**
 * The destination index (`0..count-1`) for a pointer at `contentY` (pixels from the top of
 * the scrollable content, i.e. `clientY - listTop + scrollTop`) over fixed-height rows.
 * Clamped so a drag past either end lands on the first/last slot.
 */
export function dropIndex(contentY: number, rowHeight: number, count: number): number {
  if (count <= 0 || rowHeight <= 0) return 0;
  const raw = Math.floor(contentY / rowHeight);
  return Math.max(0, Math.min(raw, count - 1));
}

/**
 * Move the item at `from` so it ends up at index `to`, returning a new array (the original
 * is never mutated). Out-of-range or no-op moves return a shallow copy unchanged.
 */
export function applyMove<T>(items: T[], from: number, to: number): T[] {
  const next = items.slice();
  if (from < 0 || from >= next.length || from === to) return next;
  const [moved] = next.splice(from, 1);
  const dest = Math.max(0, Math.min(to, next.length));
  next.splice(dest, 0, moved);
  return next;
}

/**
 * How far to nudge `scrollTop` this frame when a drag hovers near a viewport edge, so a
 * drag can reach rows outside the current window. `y` is the pointer's offset from the
 * viewport top; returns a signed delta (negative = scroll up), zero outside the edge bands.
 */
export function autoScrollStep(y: number, viewHeight: number, edge = 36, maxStep = 14): number {
  if (viewHeight <= 0) return 0;
  if (y < edge) return -Math.ceil((1 - y / edge) * maxStep);
  if (y > viewHeight - edge) return Math.ceil((1 - (viewHeight - y) / edge) * maxStep);
  return 0;
}
