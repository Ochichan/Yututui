<script lang="ts">
  // The visible half of the patch-bay convention: a small chip on every surface whose
  // wire is pending. Clicking it opens the WipModal with the feature's marching orders,
  // so both users and follow-up agents can see exactly what's missing from inside the app.
  import { WIRING, type FeatureId } from '../wiring/registry';
  import type { WipStore } from '../wiring/wip.svelte';

  interface Props {
    id: FeatureId;
    wip: WipStore;
  }
  const { id, wip }: Props = $props();
</script>

{#if !wip.wired(id)}
  <button class="wiretag" onclick={() => wip.open(id)} title={WIRING[id].title}>
    <span class="bolt" aria-hidden="true">⚡</span>
    {WIRING[id].milestone} · wiring pending
  </button>
{/if}

<style>
  .wiretag {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 1px 8px;
    border: 1px dashed var(--role-warning);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-warning);
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 16px;
    white-space: nowrap;
  }
  .wiretag:hover {
    background: var(--surface-2);
  }
  .bolt {
    font-size: 9px;
  }
</style>
