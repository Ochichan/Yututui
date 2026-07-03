<script lang="ts">
  // Settings → DJ Gem (docs/gui/07 §10). Key handling is write-only on the wire
  // (has_gemini_key presence comes back, never the key itself).
  //
  // TODO(wire:M3/settings.apply): SetGeminiKey + Apply(Streaming(...)) + clear-cache cmd.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip } = ctx;

  const stub = () => wip.gate('settings.apply');
  let reveal = $state(false);

  const MODES = ['Focused', 'Balanced', 'Discovery'];
</script>

<SettingSection title="DJ Gem">
  <SettingRow label="Enable DJ Gem" hint="AI DJ: autoplay picks, chat, why-this-track explanations">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Gemini model">
    <select class="sel" onchange={stub}>
      <option>gemini-2.5-flash</option>
      <option>gemini-2.5-pro</option>
    </select>
  </SettingRow>
  <SettingRow label="API key" hint="Write-only: the core stores it; only presence is reported back">
    <input class="ti key" type={reveal ? 'text' : 'password'} placeholder="AIza…" onchange={stub} />
    <button class="mini" onclick={() => (reveal = !reveal)} title="Reveal"
      >{reveal ? '🙈' : '👁'}</button
    >
  </SettingRow>
</SettingSection>

<SettingSection title="Autoplay streaming">
  <SettingRow label="Autoplay" hint="DJ Gem keeps the queue fed when it runs dry">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Mode">
    <div class="seg" role="radiogroup" aria-label="Streaming mode">
      {#each MODES as m (m)}
        <button
          class="seg-btn"
          class:on={m === 'Balanced'}
          role="radio"
          aria-checked={m === 'Balanced'}
          onclick={stub}
        >
          {m}
        </button>
      {/each}
    </div>
  </SettingRow>
</SettingSection>

<SettingSection title="Titles">
  <SettingRow
    label="Romanized titles"
    hint="Core-resolved display override — the GUI never romanizes itself"
  >
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Romanization cache" hint="Clears cached romanizations; reports the count">
    <button class="mini wide" onclick={stub}>Clear cache</button>
  </SettingRow>
</SettingSection>

<style>
  .key {
    width: 220px;
  }
  .mini {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .mini:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .mini.wide {
    padding: var(--space-1) var(--space-4);
    border-radius: var(--radius-pill);
  }
  .seg {
    display: flex;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    overflow: hidden;
  }
  .seg-btn {
    padding: var(--space-1) var(--space-3);
    border: none;
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .seg-btn.on {
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-weight: 600;
  }
</style>
