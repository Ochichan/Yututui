<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    title: string;
    children: Snippet;
  }
  const { title, children }: Props = $props();
</script>

<section>
  <h3>{title}</h3>
  <div class="rows">
    {@render children()}
  </div>
</section>

<style>
  section {
    margin-bottom: var(--space-8);
  }
  h3 {
    margin: 0 0 var(--space-3);
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--role-settings-group);
  }
  .rows {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-m);
    background: var(--surface-1);
  }
  .rows > :global(* + *) {
    border-top: 1px solid var(--role-border-muted);
  }
</style>
