// Local GUI themes: every skin must define all 34 roles with concrete hex — a missing
// role would silently leave the previous theme's color painted.

import { describe, expect, it } from 'vitest';
import { LOCAL_THEMES, localTheme } from '../src/lib/theme/local';
import { ROLE_IDS } from '../src/lib/theme/roles';

describe('local themes', () => {
  it('every palette covers all 34 roles with valid hex', () => {
    for (const t of LOCAL_THEMES) {
      for (const role of ROLE_IDS) {
        expect(t.roles[role], `${t.id} is missing role "${role}"`).toMatch(/^#[0-9a-f]{6}$/);
      }
      expect(Object.keys(t.roles).length, `${t.id} defines unknown extra roles`).toBe(
        ROLE_IDS.length,
      );
    }
  });

  it('ids are unique and schemes valid', () => {
    const ids = LOCAL_THEMES.map((t) => t.id);
    expect(new Set(ids).size).toBe(ids.length);
    for (const t of LOCAL_THEMES) expect(['light', 'dark']).toContain(t.scheme);
  });

  it('ships the requested skins: black×red modern and orange×wine luxury', () => {
    expect(localTheme('crimson-mono')?.roles['accent']).toBe('#e5484d');
    expect(localTheme('ember-wine')?.roles['accent']).toBe('#e88b3a');
  });
});
