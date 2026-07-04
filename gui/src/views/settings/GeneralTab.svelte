<script lang="ts">
  // Settings → General (docs/gui/07 §6). Binds the `settings` read model; every mutation
  // sends `apply` and shows optimistically until the confirming push (docs/gui/05 §5.2).
  import type { AppCtx } from '../../lib/ctx';
  import type { SearchSource } from '../../generated/protocol/SearchSource';
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

  // TODO(wire:M5/i18n.catalog): once the en/ko catalog lands, every literal string in
  // these tabs sweeps through t() and this language select drives the live switch.
  const LANGS: Array<[string, string]> = [
    ['en', 'English'],
    ['ko', '한국어'],
  ];
  const SOURCES: Array<[SearchSource, string]> = [
    ['youtube', 'YouTube Music'],
    ['sound_cloud', 'SoundCloud'],
    ['audius', 'Audius'],
    ['jamendo', 'Jamendo'],
    ['internet_archive', 'Internet Archive'],
    ['radio_browser', 'Radio Browser'],
    ['all', 'All'],
  ];

  let confirmingReset = $state(false);
  function resetAll(): void {
    settings.resetAll();
    confirmingReset = false;
    toasts.show('success', 'Settings reset to defaults.');
  }
</script>

<SettingSection title="Interface">
  <SettingRow label="Language" hint="Live-switches every label, no reload">
    <select class="sel" onchange={(e) => settings.apply('ui', 'language', e.currentTarget.value)}>
      {#each LANGS as [code, label] (code)}
        <option value={code} selected={(ui?.language ?? 'en') === code}>{label}</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label="Album art" hint="Show artwork in the player and lists">
    <Toggle
      checked={ui?.album_art ?? true}
      onchange={(v) => settings.apply('ui', 'album_art', v)}
    />
  </SettingRow>
  <SettingRow label="Mouse support" tag="(TUI)" hint="Terminal mouse capture — GUI unaffected">
    <Toggle checked={ui?.mouse ?? true} onchange={(v) => settings.apply('ui', 'mouse', v)} />
  </SettingRow>
  <SettingRow label="Autoplay on start" hint="Resume the last session when the player starts">
    <Toggle
      checked={pb?.autoplay_on_start ?? false}
      onchange={(v) => settings.apply('playback', 'autoplay_on_start', v)}
    />
  </SettingRow>
  <SettingRow
    label="Enqueue next"
    hint="New plays insert after the current track instead of replacing"
  >
    <Toggle
      checked={pb?.enqueue_next ?? false}
      onchange={(v) => settings.apply('playback', 'enqueue_next', v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title="Search sources">
  <SettingRow label="Default source">
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
  <SettingRow label="Audius" hint="App name identifies you to the Audius API">
    <input
      class="ti"
      placeholder="app name"
      size="14"
      value={search?.audius_app_name ?? ''}
      onchange={(e) => settings.apply('search', 'audius_app_name', e.currentTarget.value || null)}
    />
    <Toggle
      checked={search?.audius_enabled ?? true}
      onchange={(v) => settings.apply('search', 'audius_enabled', v)}
    />
  </SettingRow>
  <SettingRow label="Jamendo" hint="Requires a free client id">
    <input
      class="ti"
      placeholder="client id"
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

<SettingSection title="Storage">
  <SettingRow
    label="Cookies file"
    hint="Netscape-format cookies for YTM personal results (v1: plain paths, like the TUI)"
  >
    <input
      class="ti path"
      placeholder="~/cookies.txt"
      value={storage?.cookies_file ?? ''}
      onchange={(e) => settings.apply('storage', 'cookies_file', e.currentTarget.value || null)}
    />
  </SettingRow>
  <SettingRow label="Download directory">
    <input
      class="ti path"
      placeholder="~/Music/ytm-tui"
      value={storage?.download_dir ?? ''}
      onchange={(e) => settings.apply('storage', 'download_dir', e.currentTarget.value || null)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title="Danger zone">
  <SettingRow label="Reset keybindings" hint="Restore every chord to its default">
    <button class="danger" onclick={() => keymap.resetAll()}>Reset</button>
  </SettingRow>
  <SettingRow label="Reset all settings" hint="Everything back to factory — asks for confirmation">
    {#if confirmingReset}
      <button class="mini" onclick={() => (confirmingReset = false)}>Cancel</button>
      <button class="danger" onclick={resetAll}>Confirm — reset everything</button>
    {:else}
      <button class="danger" onclick={() => (confirmingReset = true)}>Reset all…</button>
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
