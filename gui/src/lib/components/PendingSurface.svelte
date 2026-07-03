<script lang="ts">
  // A whole pane whose data wire is pending: a dashed patch-bay card explaining what will
  // appear here and which milestone brings it, with a jump into the wiring brief. Used by
  // Search results, Library tabs, DJ Gem, and the Settings ribbon — one consistent look
  // for "finished UI, wire pending".
  import { WIRING, type FeatureId } from '../wiring/registry';
  import type { WipStore } from '../wiring/wip.svelte';

  interface Props {
    id: FeatureId;
    wip: WipStore;
    /** What this pane will show once wired (one sentence, user-facing). */
    body: string;
    glyph?: string;
    /** Slim single-line ribbon variant (for view headers). */
    slim?: boolean;
  }
  const { id, wip, body, glyph = '🔌', slim = false }: Props = $props();
  const spec = $derived(WIRING[id]);
</script>

{#if slim}
  <button class="ribbon" onclick={() => wip.open(id)}>
    <span aria-hidden="true">{glyph}</span>
    <span class="rb">{body}</span>
    <span class="ms mono">{spec.milestone}</span>
  </button>
{:else}
  <div class="panel">
    <div class="glyph" aria-hidden="true">{glyph}</div>
    <p class="head">Wire pending — lands in {spec.milestone}</p>
    <p class="body">{body}</p>
    <button class="brief" onclick={() => wip.open(id)}>View wiring brief</button>
    <p class="cat mono" aria-hidden="true">=^..^=&nbsp; patience, the cable is on its way</p>
  </div>
{/if}

<style>
  .panel {
    max-width: 460px;
    margin: 0 auto;
    padding: var(--space-8);
    border: 1px dashed var(--role-border-primary);
    border-radius: var(--radius-l);
    background: var(--surface-1);
    text-align: center;
  }
  .glyph {
    font-size: 28px;
    margin-bottom: var(--space-2);
  }
  .head {
    margin: 0 0 var(--space-2);
    font-size: 14px;
    font-weight: 600;
    color: var(--role-accent);
  }
  .body {
    margin: 0 0 var(--space-4);
    font-size: 12.5px;
    line-height: 1.55;
    color: var(--role-text-muted);
  }
  .brief {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-primary);
    font-size: 12px;
  }
  .brief:hover {
    background: var(--surface-2);
  }
  .cat {
    margin: var(--space-4) 0 0;
    font-size: 10.5px;
    color: var(--role-text-subtle);
  }
  .mono {
    font-family: var(--font-mono);
  }

  .ribbon {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: var(--space-1) var(--space-3);
    border: 1px dashed var(--role-border-muted);
    border-radius: var(--radius-s);
    background: var(--surface-1);
    color: var(--role-text-muted);
    font-size: 11.5px;
    text-align: left;
  }
  .ribbon:hover {
    background: var(--surface-2);
  }
  .rb {
    flex: 1;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ms {
    color: var(--role-warning);
    font-size: 10px;
  }
</style>
