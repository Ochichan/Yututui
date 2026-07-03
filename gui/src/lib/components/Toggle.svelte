<script lang="ts">
  // Switch control, token-themed (docs/gui/06 §6).
  interface Props {
    checked: boolean;
    disabled?: boolean;
    label?: string;
    onchange: (value: boolean) => void;
  }
  const { checked, disabled = false, label, onchange }: Props = $props();
</script>

<button
  class="toggle"
  class:on={checked}
  role="switch"
  aria-checked={checked}
  aria-label={label}
  {disabled}
  onclick={() => onchange(!checked)}
>
  <span class="knob"></span>
</button>

<style>
  .toggle {
    width: 38px;
    height: 22px;
    flex: none;
    padding: 2px;
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: var(--role-gauge-empty);
    transition:
      background 140ms ease,
      border-color 140ms ease;
  }
  .toggle.on {
    background: var(--role-accent);
    border-color: var(--role-accent);
  }
  .toggle:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .knob {
    display: block;
    width: 16px;
    height: 16px;
    border-radius: var(--radius-pill);
    background: var(--role-text-primary);
    transition: transform 140ms ease;
  }
  .toggle.on .knob {
    background: var(--role-text-inverse);
    transform: translateX(16px);
  }
</style>
