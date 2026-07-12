<script lang="ts">
  // Settings → Playback (docs/gui/07 §7). Binds the `settings` read model — the persisted
  // startup defaults, NOT the live player state (docs/gui/02 §11.6 binding rule: the
  // transport bar owns the live values; the Settings screen owns the defaults, and they may
  // legitimately differ). Live-apply: every change sends `apply` and shows optimistically.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';
  import { t } from '../../lib/i18n.svelte';
  import { EQ_MAX, EQ_MIN, clampGain, gainAtPointer } from '../../lib/eq';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { settings } = ctx;

  const BAND_LABELS = ['31', '62', '125', '250', '500', '1k', '2k', '4k', '8k', '16k'];
  const pb = $derived(settings.playback);
  const eq = $derived(settings.eq);
  const audio = $derived(settings.audio);
  const speed = $derived((pb?.speed_tenths ?? 10) / 10);
  const SEEKS = [5, 10, 30];

  // Drag overlay: while a band is being dragged, render from this local array at
  // pointer speed and send one `apply` per gesture on release (docs/gui/07 §7 —
  // the server accepts the full 10-band array via the settings apply path).
  let dragBands: number[] | null = $state(null);
  const cols: HTMLElement[] = [];

  const bandGain = (i: number): number => (dragBands ? dragBands[i] : (eq?.bands[i] ?? 0));
  const fmtGain = (gain: number): string => `${gain > 0 ? '+' : ''}${gain}`;

  /** Send the full band array with one band changed (wheel / keyboard / dblclick). */
  function applyBand(i: number, value: number): void {
    if (!eq) return;
    const bands = [...eq.bands];
    bands[i] = clampGain(value);
    settings.apply('eq', 'bands', bands);
  }

  function zeroBand(i: number): void {
    applyBand(i, 0);
  }

  function bandPointerDown(i: number, e: PointerEvent): void {
    if (!eq) return;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    const bands = [...eq.bands];
    bands[i] = gainAtPointer(cols[i].getBoundingClientRect(), e.clientY);
    dragBands = bands;
  }

  function bandPointerMove(i: number, e: PointerEvent): void {
    if (!dragBands) return;
    const next = [...dragBands];
    next[i] = gainAtPointer(cols[i].getBoundingClientRect(), e.clientY);
    dragBands = next;
  }

  function bandPointerEnd(): void {
    if (!dragBands) return;
    settings.apply('eq', 'bands', dragBands);
    dragBands = null;
  }

  function bandKey(i: number, e: KeyboardEvent): void {
    let next: number;
    if (e.key === 'ArrowUp' || e.key === 'ArrowRight') next = bandGain(i) + 1;
    else if (e.key === 'ArrowDown' || e.key === 'ArrowLeft') next = bandGain(i) - 1;
    else if (e.key === 'PageUp') next = bandGain(i) + 3;
    else if (e.key === 'PageDown') next = bandGain(i) - 3;
    else if (e.key === 'Home') next = 0;
    else return;
    e.preventDefault();
    applyBand(i, next);
  }

  /** Svelte registers `onwheel` passively; the EQ needs preventDefault, so bind manually. */
  function bandWheel(node: HTMLElement, i: number) {
    const handler = (e: WheelEvent) => {
      e.preventDefault();
      applyBand(i, bandGain(i) + (e.deltaY < 0 ? 1 : -1));
    };
    node.addEventListener('wheel', handler, { passive: false });
    return {
      destroy() {
        node.removeEventListener('wheel', handler);
      },
    };
  }
</script>

