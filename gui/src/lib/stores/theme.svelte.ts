// Applies the 34 theme roles to CSS custom properties on <html>, from the boot payload AND
// live `settings` theme pushes (docs/gui/06 §1–2). The core's 13 presets stay core-resolved
// (the frontend embeds none of them — the zero-palette rule); the LOCAL themes
// (lib/theme/local.ts) are a separate frontend-owned skin set applied through the same
// pipeline, persisted per window in localStorage.
//
// ── PROVISIONAL wire shape (settings.theme-editor) ───────────────────────────────────────
// The `theme` block rides the `settings` snapshot (docs/gui/02 §11.6). Only the demo core
// speaks it today; reconcile with the ts-rs `ThemeModel`/`ThemeRoleModel` when settings-v8
// lands. The 34 resolved hexes are the *output* of `ThemeConfig::effective_hex` core-side —
// the GUI never resolves a preset, it only paints what the model reports.
//
// Precedence (resolves the gui/WIRING.md open question): the core `settings.theme` push is
// authoritative for the core theme and always mirrored into `model`. A chosen local skin
// applies ON TOP and keeps `localId` set — it survives pushes (a push updates `model` for the
// editor to read, but does not repaint) until the user touches the core theme editor
// (picks a preset / edits or clears a role / toggles background-none), which hands control
// back to the core theme. While `localId === null`, every push repaints from `model.roles`.

import type { BootTheme } from '../ipc/boot';
import type { Client } from '../ipc/client';
import type { SettingsSnapshot } from './settings.svelte';
import { DEFAULT_LOCAL_THEME, localTheme, type LocalTheme } from '../theme/local';

const STORAGE_KEY = 'ytm-tui.gui.local-theme';

/** One preset's gallery preview — a handful of role hexes to paint the swatch card. */
export interface ThemePresetPreview {
  name: string;
  swatch: Record<string, string>;
}

/** The `theme` block of the `settings` snapshot (PROVISIONAL — see the file header). */
export interface ThemeModel {
  /** Active core preset name (one of `presets[*].name`). */
  preset: string;
  /** The 34 resolved hexes, `"none"` preserved for transparent roles. */
  roles: Record<string, string>;
  /** Per-role user overrides (a subset of `roles`) — drives the "overridden" badge. */
  overrides: Record<string, string>;
  /** Terminal transparency → the GUI substitutes the preset's opaque background. */
  background_none: boolean;
  /** TUI-only console mode; the GUI shows the toggle (tagged) but paint is unchanged. */
  retro: boolean;
  /** Gallery preview palettes, core-provided (the GUI embeds no preset colors). */
  presets: ThemePresetPreview[];
}

export class ThemeStore {
  applied = $state(false);
  /** Active local-theme id, or null when the boot/core theme is in effect. */
  localId = $state<string | null>(null);
  /** Last authoritative core theme from the `settings` push; null until the first one. */
  model = $state<ThemeModel | null>(null);

  constructor(client?: Client) {
    client?.on('settings', (payload) => this.#onPush(payload as SettingsSnapshot));
  }

  /** Boot order: core-provided theme first, then the user's chosen local skin on top. */
  boot(bootTheme: BootTheme | null): void {
    this.apply(bootTheme);
    const saved = storedThemeId();
    const t = saved ? localTheme(saved) : null;
    if (t) this.applyLocal(t);
    else if (!bootTheme) this.applyLocal(DEFAULT_LOCAL_THEME);
  }

  applyLocal(t: LocalTheme): void {
    this.#paint(t.roles, t.scheme);
    this.localId = t.id;
    try {
      localStorage.setItem(STORAGE_KEY, t.id);
    } catch {
      // Storage can be unavailable under a custom scheme — the theme still applies.
    }
  }

  apply(theme: BootTheme | null): void {
    if (!theme) return;
    this.#paint(theme.roles, theme.colorScheme ?? null);
  }

  // ── core theme editor (settings.theme-editor) ────────────────────────────────────────

  /** Switch the core preset. Palette is core-resolved, so no optimistic repaint — the push
   *  (< 100 ms) carries the resolved roles. Hands control back from any local skin. */
  setPreset(name: string, client: Client): void {
    this.#leaveLocal();
    client.cmd('apply', { change: { group: 'theme', field: 'preset', value: name } });
  }

  /** Override one role. We hold the hex, so apply it to the CSS var immediately (optimistic,
   *  < 100 ms target) and reconcile on the confirming push. */
  setOverride(role: string, hex: string, client: Client): void {
    this.#leaveLocal();
    document.documentElement.style.setProperty(
      `--role-${role}`,
      hex === 'none' ? 'transparent' : hex,
    );
    client.cmd('theme_set_override', { role, hex });
  }

  /** Drop a role's override back to the preset value — reconciled on the push. */
  clearOverride(role: string, client: Client): void {
    this.#leaveLocal();
    client.cmd('theme_clear_override', { role });
  }

  setBackgroundNone(on: boolean, client: Client): void {
    this.#leaveLocal();
    client.cmd('apply', { change: { group: 'theme', field: 'background_none', value: on } });
  }

  /** RetroMode is a TUI-only concept — round-trip the toggle, GUI paint is unchanged. */
  setRetro(on: boolean, client: Client): void {
    client.cmd('apply', { change: { group: 'theme', field: 'retro', value: on } });
  }

  /** True if `role` carries a user override in the authoritative model. */
  isOverridden(role: string): boolean {
    return this.model != null && role in this.model.overrides;
  }

  // ── internals ────────────────────────────────────────────────────────────────────────

  /** Leave any active local skin and repaint the core theme so the editor operates on it. */
  #leaveLocal(): void {
    if (this.localId == null) return;
    this.localId = null;
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      // ignore — best effort
    }
    if (this.model) this.#paint(this.model.roles, null);
  }

  #onPush(snap: SettingsSnapshot): void {
    const theme = snap?.model?.theme;
    if (!theme) return;
    this.model = theme;
    // A local skin owns the screen until the user edits the core theme; a push still updates
    // `model` (so the editor reads core values) but does not repaint under a local skin.
    if (this.localId == null) this.#paint(theme.roles, null);
  }

  /** Write the 34 role vars + JS-computed surface tints (oklab) + color-scheme to <html>. */
  #paint(roles: Record<string, string>, scheme: 'light' | 'dark' | null): void {
    const root = document.documentElement;
    for (const [role, hex] of Object.entries(roles)) {
      root.style.setProperty(`--role-${role}`, hex === 'none' ? 'transparent' : hex);
    }

    const bg = roles['background'];
    const borderMuted = roles['border-muted'];
    if (bg && borderMuted && bg !== 'none') {
      // Surface tints: mix the background toward border-muted (6% / 12%) in OKLab, so cards
      // read as subtly raised without a second palette. OKLab (not CSS `color-mix()`) so the
      // WebKit that ships without `color-mix` still paints correctly (docs/gui/06 §1).
      root.style.setProperty('--surface-1', mixOklab(bg, borderMuted, 0.06));
      root.style.setProperty('--surface-2', mixOklab(bg, borderMuted, 0.12));
    }

    const eff = scheme ?? (luminance(bg) > 0.5 ? 'light' : 'dark');
    root.style.setProperty('color-scheme', eff);
    root.dataset.theme = eff;
    this.applied = true;
  }
}

