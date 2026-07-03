// The patch-bay contract (gui/WIRING.md): the registry and the TODO(wire:) markers in
// the source may not drift. Deleting a registry entry forces cleaning up its call
// sites; adding a marker forces registering the feature.

import { describe, expect, it } from 'vitest';
import { FEATURE_IDS, WIRING, agentBrief, marker } from '../src/lib/wiring/registry';

// Vite loads every source file as a raw string at collect time — no node:fs needed.
const RAW = import.meta.glob(['../src/**/*.ts', '../src/**/*.svelte', '../src/**/*.css'], {
  eager: true,
  query: '?raw',
  import: 'default',
}) as Record<string, string>;

const files = Object.entries(RAW)
  .filter(([path]) => !path.includes('/generated/'))
  .map(([path, text]) => ({ path, text }));
const allText = files.map((f) => f.text).join('\n');

describe('wiring registry', () => {
  it('scans a plausible number of source files', () => {
    expect(files.length).toBeGreaterThan(30);
  });

  it('every entry is fully specified', () => {
    for (const id of FEATURE_IDS) {
      const w = WIRING[id];
      expect(w.title, id).toBeTruthy();
      expect(w.milestone, id).toMatch(/^(M[1-5]|B[1-3])$/);
      expect(w.brief, id).toContain('docs/gui/');
      expect(w.protocol.length, id).toBeGreaterThan(10);
      expect(w.seam.length, id).toBeGreaterThan(10);
    }
  });

  it('every TODO(wire:) marker in src refers to a registered feature, milestone included', () => {
    const re = /TODO\(wire:([A-Z0-9]+)\/([a-z0-9.-]+)\)/g;
    let sawAny = false;
    for (const f of files) {
      for (const m of f.text.matchAll(re)) {
        sawAny = true;
        const [, milestone, id] = m;
        expect(FEATURE_IDS, `${f.path} marks unregistered feature "${id}"`).toContain(id);
        expect(milestone, `${f.path}: marker milestone drifted for "${id}"`).toBe(
          WIRING[id as (typeof FEATURE_IDS)[number]].milestone,
        );
      }
    }
    expect(sawAny).toBe(true);
  });

  it('every registered feature is referenced somewhere in src', () => {
    for (const id of FEATURE_IDS) {
      expect(
        allText.includes(id),
        `registry entry "${id}" has no call site — wire done? delete it`,
      ).toBe(true);
    }
  });

  it('agentBrief carries the grep marker, the spec ref, and the gates', () => {
    for (const id of FEATURE_IDS) {
      const brief = agentBrief(id);
      expect(brief).toContain(marker(id));
      expect(brief).toContain(WIRING[id].brief);
      expect(brief).toContain('npm run check && npm test && npm run build');
      expect(brief).toContain('democore');
    }
  });
});
