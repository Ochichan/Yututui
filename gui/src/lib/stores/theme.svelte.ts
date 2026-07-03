// Applies the 34 theme roles from the boot payload (and, later, live `settings` pushes) to
// CSS custom properties on <html>, computing surface tints in JS (docs/gui/06 §1–2). The
// core's 13 presets stay core-resolved (the frontend embeds none of them); the LOCAL
// themes (lib/theme/local.ts) are a separate frontend-owned skin set applied through the
// same pipeline, persisted per window in localStorage.
//
// TODO(wire:M3/settings.theme-editor): on the live `settings` theme push, decide
// precedence — a chosen local skin currently wins over the boot payload; the wiring agent
// reconciles this with core-side theme state (see gui/WIRING.md).

import type { BootTheme } from '../ipc/boot';
import { DEFAULT_LOCAL_THEME, localTheme, type LocalTheme } from '../theme/local';

const STORAGE_KEY = 'ytm-tui.gui.local-theme';

export class ThemeStore {
  applied = $state(false);
  /** Active local-theme id, or null when the boot/core theme is in effect. */
  localId = $state<string | null>(null);

  /** Boot order: core-provided theme first, then the user's chosen local skin on top. */
  boot(bootTheme: BootTheme | null): void {
    this.apply(bootTheme);
    const saved = storedThemeId();
    const t = saved ? localTheme(saved) : null;
    if (t) this.applyLocal(t);
    else if (!bootTheme) this.applyLocal(DEFAULT_LOCAL_THEME);
  }

  applyLocal(t: LocalTheme): void {
    this.apply({ roles: t.roles, colorScheme: t.scheme });
    this.localId = t.id;
    try {
      localStorage.setItem(STORAGE_KEY, t.id);
    } catch {
      // Storage can be unavailable under a custom scheme — the theme still applies.
    }
  }

  apply(theme: BootTheme | null): void {
    if (!theme) return;
    const root = document.documentElement;
    for (const [role, hex] of Object.entries(theme.roles)) {
      root.style.setProperty(`--role-${role}`, hex === 'none' ? 'transparent' : hex);
    }

    const bg = theme.roles['background'];
    const borderMuted = theme.roles['border-muted'];
    if (bg && borderMuted && bg !== 'none') {
      // Surface tints: mix the background toward border-muted (6% / 12%), so cards read as
      // subtly raised without a second palette. sRGB mix is fine for the skeleton; the M3
      // theme editor upgrades this to oklab.
      root.style.setProperty('--surface-1', mix(bg, borderMuted, 0.06));
      root.style.setProperty('--surface-2', mix(bg, borderMuted, 0.12));
    }

    const scheme = theme.colorScheme ?? (luminance(bg) > 0.5 ? 'light' : 'dark');
    root.style.setProperty('color-scheme', scheme);
    root.dataset.theme = scheme;
    this.applied = true;
  }
}

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

function mix(a: string, b: string, t: number): string {
  const ca = parseHex(a);
  const cb = parseHex(b);
  if (!ca || !cb) return a;
  return toHex([
    ca[0] + (cb[0] - ca[0]) * t,
    ca[1] + (cb[1] - ca[1]) * t,
    ca[2] + (cb[2] - ca[2]) * t,
  ]);
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
