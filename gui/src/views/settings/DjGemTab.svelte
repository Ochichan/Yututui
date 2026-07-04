<script lang="ts">
  // Settings → DJ Gem (docs/gui/07 §10). Streaming/AI settings bind the `settings` read
  // model; the API key is write-only on the wire (only `has_gemini_key` presence returns,
  // never the key itself — docs/gui/02 §16).
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { settings, toasts } = ctx;

  const st = $derived(settings.streaming);
  const ui = $derived(settings.ui);
  let reveal = $state(false);

  const MODES: Array<[string, string]> = [
    ['focused', 'Focused'],
    ['balanced', 'Balanced'],
    ['discovery', 'Discovery'],
  ];

  async function clearCache(): Promise<void> {
    const n = await settings.clearRomanizationCache();
    toasts.show('success', `Cleared ${n} romanization${n === 1 ? '' : 's'}.`);
  }
</script>

<SettingSection title="DJ Gem">
  <SettingRow label="Enable DJ Gem" hint="AI DJ: autoplay picks, chat, why-this-track explanations">
    <Toggle
      checked={st?.ai_enabled ?? false}
      onchange={(v) => settings.apply('streaming', 'ai_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Gemini model">
    <select
      class="sel"
      onchange={(e) => settings.apply('streaming', 'gemini_model', e.currentTarget.value)}
    >
      {#each ['gemini-2.5-flash', 'gemini-2.5-pro'] as model (model)}
        <option value={model} selected={(st?.gemini_model ?? 'gemini-2.5-flash') === model}
          >{model}</option
        >
      {/each}
    </select>
  </SettingRow>
  <SettingRow label="API key" hint="Write-only: the core stores it; only presence is reported back">
    <input
      class="ti key"
      type={reveal ? 'text' : 'password'}
      placeholder={st?.has_gemini_key ? '•••••••• (saved)' : 'AIza…'}
      onchange={(e) => {
        const key = e.currentTarget.value;
        if (key) settings.setGeminiKey(key);
        e.currentTarget.value = '';
      }}
    />
    <button class="mini" onclick={() => (reveal = !reveal)} title="Reveal"
      >{reveal ? '🙈' : '👁'}</button
    >
  </SettingRow>
</SettingSection>

<SettingSection title="Autoplay streaming">
  <SettingRow label="Autoplay" hint="DJ Gem keeps the queue fed when it runs dry">
    <Toggle
      checked={st?.autoplay ?? false}
      onchange={(v) => settings.apply('streaming', 'autoplay', v)}
    />
  </SettingRow>
  <SettingRow label="Mode">
    <div class="seg" role="radiogroup" aria-label="Streaming mode">
      {#each MODES as [value, label] (value)}
        <button
          class="seg-btn"
          class:on={(st?.mode ?? 'balanced') === value}
          role="radio"
          aria-checked={(st?.mode ?? 'balanced') === value}
          onclick={() => settings.apply('streaming', 'mode', value)}
        >
          {label}
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
    <Toggle
      checked={ui?.romanized_titles ?? false}
      onchange={(v) => settings.apply('ui', 'romanized_titles', v)}
    />
  </SettingRow>
  <SettingRow label="Romanization cache" hint="Clears cached romanizations; reports the count">
    <button class="mini wide" onclick={clearCache}>Clear cache</button>
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
