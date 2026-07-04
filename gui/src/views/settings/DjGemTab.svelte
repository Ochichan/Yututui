<script lang="ts">
  // Settings → DJ Gem (docs/gui/07 §10). Streaming/AI settings bind the `settings` read
  // model; the API key is write-only on the wire (only `has_gemini_key` presence returns,
  // never the key itself — docs/gui/02 §16).
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { settings, toasts } = ctx;

  const st = $derived(settings.streaming);
  const ui = $derived(settings.ui);
  let reveal = $state(false);

  const MODES: Array<[string, string]> = $derived([
    ['focused', t('settings.djgem.modeFocused')],
    ['balanced', t('settings.djgem.modeBalanced')],
    ['discovery', t('settings.djgem.modeDiscovery')],
  ]);

  async function clearCache(): Promise<void> {
    const n = await settings.clearRomanizationCache();
    toasts.show('success', t('settings.djgem.cacheCleared', { n }));
  }
</script>

<SettingSection title="DJ Gem">
  <SettingRow label={t('settings.djgem.enable')} hint={t('settings.djgem.enableHint')}>
    <Toggle
      checked={st?.ai_enabled ?? false}
      onchange={(v) => settings.apply('streaming', 'ai_enabled', v)}
    />
  </SettingRow>
  <SettingRow label={t('settings.djgem.geminiModel')}>
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
  <SettingRow label={t('settings.djgem.apiKey')} hint={t('settings.djgem.apiKeyHint')}>
    <input
      class="ti key"
      type={reveal ? 'text' : 'password'}
      placeholder={st?.has_gemini_key ? t('settings.djgem.keySaved') : 'AIza…'}
      onchange={(e) => {
        const key = e.currentTarget.value;
        if (key) settings.setGeminiKey(key);
        e.currentTarget.value = '';
      }}
    />
    <button class="mini" onclick={() => (reveal = !reveal)} title={t('settings.djgem.reveal')}
      >{reveal ? '🙈' : '👁'}</button
    >
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.djgem.autoplayStreaming')}>
  <SettingRow label={t('settings.djgem.autoplay')} hint={t('settings.djgem.autoplayHint')}>
    <Toggle
      checked={st?.autoplay ?? false}
      onchange={(v) => settings.apply('streaming', 'autoplay', v)}
    />
  </SettingRow>
  <SettingRow label={t('settings.djgem.mode')}>
    <div class="seg" role="radiogroup" aria-label={t('settings.djgem.streamingModeAria')}>
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

<SettingSection title={t('settings.djgem.titles')}>
  <SettingRow
    label={t('settings.djgem.romanizedTitles')}
    hint={t('settings.djgem.romanizedTitlesHint')}
  >
    <Toggle
      checked={ui?.romanized_titles ?? false}
      onchange={(v) => settings.apply('ui', 'romanized_titles', v)}
    />
  </SettingRow>
  <SettingRow
    label={t('settings.djgem.romanizationCache')}
    hint={t('settings.djgem.romanizationCacheHint')}
  >
    <button class="mini wide" onclick={clearCache}>{t('settings.djgem.clearCache')}</button>
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
