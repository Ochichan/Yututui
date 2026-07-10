<script lang="ts">
  // Settings → Graphics (docs/gui/07 §9): preset gallery, the 34-role color editor, and
  // the 25-effect animation grid — all live now.
  //
  // The LOCAL themes section is frontend-owned skins (applied live, persisted). The core
  // theme is wired (settings.theme-editor): the preset gallery renders from the pushed
  // `theme.presets` previews and switching applies live; the 34-role editor writes optimistic
  // CSS-var overrides reconciled by the `settings` push (stores/theme.svelte.ts owns the
  // pipeline + local-skin precedence). The Animations block binds the live `settings`
  // animations model via `apply { group: 'animations' }` (stores/anim.svelte.ts).
  import type { AppCtx } from '../../lib/ctx';
  import { LOCAL_THEMES } from '../../lib/theme/local';
  import { ROLES } from '../../lib/theme/roles';
  import { EFFECT_GROUPS, FPS_MIN, FPS_MAX, FPS_DEFAULT } from '../../lib/stores/anim.svelte';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { theme, toasts, settings, client } = ctx;

  const anim = $derived(settings.animations);

  // A role's current hex: the authoritative core theme when we have it, else whatever the
  // window is painted with (before the first push / under a local skin fallback).
  function liveHex(role: string): string {
    void theme.localId;
    return getComputedStyle(document.documentElement).getPropertyValue(`--role-${role}`).trim();
  }
  function roleHex(role: string): string {
    return theme.model?.roles[role] ?? liveHex(role);
  }
  // The native <input type=color> needs a 6-hex; transparent/none roles fall back to black.
  function colorFor(role: string): string {
    const h = roleHex(role);
    return /^#[0-9a-f]{6}$/i.test(h) ? h : '#000000';
  }

  const ANIM_GROUPS = $derived(
    EFFECT_GROUPS.map((group) => ({
      title: t(`settings.graphics.animGroup.${group.id}`),
      tui: group.id === 'filler',
      effects: group.effects,
    })),
  );
  const TUI_ONLY = new Set(['rain', 'donut', 'starfield', 'bounce']);
</script>

