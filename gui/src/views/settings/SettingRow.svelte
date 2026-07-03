<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    label: string;
    hint?: string;
    /** Tag rendered after the label, e.g. "(TUI)" for terminal-only settings. */
    tag?: string;
    children: Snippet;
  }
  const { label, hint, tag, children }: Props = $props();
</script>

<div class="row">
  <div class="lab">
    <span class="l">
      {label}
      {#if tag}<span class="tag">{tag}</span>{/if}
    </span>
    {#if hint}<span class="h">{hint}</span>{/if}
  </div>
  <div class="ctl">
    {@render children()}
  </div>
</div>

<style>
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-6);
    padding: var(--space-3) var(--space-4);
  }
  .lab {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .l {
    font-size: 13px;
    color: var(--role-settings-label);
  }
  .tag {
    margin-left: var(--space-1);
    padding: 0 6px;
    border-radius: var(--radius-pill);
    background: var(--surface-2);
    color: var(--role-text-subtle);
    font-size: 10px;
    font-family: var(--font-mono);
  }
  .h {
    font-size: 11px;
    color: var(--role-text-subtle);
    line-height: 1.4;
  }
  .ctl {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex: none;
  }
</style>
