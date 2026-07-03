// Applies the 34 theme roles from the boot payload (and, later, live `settings` pushes) to
// CSS custom properties on <html>, computing surface tints in JS (docs/gui/06 §1–2). The
// frontend embeds zero preset palettes — the core resolves preset+overrides to hex per role.

import type { BootTheme } from '../ipc/boot';

export class ThemeStore {
  applied = $state(false);

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
  return '#' + rgb.map((v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, '0')).join('');
}

function mix(a: string, b: string, t: number): string {
  const ca = parseHex(a);
  const cb = parseHex(b);
  if (!ca || !cb) return a;
  return toHex([ca[0] + (cb[0] - ca[0]) * t, ca[1] + (cb[1] - ca[1]) * t, ca[2] + (cb[2] - ca[2]) * t]);
}

function luminance(hex: string | undefined): number {
  const c = parseHex(hex);
  if (!c) return 0;
  // Rec. 601 luma, good enough to choose a light/dark color-scheme.
  return (0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]) / 255;
}
