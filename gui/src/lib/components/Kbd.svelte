<script lang="ts">
  // A chord chip, colored by the help-key role like the TUI's help overlay.
  interface Props {
    chord: string;
  }
  const { chord }: Props = $props();
</script>

<kbd>{chord}</kbd>

<style>
  kbd {
    display: inline-block;
    min-width: 20px;
    padding: 1px 6px;
    border: 1px solid var(--role-border-muted);
    border-bottom-width: 2px;
    border-radius: var(--radius-s);
    background: var(--surface-2);
    color: var(--role-help-key);
    font-family: var(--font-mono);
    font-size: 11px;
    text-align: center;
  }
</style>
