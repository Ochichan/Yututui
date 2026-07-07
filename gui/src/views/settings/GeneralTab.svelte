<script lang="ts">
  // Settings → General (docs/gui/07 §6). Binds the `settings` read model; every mutation
  // sends `apply` and shows optimistically until the confirming push (docs/gui/05 §5.2).
  import type { AppCtx } from '../../lib/ctx';
  import type { SearchSource } from '../../generated/protocol/SearchSource';
  import { t, LANGUAGES } from '../../lib/i18n.svelte';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { settings, keymap, toasts } = ctx;

  const ui = $derived(settings.ui);
  const search = $derived(settings.search);
  const storage = $derived(settings.storage);
  const pb = $derived(settings.playback);

  // Source names are proper nouns (kept verbatim); only "All" is a translatable label, so
  // the list is derived to live-switch with the language.
  const SOURCES = $derived<Array<[SearchSource, string]>>([
    ['youtube', 'YouTube Music'],
    ['sound_cloud', 'SoundCloud'],
    ['audius', 'Audius'],
    ['jamendo', 'Jamendo'],
    ['internet_archive', 'Internet Archive'],
    ['radio_browser', 'Radio Browser'],
    ['all', t('common.all')],
  ]);

  let confirmingReset = $state(false);
  function resetAll(): void {
    settings.resetAll();
    confirmingReset = false;
    toasts.show('success', t('settings.general.resetDone'));
  }
</script>

<SettingSection title={t('settings.general.interface')}>
  <SettingRow label={t('settings.general.language')} hint={t('settings.general.languageHint')}>
    <select class="sel" onchange={(e) => settings.apply('ui', 'language', e.currentTarget.value)}>
      {#each LANGUAGES as { code, label } (code)}
        <option value={code} selected={(ui?.language ?? 'en') === code}>{label}</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label={t('settings.general.albumArt')} hint={t('settings.general.albumArtHint')}>
    <Toggle
      checked={ui?.album_art ?? true}
      onchange={(v) => settings.apply('ui', 'album_art', v)}
    />
  </SettingRow>
  <SettingRow
    label={t('settings.general.mouse')}
    tag="(TUI)"
    hint={t('settings.general.mouseHint')}
  >
    <Toggle checked={ui?.mouse ?? true} onchange={(v) => settings.apply('ui', 'mouse', v)} />
  </SettingRow>
  <SettingRow
    label={t('settings.general.autoplayStart')}
    hint={t('settings.general.autoplayStartHint')}
  >
    <Toggle
      checked={pb?.autoplay_on_start ?? false}
      onchange={(v) => settings.apply('playback', 'autoplay_on_start', v)}
    />
  </SettingRow>
  <SettingRow
    label={t('settings.general.enqueueNext')}
    hint={t('settings.general.enqueueNextHint')}
  >
    <Toggle
      checked={pb?.enqueue_next ?? false}
      onchange={(v) => settings.apply('playback', 'enqueue_next', v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.general.searchSources')}>
  <SettingRow label={t('settings.general.defaultSource')}>
    <select
      class="sel"
      onchange={(e) =>
        settings.apply('search', 'default_source', e.currentTarget.value as SearchSource)}
    >
      {#each SOURCES as [value, label] (value)}
        <option {value} selected={(search?.default_source ?? 'youtube') === value}>{label}</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label="SoundCloud">
    <Toggle
      checked={search?.soundcloud_enabled ?? true}
      onchange={(v) => settings.apply('search', 'soundcloud_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Audius" hint={t('settings.general.audiusHint')}>
    <input
      class="ti"
      placeholder={t('settings.general.appNamePlaceholder')}
      size="14"
      value={search?.audius_app_name ?? ''}
      onchange={(e) => settings.apply('search', 'audius_app_name', e.currentTarget.value || null)}
    />
    <Toggle
      checked={search?.audius_enabled ?? true}
      onchange={(v) => settings.apply('search', 'audius_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Jamendo" hint={t('settings.general.jamendoHint')}>
    <input
      class="ti"
      placeholder={t('settings.general.clientIdPlaceholder')}
      size="14"
      value={search?.jamendo_client_id ?? ''}
      onchange={(e) => settings.apply('search', 'jamendo_client_id', e.currentTarget.value || null)}
    />
    <Toggle
      checked={search?.jamendo_enabled ?? false}
      onchange={(v) => settings.apply('search', 'jamendo_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Internet Archive">
    <Toggle
      checked={search?.internet_archive_enabled ?? true}
      onchange={(v) => settings.apply('search', 'internet_archive_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Radio Browser">
    <Toggle
      checked={search?.radio_browser_enabled ?? true}
      onchange={(v) => settings.apply('search', 'radio_browser_enabled', v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.general.storage')}>
  <SettingRow label={t('settings.general.cookiesFile')} hint={t('settings.general.cookiesHint')}>
    <input
      class="ti path"
      placeholder="~/cookies.txt"
      value={storage?.cookies_file ?? ''}
      onchange={(e) => settings.apply('storage', 'cookies_file', e.currentTarget.value || null)}
    />
  </SettingRow>
  <SettingRow label={t('settings.general.downloadDir')}>
    <input
      class="ti path"
      placeholder="~/Music/yututui"
      value={storage?.download_dir ?? ''}
      onchange={(e) => settings.apply('storage', 'download_dir', e.currentTarget.value || null)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.general.dangerZone')}>
  <SettingRow label={t('settings.general.resetKeys')} hint={t('settings.general.resetKeysHint')}>
    <button class="danger" onclick={() => keymap.resetAll()}>{t('common.reset')}</button>
  </SettingRow>
  <SettingRow label={t('settings.general.resetAll')} hint={t('settings.general.resetAllHint')}>
    {#if confirmingReset}
      <button class="mini" onclick={() => (confirmingReset = false)}>{t('common.cancel')}</button>
      <button class="danger" onclick={resetAll}>{t('settings.general.resetAllConfirm')}</button>
    {:else}
      <button class="danger" onclick={() => (confirmingReset = true)}
        >{t('settings.general.resetAllCta')}</button
      >
    {/if}
  </SettingRow>
</SettingSection>

<style>
  .path {
    width: 260px;
  }
  .mini {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .mini:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .danger {
    padding: var(--space-1) var(--space-4);
    border: 1px solid var(--role-error);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-error);
    font-size: 12px;
  }
  .danger:hover {
    background: var(--role-error);
    color: var(--role-text-inverse);
  }
</style>
