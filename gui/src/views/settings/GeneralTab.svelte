<script lang="ts">
  // Settings → General (docs/gui/07 §6). The forms are visually complete; every mutation
  // routes through the settings.apply patch-bay gate until the settings-v8 wire lands.
  // Values shown are representative defaults, flagged by the ribbon in SettingsView.
  //
  // TODO(wire:M3/settings.apply): bind these controls to settings.svelte.ts
  // (model + pending overlay) and send Apply(Ui/Search/Storage(...)).
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
  // TODO(wire:M5/i18n.catalog): once the en/ko catalog lands, every literal string in
  // these tabs sweeps through t() and this language select drives the live switch.
</script>

<SettingSection title="Interface">
  <SettingRow label="Language" hint="Live-switches every label, no reload">
    <select class="sel" onchange={stub}>
      <option>English</option>
      <option>한국어</option>
    </select>
  </SettingRow>
  <SettingRow label="Album art" hint="Show artwork in the player and lists">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Mouse support" tag="(TUI)" hint="Terminal mouse capture — GUI unaffected">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Autoplay on start" hint="Resume the last session when the player starts">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow
    label="Enqueue next"
    hint="New plays insert after the current track instead of replacing"
  >
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Search sources">
  <SettingRow label="Default source">
    <select class="sel" onchange={stub}>
      <option>YouTube Music</option>
      <option>SoundCloud</option>
      <option>Audius</option>
      <option>Jamendo</option>
      <option>Internet Archive</option>
      <option>Radio Browser</option>
      <option>All</option>
    </select>
  </SettingRow>
  <SettingRow label="SoundCloud">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Audius" hint="App name identifies you to the Audius API">
    <input class="ti" placeholder="app name" size="14" onchange={stub} />
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Jamendo" hint="Requires a free client id">
    <input class="ti" placeholder="client id" size="14" onchange={stub} />
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Internet Archive">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Radio Browser">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Storage">
  <SettingRow
    label="Cookies file"
    hint="Netscape-format cookies for YTM personal results (v1: plain paths, like the TUI)"
  >
    <input class="ti path" placeholder="~/cookies.txt" onchange={stub} />
  </SettingRow>
  <SettingRow label="Download directory">
    <input class="ti path" placeholder="~/Music/ytm-tui" onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Danger zone">
  <SettingRow label="Reset keybindings" hint="Restore every chord to its default">
    <button class="danger" onclick={stub}>Reset</button>
  </SettingRow>
  <SettingRow label="Reset all settings" hint="Everything back to factory — asks for confirmation">
    <button class="danger" onclick={stub}>Reset all…</button>
  </SettingRow>
</SettingSection>

<style>
  .path {
    width: 260px;
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