<SettingSection title={t('settings.playback.nowPlaying')}>
  <SettingRow label={t('settings.playback.speed')} hint={t('settings.playback.speedHint')}>
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
  <SettingRow
    label={t('settings.playback.seekInterval')}
    hint={t('settings.playback.seekIntervalHint')}
  >
    <select
      class="sel"
      onchange={(e) => settings.apply('playback', 'seek_seconds', Number(e.currentTarget.value))}
    >
      {#each SEEKS as s (s)}
        <option value={s} selected={(pb?.seek_seconds ?? 5) === s}
          >{t('settings.playback.secondsValue', { n: s })}</option
        >
      {/each}
    </select>
  </SettingRow>
  <SettingRow
    label={t('settings.playback.wheelVolume')}
    hint={t('settings.playback.wheelVolumeHint')}
  >
    <Toggle
      checked={pb?.mouse_wheel_volume ?? true}
      onchange={(v) => settings.apply('playback', 'mouse_wheel_volume', v)}
    />
  </SettingRow>
  <SettingRow label={t('settings.playback.gapless')}>
    <Toggle
      checked={pb?.gapless ?? true}
      onchange={(v) => settings.apply('playback', 'gapless', v)}
    />
  </SettingRow>
  <SettingRow
    label={t('settings.playback.mediaControls')}
    hint={t('settings.playback.mediaControlsHint')}
  >
    <Toggle
      checked={pb?.media_controls ?? true}
      onchange={(v) => settings.apply('playback', 'media_controls', v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.playback.audioBackend')}>
  <SettingRow label={t('settings.playback.backend')} hint={t('settings.playback.backendHint')}>
    <span class="val mono">{audio?.backend ?? 'mpv'}</span>
  </SettingRow>
  <SettingRow label={t('settings.playback.mpvOutput')} hint={t('settings.playback.mpvOutputHint')}>
    <input
      class="text"
      type="text"
      value={audio?.mpv_output ?? ''}
      placeholder="auto"
      onchange={(e) => settings.apply('audio', 'mpv_output', e.currentTarget.value || null)}
    />
  </SettingRow>
  <SettingRow label={t('settings.playback.mpvDevice')} hint={t('settings.playback.mpvDeviceHint')}>
    <input
      class="text"
      type="text"
      value={audio?.mpv_device ?? ''}
      placeholder="auto"
      onchange={(e) => settings.apply('audio', 'mpv_device', e.currentTarget.value || null)}
    />
  </SettingRow>
  <SettingRow label={t('settings.playback.cacheForward')} hint={t('settings.playback.cacheHint')}>
    <input
      class="short-text"
      type="text"
      value={audio?.mpv_cache_forward ?? '32MiB'}
      onchange={(e) => settings.apply('audio', 'mpv_cache_forward', e.currentTarget.value)}
    />
  </SettingRow>
  <SettingRow label={t('settings.playback.cacheBack')} hint={t('settings.playback.cacheHint')}>
    <input
      class="short-text"
      type="text"
      value={audio?.mpv_cache_back ?? '8MiB'}
      onchange={(e) => settings.apply('audio', 'mpv_cache_back', e.currentTarget.value)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.playback.equalizer')}>
  <SettingRow label={t('settings.playback.preset')} hint={t('settings.playback.presetHint')}>
    <select class="sel" onchange={(e) => settings.apply('eq', 'preset', e.currentTarget.value)}>
      {#each ['flat', 'bass', 'vocal', 'rock', 'custom'] as preset (preset)}
        <option value={preset} selected={(eq?.preset ?? 'flat') === preset}>{preset}</option>
      {/each}
    </select>
  </SettingRow>
  <SettingRow label={t('settings.playback.normalize')}>
    <Toggle
      checked={eq?.normalize ?? false}
      onchange={(v) => settings.apply('eq', 'normalize', v)}
    />
  </SettingRow>
  <div class="bank" role="group" aria-label={t('settings.playback.eqBandsAria')}>
    {#each BAND_LABELS as label, i (label)}
      {@const gain = bandGain(i)}
      <div
        class="band"
        role="slider"
        tabindex="0"
        aria-label={t('settings.playback.bandAria', { freq: label })}
        aria-valuemin={EQ_MIN}
        aria-valuemax={EQ_MAX}
        aria-valuenow={gain}
        aria-valuetext="{fmtGain(gain)} dB"
        title={t('settings.playback.bandTitle', { freq: label, gain: fmtGain(gain) })}
        onpointerdown={(e) => bandPointerDown(i, e)}
        onpointermove={(e) => bandPointerMove(i, e)}
        onpointerup={bandPointerEnd}
        onpointercancel={bandPointerEnd}
        onkeydown={(e) => bandKey(i, e)}
        ondblclick={() => zeroBand(i)}
        use:bandWheel={i}
      >
        <span class="col" bind:this={cols[i]}>
          <span class="fill" style:height="{((gain + 12) / 24) * 100}%"></span>
        </span>
        <span class="gain mono">{fmtGain(gain)}</span>
        <span class="freq mono">{label}</span>
      </div>
    {/each}
    <p class="bank-hint">{t('settings.playback.bankHint')}</p>
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
  .text {
    width: 180px;
  }
  .short-text {
    width: 84px;
  }
  .text,
  .short-text {
    min-height: 30px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-s);
    background: var(--surface-0);
    color: var(--role-settings-value);
    padding: 0 var(--space-2);
    font: inherit;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .bank {
    display: flex;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
    padding: var(--space-4);
  }
  .band {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-1);
    border: none;
    border-radius: var(--radius-s);
    background: transparent;
    padding: var(--space-1);
    cursor: ns-resize;
    touch-action: none;
    user-select: none;
  }
  .band:focus-visible {
    outline: 1px solid var(--role-border-focused);
  }
  .col {
    position: relative;
    width: 18px;
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
    transition: height 120ms ease;
  }
  .band:hover .col {
    outline: 1px solid var(--role-border-focused);
  }
  .gain {
    font-size: 9.5px;
    color: var(--role-settings-value);
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
