// i18n catalog wiring (docs/gui/05 §9, i18n.catalog): the en/ko catalogs stay in lockstep,
// carry no blanks, keep interpolation placeholders aligned, and cover every literal `t()`
// key the source references. Plus the reactive t()'s interpolation + fallback + live switch.

import { describe, expect, it } from 'vitest';
import en from '../src/i18n/en.json';
import ko from '../src/i18n/ko.json';
import { i18n, t } from '../src/lib/i18n.svelte';

const enCat = en as Record<string, string>;
const koCat = ko as Record<string, string>;
const enKeys = Object.keys(enCat).sort();
const koKeys = Object.keys(koCat).sort();

// Every literal t('some.key') reference in src (dynamic `t(`nav.${id}`)` keys are excluded).
const RAW = import.meta.glob(['../src/**/*.svelte', '../src/**/*.ts'], {
  eager: true,
  query: '?raw',
  import: 'default',
}) as Record<string, string>;
const allText = Object.entries(RAW)
  .filter(([p]) => !p.includes('/generated/') && !p.includes('/i18n.svelte'))
  .map(([, v]) => v)
  .join('\n');
const literalKeys = [...allText.matchAll(/\bt\(\s*'([a-z][\w.]*)'/g)].map((m) => m[1]);

const placeholders = (s: string) => (s.match(/\{[a-zA-Z0-9_]+\}/g) ?? []).sort();

describe('i18n catalog', () => {
  it('en and ko declare the identical key set', () => {
    expect(koKeys).toEqual(enKeys);
  });

  it('has a plausible number of keys', () => {
    expect(enKeys.length).toBeGreaterThan(80);
  });

  it('carries no blank strings', () => {
    for (const k of enKeys) {
      expect(enCat[k], `en.${k}`).toBeTruthy();
      expect(koCat[k], `ko.${k}`).toBeTruthy();
    }
  });

  it('keeps interpolation placeholders aligned across languages', () => {
    for (const k of enKeys) {
      expect(placeholders(koCat[k]), `placeholders for ${k}`).toEqual(placeholders(enCat[k]));
    }
  });

  it('covers every literal t() key referenced in src', () => {
    for (const k of new Set(literalKeys)) {
      expect(enKeys, `source references t('${k}') but the catalog has no such key`).toContain(k);
    }
  });
});

describe('t()', () => {
  it('interpolates params and falls back to the raw key when missing', () => {
    i18n.set('en');
    expect(t('queue.summary', { n: 3, time: '5:00' })).toContain('3');
    expect(t('does.not.exist')).toBe('does.not.exist');
  });

  it('live-switches to another language and ignores unknown ones', () => {
    i18n.set('ko');
    expect(t('nav.search')).toBe(koCat['nav.search']);
    i18n.set('zz-nope');
    expect(i18n.lang).toBe('ko'); // stayed put on an unknown code
    i18n.set('en');
    expect(t('nav.search')).toBe(enCat['nav.search']);
  });
});
