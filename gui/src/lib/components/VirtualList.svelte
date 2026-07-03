<script lang="ts" generics="T">
  // Hand-rolled fixed-row-height windowing (docs/gui/05 §7): third-party Svelte 5 virtual
  // lists are immature, and our rows are all fixed-height by design.
  //
  // TODO(wire:M2/queue.reorder): pointer-drag reorder (with auto-scroll) lands here.
  import type { Snippet } from 'svelte';

  interface Props {
    items: T[];
    rowHeight: number;
    overscan?: number;
    row: Snippet<[T, number]>;
    empty?: Snippet;
  }
  const { items, rowHeight, overscan = 6, row, empty }: Props = $props();

  let scrollTop = $state(0);
  let viewHeight = $state(0);

  const first = $derived(Math.max(0, Math.floor(scrollTop / rowHeight) - overscan));
  const last = $derived(
    Math.min(items.length, Math.ceil((scrollTop + viewHeight) / rowHeight) + overscan),
  );
</script>

{#if items.length === 0}
  {#if empty}{@render empty()}{/if}
{:else}
  <div
    class="vlist"
    onscroll={(e) => (scrollTop = e.currentTarget.scrollTop)}
    bind:clientHeight={viewHeight}
  >
    <div class="spacer" style:height="{items.length * rowHeight}px">
      {#each items.slice(first, last) as item, i (first + i)}
        <div class="vrow" style:top="{(first + i) * rowHeight}px" style:height="{rowHeight}px">
          {@render row(item, first + i)}
        </div>
      {/each}
    </div>
  </div>
{/if}

<style>
  .vlist {
    height: 100%;
    overflow-y: auto;
  }
  .spacer {
    position: relative;
  }
  .vrow {
    position: absolute;
    left: 0;
    right: 0;
  }
</style>
