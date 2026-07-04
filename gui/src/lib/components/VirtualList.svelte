<script lang="ts" generics="T">
  // Hand-rolled fixed-row-height windowing (docs/gui/05 §7): third-party Svelte 5 virtual
  // lists are immature, and our rows are all fixed-height by design.
  //
  // Opt-in pointer-drag reorder (queue.reorder): pass `reorder` and mark a handle inside the
  // row with `data-drag-handle`. Pointer events, NOT HTML5 DnD (unreliable in WKWebView).
  // Index math + auto-scroll live in lib/dnd/reorder.ts (unit-tested); this drives them.
  import type { Snippet } from 'svelte';
  import { dropIndex, autoScrollStep } from '../dnd/reorder';

  interface Props {
    items: T[];
    rowHeight: number;
    overscan?: number;
    row: Snippet<[T, number]>;
    empty?: Snippet;
    /** Enable drag-reorder; fires with (from, to) on drop. Rows must carry a
     *  `data-drag-handle` element to start a drag. */
    reorder?: (from: number, to: number) => void;
  }
  const { items, rowHeight, overscan = 6, row, empty, reorder }: Props = $props();

  let listEl = $state<HTMLDivElement | null>(null);
  let scrollTop = $state(0);
  let viewHeight = $state(0);

  const first = $derived(Math.max(0, Math.floor(scrollTop / rowHeight) - overscan));
  const last = $derived(
    Math.min(items.length, Math.ceil((scrollTop + viewHeight) / rowHeight) + overscan),
  );

  // ── drag-reorder ─────────────────────────────────────────────────────────────────────
  let dragFrom = $state<number | null>(null);
  let dragTo = $state<number | null>(null);
  let pointerClientY = 0;
  let rafId: number | null = null;

  $effect(() => () => {
    if (rafId != null) cancelAnimationFrame(rafId);
  });

  function rowIndexOf(target: EventTarget | null): number | null {
    const el = (target as HTMLElement | null)?.closest?.('.vrow') as HTMLElement | null;
    const idx = el?.dataset.index;
    return idx == null ? null : Number(idx);
  }

  function onPointerDown(e: PointerEvent) {
    if (!reorder || !(e.target as HTMLElement | null)?.closest?.('[data-drag-handle]')) return;
    const from = rowIndexOf(e.target);
    if (from == null) return;
    e.preventDefault();
    dragFrom = from;
    dragTo = from;
    pointerClientY = e.clientY;
    try {
      listEl?.setPointerCapture?.(e.pointerId);
    } catch {
      /* pointer capture is best-effort (absent under happy-dom / older engines) */
    }
    startAutoScroll();
  }

  function refreshTarget() {
    if (dragFrom == null || !listEl) return;
    const contentY = pointerClientY - listEl.getBoundingClientRect().top + listEl.scrollTop;
    dragTo = dropIndex(contentY, rowHeight, items.length);
  }

  function onPointerMove(e: PointerEvent) {
    if (dragFrom == null) return;
    pointerClientY = e.clientY;
    refreshTarget();
  }

  function startAutoScroll() {
    if (rafId != null) return;
    const tick = () => {
      if (dragFrom == null || !listEl) {
        rafId = null;
        return;
      }
      const step = autoScrollStep(pointerClientY - listEl.getBoundingClientRect().top, viewHeight);
      if (step !== 0) {
        listEl.scrollTop += step;
        refreshTarget();
      }
      rafId = requestAnimationFrame(tick);
    };
    rafId = requestAnimationFrame(tick);
  }

  function endDrag(commit: boolean) {
    if (rafId != null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
    const from = dragFrom;
    const to = dragTo;
    dragFrom = null;
    dragTo = null;
    if (commit && reorder && from != null && to != null && from !== to) reorder(from, to);
  }
</script>

{#if items.length === 0}
  {#if empty}{@render empty()}{/if}
{:else}
  <!-- svelte-ignore a11y_no_static_element_interactions -- the pointer handlers delegate
       drag from a row's [data-drag-handle]; the scroll container itself has no role -->
  <div
    class="vlist"
    class:reordering={dragFrom != null}
    bind:this={listEl}
    onscroll={(e) => (scrollTop = e.currentTarget.scrollTop)}
    bind:clientHeight={viewHeight}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={() => endDrag(true)}
    onpointercancel={() => endDrag(false)}
  >
    <div class="spacer" style:height="{items.length * rowHeight}px">
      {#each items.slice(first, last) as item, i (first + i)}
        <div
          class="vrow"
          class:dragging={first + i === dragFrom}
          data-index={first + i}
          style:top="{(first + i) * rowHeight}px"
          style:height="{rowHeight}px"
        >
          {@render row(item, first + i)}
        </div>
      {/each}
      {#if dragFrom != null && dragTo != null}
        <div class="drop-line" style:top="{dragTo * rowHeight}px"></div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .vlist {
    height: 100%;
    overflow-y: auto;
  }
  .vlist.reordering {
    user-select: none;
    cursor: grabbing;
  }
  .spacer {
    position: relative;
  }
  .vrow {
    position: absolute;
    left: 0;
    right: 0;
  }
  .vrow.dragging {
    opacity: 0.4;
  }
  .drop-line {
    position: absolute;
    left: 0;
    right: 0;
    height: 2px;
    margin-top: -1px;
    background: var(--role-accent);
    border-radius: 1px;
    pointer-events: none;
  }
</style>
