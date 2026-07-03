<script lang="ts">
  // Settings → Playback (docs/gui/07 §7). The EQ bank reads LIVE values from the player
  // topic (playback.model.eq) — the display side is already wired; only mutations wait
  // on settings-v8.
  //
  // TODO(wire:M3/settings.apply): send Apply(Playback(...)) / Apply(Eq(Preset|Bands)).
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip, playback } = ctx;

  const stub = () => wip.gate('settings.apply');

  const BAND_LABELS = ['31', '62', '125', '250', '500', '1k', '2k', '4k', '8k', '16k'];
  const eq = $derived(playback.model?.eq ?? null);
  const speed = $derived((playback.model?.speed_tenths ?? 10) / 10);
</script>

<SettingSection title="Now playing">
  <SettingRow label="Playback speed" hint="0.5× – 2.0×, step 0.1">
    <input class="range" type="range" min="0.5" max="2" step="0.1" value={speed} onchange={stub} />
    <span class="val mono">{speed.toFixed(1)}×</span>
  </SettingRow>
  <SettingRow label="Seek interval" hint="Arrow-key / button seek step">
    <select class="sel" onchange={stub}>
      <option>5 s</option>
      <option>10 s</option>
      <option>30 s</option>
    </select>
  </SettingRow>
  <SettingRow label="Wheel volume" hint="Mouse wheel over the player adjusts volume">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="Gapless playback">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
  <SettingRow label="OS media controls" hint="Media keys + system now-playing integration">
    <Toggle checked={true} onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Equalizer">
  <SettingRow label="Preset" hint="Applies live through mpv's audio filter chain">
    <select class="sel" onchange={stub}>
      <option selected={eq?.preset === 'flat' || eq == null}>flat</option>
      <option selected={eq?.preset === 'bass'}>bass</option>
      <option selected={eq?.preset === 'vocal'}>vocal</option>
      <option selected={eq?.preset === 'rock'}>rock</option>
      <option selected={eq?.preset === 'custom'}>custom</option>
    </select>
  </SettingRow>
  <SettingRow label="Normalize loudness">
    <Toggle checked={eq?.normalize ?? false} onchange={stub} />
  </SettingRow>
  <div class="bank" role="group" aria-label="EQ bands">
    {#each BAND_LABELS as label, i (label)}
      {@const gain = eq?.bands[i] ?? 0}
      <button class="band" onclick={stub} title="{label} Hz · {gain > 0 ? '+' : ''}{gain} dB">
        <span class="col">
          <span class="fill" style:height="{((gain + 12) / 24) * 100}%"></span>
        </span>
        <span class="freq mono">{label}</span>
      </button>
    {/each}
    <p class="bank-hint">
      ±12 dB per ISO band — live values from the player; editing lands with settings-v8
    </p>
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