<SettingSection title={t('settings.graphics.localThemes')}>
  <div class="gallery">
    {#each LOCAL_THEMES as lt (lt.id)}
      <button
        class="preset local"
        class:on={theme.localId === lt.id}
        style:background={lt.roles['background']}
        style:color={lt.roles['text-primary']}
        onclick={() => {
          theme.applyLocal(lt);
          toasts.show('success', t('settings.graphics.themeApplied', { name: lt.name }));
        }}
      >
        <span class="strip">
          <i style:background={lt.roles['accent']}></i>
          <i style:background={lt.roles['accent-alt']}></i>
          <i style:background={lt.roles['success']}></i>
          <i style:background={lt.roles['warning']}></i>
          <i style:background={lt.roles['error']}></i>
        </span>
        <span class="pname">{lt.name}</span>
        <span class="ptag" style:color={lt.roles['text-muted']}>{lt.tagline}</span>
      </button>
    {/each}
    <p class="gallery-hint">
      {t('settings.graphics.localThemesHint')}
    </p>
  </div>
</SettingSection>

<SettingSection title={t('settings.graphics.corePresets')}>
  <div class="gallery">
    {#each theme.model?.presets ?? [] as p (p.name)}
      <button
        class="preset"
        class:on={theme.localId === null && theme.model?.preset === p.name}
        style:background={p.swatch['background']}
        style:color={p.swatch['text-primary']}
        onclick={() => theme.setPreset(p.name, client)}
      >
        <span class="strip">
          <i style:background={p.swatch['accent']}></i>
          <i style:background={p.swatch['accent-alt']}></i>
          <i style:background={p.swatch['success']}></i>
          <i style:background={p.swatch['warning']}></i>
          <i style:background={p.swatch['error']}></i>
        </span>
        <span class="pname">{p.label}</span>
      </button>
    {/each}
    <p class="gallery-hint">{t('settings.graphics.corePresetsHint')}</p>
  </div>
  <SettingRow
    label={t('settings.graphics.backgroundNone')}
    hint={t('settings.graphics.backgroundNoneHint')}
  >
    <Toggle
      checked={theme.model?.background_none ?? false}
      onchange={(v) => theme.setBackgroundNone(v, client)}
    />
  </SettingRow>
  <SettingRow label="Retro mode" tag="(TUI)" hint={t('settings.graphics.retroModeHint')}>
    <Toggle checked={theme.model?.retro ?? false} onchange={(v) => theme.setRetro(v, client)} />
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.graphics.colors')}>
  <div class="roles">
    {#each ROLES as [role, label] (role)}
      {@const hex = roleHex(role)}
      {@const over = theme.isOverridden(role)}
      <div class="role-row" class:over>
        <label class="swatch-wrap" title={t('settings.graphics.editRole', { role: label })}>
          <span class="swatch" style:background={hex || 'transparent'}></span>
          <input
            class="color-in"
            type="color"
            value={colorFor(role)}
            onchange={(e) => theme.setOverride(role, e.currentTarget.value, client)}
            aria-label={t('settings.graphics.editRole', { role: label })}
          />
        </label>
        <span class="rl">{label}</span>
        <span class="hex mono">{hex || 'none'}</span>
        {#if over}
          <button
            class="reset"
            title={t('settings.graphics.resetToPreset')}
            onclick={() => theme.clearOverride(role, client)}>↺</button
          >
        {/if}
      </div>
    {/each}
  </div>
</SettingSection>

<SettingSection title={t('settings.graphics.animations')}>
  <SettingRow label={t('settings.graphics.master')} hint={t('settings.graphics.masterHint')}>
    <Toggle
      checked={anim?.master ?? false}
      onchange={(v) => settings.apply('animations', 'master', v)}
    />
  </SettingRow>
  <SettingRow label={t('settings.graphics.pauseUnfocused')}>
    <Toggle
      checked={anim?.pause_unfocused ?? true}
      onchange={(v) => settings.apply('animations', 'pause_unfocused', v)}
    />
  </SettingRow>
  {@const fps = anim?.fps ?? FPS_DEFAULT}
  <SettingRow label="FPS" hint={t('settings.graphics.fpsHint')}>
    <input
      class="range"
      type="range"
      min={FPS_MIN}
      max={FPS_MAX}
      step="5"
      value={fps}
      disabled={!(anim?.master ?? false)}
      onchange={(e) => settings.apply('animations', 'fps', e.currentTarget.valueAsNumber)}
    />
    <span class="val mono">{fps}</span>
  </SettingRow>
  <div class="anim-grid" class:dim={!(anim?.master ?? false)}>
    {#each ANIM_GROUPS as g (g.title)}
      <div class="ag">
        <h4>{g.title}</h4>
        {#each g.effects as fx (fx)}
          {@const on = anim?.[fx] ?? false}
          <button
            class="fx"
            class:tui={TUI_ONLY.has(fx)}
            aria-pressed={on}
            onclick={() => settings.apply('animations', fx, !on)}
          >
            <span class="dot" class:on></span>
            <span class="mono">{fx}</span>
            {#if TUI_ONLY.has(fx)}<span class="tui-tag">(TUI)</span>{/if}
          </button>
        {/each}
      </div>
    {/each}
  </div>
</SettingSection>

<style>
  .gallery {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(120px, 1fr));
    gap: var(--space-2);
    padding: var(--space-4);
  }
  .preset {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-m);
    background: var(--surface-0);
    color: var(--role-text-primary);
    font-size: 12px;
    text-align: left;
  }
  .preset:hover {
    border-color: var(--role-border-primary);
  }
  .preset.on {
    border-color: var(--role-accent);
    box-shadow: inset 0 0 0 1px var(--role-accent);
  }
  .pname {
    font-weight: 600;
  }
  .ptag {
    font-size: 10px;
    line-height: 1.35;
  }
  .strip {
    display: flex;
    gap: 3px;
  }
  .strip i {
    width: 14px;
    height: 14px;
    border-radius: 4px;
  }
  .gallery-hint {
    grid-column: 1 / -1;
    margin: 0;
    font-size: 10.5px;
    color: var(--role-text-subtle);
  }
  .roles {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
    gap: 1px;
    padding: var(--space-2);
  }
  .role-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border: none;
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--role-text-primary);
    font-size: 12px;
    text-align: left;
  }
  .role-row:hover {
    background: var(--surface-2);
  }
  .swatch-wrap {
    position: relative;
    width: 16px;
    height: 16px;
    flex: none;
    cursor: pointer;
  }
  .swatch {
    display: block;
    width: 16px;
    height: 16px;
    border-radius: 4px;
    border: 1px solid var(--role-border-muted);
  }
  /* The native color popover, laid transparently over the swatch — click the swatch to edit. */
  .color-in {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    padding: 0;
    border: none;
    opacity: 0;
    cursor: pointer;
  }
  .reset {
    flex: none;
    padding: 0 6px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--role-accent);
    font-size: 12px;
    line-height: 1.4;
  }
  .reset:hover {
    background: var(--surface-2);
  }
  .role-row.over .rl {
    color: var(--role-accent);
  }
  .rl {
    flex: 1;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hex {
    color: var(--role-text-subtle);
    font-size: 10.5px;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .range {
    width: 160px;
    accent-color: var(--role-accent);
  }
  .val {
    min-width: 24px;
    text-align: right;
    font-size: 12px;
  }
  .anim-grid {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4);
    transition: opacity 140ms ease;
  }
  /* Master off: the per-effect flags still edit, but read as inert (nothing animates). */
  .anim-grid.dim {
    opacity: 0.5;
  }
  .ag {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    column-gap: var(--space-3);
  }
  .ag h4 {
    grid-column: 1 / -1;
    margin: 0 0 var(--space-2);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--role-text-subtle);
  }
  .fx {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: 3px var(--space-2);
    border: none;
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--role-text-primary);
    font-size: 12px;
    text-align: left;
  }
  .fx:hover {
    background: var(--surface-2);
  }
  .fx.tui {
    color: var(--role-text-muted);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
    background: var(--role-gauge-empty);
  }
  .dot.on {
    background: var(--role-success);
  }
  .tui-tag {
    font-size: 9px;
    color: var(--role-text-subtle);
  }
</style>
