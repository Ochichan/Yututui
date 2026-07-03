<script lang="ts">
  // Settings → Graphics (docs/gui/07 §9): preset gallery, the 34-role color editor, and
  // the 25-effect animation grid. The color rows read LIVE values from the CSS custom
  // properties (what the window is actually painted with right now); edits gate through
  // the patch bay until settings-v8.
  //
  // The LOCAL themes section is fully functional (frontend-owned skins, applied live and
  // persisted); the core's 13 presets and per-role editing stay behind the patch bay.
  //
  // TODO(wire:M3/settings.theme-editor): Apply(Theme(...)) with optimistic CSS-var apply.
  // TODO(wire:M3/settings.animations): Apply(Animations(...)) + the anim store/ticker.
  import type { AppCtx } from '../../lib/ctx';
  import { LOCAL_THEMES } from '../../lib/theme/local';
  import { ROLES } from '../../lib/theme/roles';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip, theme, toasts } = ctx;

  const themeStub = () => wip.gate('settings.theme-editor');
  const animStub = () => wip.gate('settings.animations');

  const PRESETS = [
    'Default',
    'Retro',
    'Dario',
    'Midnight',
    'Light',
    'High Contrast',
    'Terminal Green',
    'Gruvbox',
    'Nord',
    'Dracula',
    'Tokyo Night',
    'Solarized',
    'Rosé Pine',
  ];

  // Live values: what the window is painted with right now (theme push or fallbacks).
  // Recomputed when the active local theme changes so the rows track the repaint.
  function liveHex(role: string): string {
    void theme.localId;
    return getComputedStyle(document.documentElement).getPropertyValue(`--role-${role}`).trim();
  }

  const ANIM_GROUPS: Array<{ title: string; tui?: boolean; effects: string[] }> = [
    {
      title: 'Element',
      effects: ['title', 'heart', 'seekbar', 'spinner', 'eq_bars', 'controls', 'border'],
    },
    {
      title: 'One-shot',
      effects: ['track_intro', 'lyrics', 'toast', 'volume_flash', 'like_burst', 'seek_flash'],
    },
    {
      title: 'UI-wide',
      effects: ['selection', 'stagger', 'caret', 'tabs', 'popup_fade', 'activity', 'about_fx'],
    },
    {
      title: 'Filler / terminal-only',
      tui: true,
      effects: ['visualizer', 'rain', 'donut', 'starfield', 'bounce'],
    },
  ];
  const TUI_ONLY = new Set(['rain', 'donut', 'starfield', 'bounce']);
</script>

<SettingSection title="Local themes — applies instantly">
  <div class="gallery">
    {#each LOCAL_THEMES as t (t.id)}
      <button
        class="preset local"
        class:on={theme.localId === t.id}
        style:background={t.roles['background']}
        style:color={t.roles['text-primary']}
        onclick={() => {
          theme.applyLocal(t);
          toasts.show('success', `Theme: ${t.name}`);
        }}
      >
        <span class="strip">
          <i style:background={t.roles['accent']}></i>
          <i style:background={t.roles['accent-alt']}></i>
          <i style:background={t.roles['success']}></i>
          <i style:background={t.roles['warning']}></i>
          <i style:background={t.roles['error']}></i>
        </span>
        <span class="pname">{t.name}</span>
        <span class="ptag" style:color={t.roles['text-muted']}>{t.tagline}</span>
      </button>
    {/each}
    <p class="gallery-hint">
      GUI-owned skins, saved per window. The core's presets below stay core-resolved.
    </p>
  </div>
</SettingSection>

<SettingSection title="Core presets">
  <div class="gallery">
    {#each PRESETS as name (name)}
      <button class="preset" onclick={themeStub}>
        <span class="strip">
          <i style:background="var(--role-accent)"></i>
          <i style:background="var(--role-accent-alt)"></i>
          <i style:background="var(--role-success)"></i>
          <i style:background="var(--role-warning)"></i>
          <i style:background="var(--role-error)"></i>
        </span>
        <span class="pname">{name}</span>
      </button>
    {/each}
    <p class="gallery-hint">
      The 13 TUI presets, resolved core-side — live previews + switching arrive with the settings
      wire (M3).
    </p>
  </div>
  <SettingRow
    label="Background: none"
    hint="(terminal transparency) — GUI substitutes the preset's opaque background"
  >
    <Toggle checked={false} onchange={themeStub} />
  </SettingRow>
  <SettingRow
    label="Retro mode"
    tag="(TUI)"
    hint="CP437-safe console rendering — does not change the GUI"
  >
    <Toggle checked={false} onchange={themeStub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Colors — 34 roles, live editor">
  <div class="roles">
    {#each ROLES as [role, label] (role)}
      {@const hex = liveHex(role)}
      <button class="role-row" onclick={themeStub} title="Edit {label}">
        <span class="swatch" style:background={hex || 'transparent'}></span>
        <span class="rl">{label}</span>
        <span class="hex mono">{hex || 'none'}</span>
      </button>
    {/each}
  </div>
</SettingSection>

<SettingSection title="Animations">
  <SettingRow label="Master" hint="Off cancels the animation loop outright — zero overhead">
    <Toggle checked={true} onchange={animStub} />
  </SettingRow>
  <SettingRow label="FPS" hint="5–60; one-shots run full rate, canvas 20, ambient 12">
    <input class="range" type="range" min="5" max="60" step="5" value={30} onchange={animStub} />
    <span class="val mono">30</span>
  </SettingRow>
  <SettingRow label="Pause when unfocused">
    <Toggle checked={true} onchange={animStub} />
  </SettingRow>
  <div class="anim-grid">
    {#each ANIM_GROUPS as g (g.title)}
      <div class="ag">
        <h4>{g.title}</h4>
        {#each g.effects as fx (fx)}
          <button class="fx" class:tui={TUI_ONLY.has(fx)} onclick={animStub}>
            <span class="dot on"></span>
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
  .swatch {
    width: 16px;
    height: 16px;
    flex: none;
    border-radius: 4px;
    border: 1px solid var(--role-border-muted);
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
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: var(--space-4);
    padding: var(--space-4);
  }
  .ag h4 {
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
