<script lang="ts">
  // Settings → Playback (docs/gui/07 §7). Binds the `settings` read model — the persisted
  // startup defaults, NOT the live player state (docs/gui/02 §11.6 binding rule: the
  // transport bar owns the live values; the Settings screen owns the defaults, and they may
  // legitimately differ). Live-apply: every change sends `apply` and shows optimistically.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { settings } = ctx;

  const BAND_LABELS = ['31', '62', '125', '250', '500', '1k', '2k', '4k', '8k', '16k'];
  const pb = $derived(settings.playback);
  const eq = $derived(settings.eq);
  const speed = $derived((pb?.speed_tenths ?? 10) / 10);
  const SEEKS = [5, 10, 30];

  /** Click a band to zero it (docs/gui/07 §7); sends the full 10-band array. */
  function zeroBand(i: number): void {
    if (!eq) return;
    const bands = [...eq.bands];
    bands[i] = 0;
    settings.apply('eq', 'bands', bands);
  }
</script>

<SettingSection title="Now playing">
  <SettingRow label="Playback speed" hint="0.5× – 2.0×, step 0.1">
    <input
      class="range"
      type="range"
      min="0.5"
      max="2"
      step="0.1"
      value={speed}
      onchange={(e) =>
        settings.apply('playback', 'speed_tenths', Math.round(Number(e.currentTarget.value) * 10))}
    />
    <span class="val mono">{speed.toFixed(1)}×</span>
  </SettingRow>
  <SettingRow label="Seek interval" hint="Arrow-key / button seek step">
    <select
      class="sel"
      onchange={(e) => settings.apply('playback', 'seek_seconds', Number(e.currentTarget.value))}
    >
      {#each SEEKS as s (s)}
        <option value={s} selected={(pb?.seek_seconds ?? 5) === s}>{s} s</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label="Wheel volume" hint="Mouse wheel over the player adjusts volume">
    <Toggle
      checked={pb?.mouse_wheel_volume ?? true}
      onchange={(v) => settings.apply('playback', 'mouse_wheel_volume', v)}
    />
  </SettingRow>
  <SettingRow label="Gapless playback">
    <Toggle
      checked={pb?.gapless ?? true}
      onchange={(v) => settings.apply('playback', 'gapless', v)}
    />
  </SettingRow>
  <SettingRow label="OS media controls" hint="Media keys + system now-playing integration">
    <Toggle
      checked={pb?.media_controls ?? true}
      onchange={(v) => settings.apply('playback', 'media_controls', v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title="Equalizer">
  <SettingRow label="Preset" hint="Applies live through mpv's audio filter chain">
    <select class="sel" onchange={(e) => settings.apply('eq', 'preset', e.currentTarget.value)}>
      {#each ['flat', 'bass', 'vocal', 'rock', 'custom'] as preset (preset)}
        <option value={preset} selected={(eq?.preset ?? 'flat') === preset}>{preset}</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label="Normalize loudness">
    <Toggle
      checked={eq?.normalize ?? false}
      onchange={(v) => settings.apply('eq', 'normalize', v)}
    />
  </SettingRow>
  <div class="bank" role="group" aria-label="EQ bands">
    {#each BAND_LABELS as label, i (label)}
      {@const gain = eq?.bands[i] ?? 0}
      <button
        class="band"
        onclick={() => zeroBand(i)}
        title="{label} Hz · {gain > 0 ? '+' : ''}{gain} dB — click to zero"
      >
        <span class="col">
          <span class="fill" style:height="{((gain + 12) / 24) * 100}%"></span>
        </span>
        <span class="freq mono">{label}</span>
      </button>
    {/each}
    <p class="bank-hint">±12 dB per ISO band — click a band to zero it</p>
  </div>
</SettingSection>

<style>
  .range {
    width: 160px;
    accent-color: var(--role-accent);
  }
  .val {
    min-width: 36px;
    text-align: right;
    font-size: 12px;
    color: var(--role-settings-value);
  }
  .mono {
    font-family: var(--font-mono);
  }
  .bank {
    display: flex;
    align-items: flex-end;
    gap: var(--space-2);
    flex-wrap: wrap;
    padding: var(--space-4);
  }
  .band {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-1);
    border: none;
    background: transparent;
    padding: 0;
  }
  .col {
    position: relative;
    width: 14px;
    height: 96px;
    border-radius: var(--radius-pill);
    background: var(--role-gauge-empty);
    overflow: hidden;
    display: flex;
    align-items: flex-end;
  }
  .fill {
    width: 100%;
    background: var(--role-gauge-filled);
    transition: height 160ms ease;
  }
  .band:hover .col {
    outline: 1px solid var(--role-border-focused);
  }
  .freq {
    font-size: 9px;
    color: var(--role-text-subtle);
  }
  .bank-hint {
    flex-basis: 100%;
    margin: var(--space-2) 0 0;
    font-size: 10.5px;
    color: var(--role-text-subtle);
  }
</style>