// ── color math ─────────────────────────────────────────────────────────────────────────

function parseHex(hex: string | undefined): [number, number, number] | null {
  if (!hex) return null;
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return null;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff];
}

function toHex(rgb: [number, number, number]): string {
  return (
    '#' +
    rgb
      .map((v) =>
        Math.max(0, Math.min(255, Math.round(v)))
          .toString(16)
          .padStart(2, '0'),
      )
      .join('')
  );
}

// sRGB 8-bit ⇄ linear-light, the standard transfer functions.
function toLinear(c: number): number {
  const s = c / 255;
  return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
}
function fromLinear(c: number): number {
  const s = c <= 0.0031308 ? c * 12.92 : 1.055 * Math.pow(c, 1 / 2.4) - 0.055;
  return s * 255;
}

// Linear sRGB → OKLab and back (Björn Ottosson's matrices).
function linearToOklab(r: number, g: number, b: number): [number, number, number] {
  const l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
  const m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
  const s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;
  const l_ = Math.cbrt(l);
  const m_ = Math.cbrt(m);
  const s_ = Math.cbrt(s);
  return [
    0.2104542553 * l_ + 0.793617785 * m_ - 0.0040720468 * s_,
    1.9779984951 * l_ - 2.428592205 * m_ + 0.4505937099 * s_,
    0.0259040371 * l_ + 0.7827717662 * m_ - 0.808675766 * s_,
  ];
}
function oklabToLinear(L: number, a: number, b: number): [number, number, number] {
  const l_ = L + 0.3963377774 * a + 0.2158037573 * b;
  const m_ = L - 0.1055613458 * a - 0.0638541728 * b;
  const s_ = L - 0.0894841775 * a - 1.291485548 * b;
  const l = l_ * l_ * l_;
  const m = m_ * m_ * m_;
  const s = s_ * s_ * s_;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
}

/** Mix two hex colors in OKLab by `t` (0 → a, 1 → b). Falls back to `a` on unparseable input. */
export function mixOklab(a: string, b: string, t: number): string {
  const ca = parseHex(a);
  const cb = parseHex(b);
  if (!ca || !cb) return a;
  const la = linearToOklab(toLinear(ca[0]), toLinear(ca[1]), toLinear(ca[2]));
  const lb = linearToOklab(toLinear(cb[0]), toLinear(cb[1]), toLinear(cb[2]));
  const [lr, lg, lbl] = oklabToLinear(
    la[0] + (lb[0] - la[0]) * t,
    la[1] + (lb[1] - la[1]) * t,
    la[2] + (lb[2] - la[2]) * t,
  );
  return toHex([fromLinear(lr), fromLinear(lg), fromLinear(lbl)]);
}

function storedThemeId(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function luminance(hex: string | undefined): number {
  const c = parseHex(hex);
  if (!c) return 0;
  // Rec. 601 luma, good enough to choose a light/dark color-scheme.
  return (0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]) / 255;
}
