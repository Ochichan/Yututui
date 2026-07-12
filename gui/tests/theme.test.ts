// Theme editor wiring (docs/gui/06 §1–3, 07 §9). The store paints the 34 roles + OKLab
// surface tints from the `settings` theme push, switches presets, applies acknowledged
// per-role overrides, and settles the local-skin precedence rule.

import { afterEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { ThemeStore, mixOklab, type ThemeModel } from '../src/lib/stores/theme.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';

class MockTransport implements Transport {
  readonly live = false;
  sent: OutEnvelope[] = [];
  #cb: ((env: InEnvelope) => void) | null = null;
  send(env: OutEnvelope): void {
    this.sent.push(env);
  }
  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
  }
  emit(env: InEnvelope): void {
    this.#cb?.(env);
  }
}

function themeModel(over: Partial<ThemeModel> = {}): ThemeModel {
  return {
    preset: 'Default',
    roles: {
      background: '#101010',
      'border-muted': '#303030',
      accent: '#5b8cff',
      'text-primary': '#eeeeee',
    },
    overrides: {},
    background_none: false,
    retro: false,
    presets: [{ name: 'Default', label: 'Default', swatch: { accent: '#5b8cff' } }],
    ...over,
  };
}

function pushTheme(t: MockTransport, theme: ThemeModel): void {
  t.emit({
    v: 1,
    kind: 'event',
    topic: 'settings',
    payload: { kind: 'settings_snapshot', model: { theme } },
  });
}

function cssVar(name: string): string {
  return document.documentElement.style.getPropertyValue(name).trim();
}

const settle = () => new Promise((r) => setTimeout(r, 60));

afterEach(() => {
  document.documentElement.removeAttribute('style');
  try {
    localStorage.clear();
  } catch {
    // ignore
  }
});

describe('mixOklab', () => {
  it('returns the endpoints at t=0 and t=1', () => {
    expect(mixOklab('#000000', '#ffffff', 0)).toBe('#000000');
    expect(mixOklab('#123456', '#abcdef', 1)).toBe('#abcdef');
  });

  it('lands a gray strictly between black and white at the midpoint', () => {
    const mid = mixOklab('#000000', '#ffffff', 0.5);
    const n = parseInt(mid.slice(1), 16);
    const r = (n >> 16) & 0xff;
    const g = (n >> 8) & 0xff;
    const b = n & 0xff;
    expect(r).toBe(g);
    expect(g).toBe(b);
    expect(r).toBeGreaterThan(0);
    expect(r).toBeLessThan(255);
  });

  it('falls back to the first color on unparseable input', () => {
    expect(mixOklab('#5b8cff', 'transparent', 0.2)).toBe('#5b8cff');
  });
});

describe('ThemeStore push → paint', () => {
  it('mirrors the pushed theme into model and writes the role vars', () => {
    const t = new MockTransport();
    const store = new ThemeStore(new Client(t));
    expect(store.model).toBeNull();

    pushTheme(t, themeModel());
    expect(store.model?.preset).toBe('Default');
    expect(cssVar('--role-accent')).toBe('#5b8cff');
    // OKLab surface tints get concrete hex from background→border-muted.
    expect(cssVar('--surface-1')).toMatch(/^#[0-9a-f]{6}$/i);
    expect(cssVar('--surface-2')).toMatch(/^#[0-9a-f]{6}$/i);
  });

  it('reports overrides from the model', () => {
    const t = new MockTransport();
    const store = new ThemeStore(new Client(t));
    pushTheme(t, themeModel({ overrides: { accent: '#5b8cff' } }));
    expect(store.isOverridden('accent')).toBe(true);
    expect(store.isOverridden('error')).toBe(false);
  });
});

describe('ThemeStore against the demo core', () => {
  it('seeds all 13 presets and 34 roles, and switches preset live', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new ThemeStore(client);
    client.sub(['settings']);
    await settle();

    expect(store.model?.preset).toBe('Default');
    expect(store.model?.presets.length).toBe(13);
    expect(Object.keys(store.model?.roles ?? {}).length).toBe(34);

    store.setPreset('Nord', client);
    await settle();
    expect(store.model?.preset).toBe('Nord');
    expect(store.model?.roles.accent).toBe('#88c0d0');
    expect(cssVar('--role-accent')).toBe('#88c0d0');
  });

  it('applies an override after acknowledgement and reconciles it on the push', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new ThemeStore(client);
    client.sub(['settings']);
    await settle();

    store.setOverride('accent', '#123456', client);
    await settle();
    expect(cssVar('--role-accent')).toBe('#123456');
    expect(store.model?.overrides.accent).toBe('#123456');
    expect(store.isOverridden('accent')).toBe(true);
    expect(store.model?.roles.accent).toBe('#123456');

    store.clearOverride('accent', client);
    await settle();
    expect(store.isOverridden('accent')).toBe(false);
    // Reverts to the current preset's resolved value (Default accent).
    expect(store.model?.roles.accent).toBe('#5b8cff');
  });
});

describe('local-skin precedence', () => {
  it('a push does not repaint while a local skin is active, and acknowledged editing hands control back', async () => {
    const t = new MockTransport();
    const client = new Client(t);
    const store = new ThemeStore(client);

    store.applyLocal({
      id: 'crimson',
      name: 'Crimson',
      tagline: 'x',
      scheme: 'dark',
      roles: { accent: '#ff0033', background: '#0a0a0a', 'border-muted': '#222222' } as Record<
        string,
        string
      >,
    });
    expect(store.localId).toBe('crimson');
    expect(cssVar('--role-accent')).toBe('#ff0033');

    // A core theme push updates the model but must not repaint under the local skin.
    pushTheme(t, themeModel());
    expect(store.model?.preset).toBe('Default');
    expect(cssVar('--role-accent')).toBe('#ff0033');

    // Editing the core theme leaves the local skin and repaints the core theme + override.
    store.setOverride('accent', '#00ff88', client);
    expect(store.localId).toBe('crimson');
    const command = t.sent.at(-1)!;
    pushTheme(
      t,
      themeModel({
        roles: { ...themeModel().roles, accent: '#00ff88' },
        overrides: { accent: '#00ff88' },
      }),
    );
    t.emit({ v: 1, id: command.id, kind: 'res', payload: { ok: true } });
    await vi.waitFor(() => expect(store.localId).toBeNull());
    expect(cssVar('--role-accent')).toBe('#00ff88');
  });
});
